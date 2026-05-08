use super::{
    ingress::{Mailbox, Message},
    mempool::Mempool,
    Config,
};
use crate::{supervisor::Supervisor, utils::OneshotClosedFut};
use alto_types::{Block, MAX_BLOCK_TRANSACTIONS};
use commonware_codec::DecodeExt;
use commonware_consensus::{marshal, threshold_simplex::types::View};
use commonware_cryptography::{
    bls12381::primitives::variant::MinSig, ed25519::Batch, BatchVerifier, Digestible, Hasher, Sha256,
    sha256::Digest,
};
use commonware_macros::select;
use commonware_p2p::{Receiver, Recipients, Sender};
use commonware_runtime::{Clock, Handle, Metrics, Spawner};
use commonware_utils::SystemTimeExt;
use bytes::Bytes;
use futures::StreamExt;
use futures::{channel::{mpsc, oneshot}, future::try_join};
use futures::{future, future::Either};
use rand::{CryptoRng, Rng};
use std::collections::{HashMap, HashSet, BTreeMap};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tracing::{debug, info, warn};

/// AncestorFetcher is a message type for requesting and receiving missing ancestor blocks.
/// This is used to synchronize validators that have fallen behind or need to fetch ancestor chains.
#[derive(Clone, Debug, PartialEq)]
pub enum AncestorFetcher {
    /// Request for missing ancestor blocks (batched by digest)
    Request(Request),
    /// Response containing requested ancestor blocks (batched)
    Response(Response),
}

/// Request for ancestor blocks.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    /// Unique request ID to match requests with responses
    pub id: u64,
    /// List of block digests being requested
    pub digests: Vec<Digest>,
}

/// Response containing ancestor blocks.
#[derive(Clone, Debug, PartialEq)]
pub struct Response {
    /// Request ID this response corresponds to
    pub id: u64,
    /// List of blocks found (may be fewer than requested if some are missing)
    pub blocks: Vec<Block>,
}

impl Request {
    pub fn new(id: u64, digests: Vec<Digest>) -> Self {
        Self { id, digests }
    }
}

impl Response {
    pub fn new(id: u64, blocks: Vec<Block>) -> Self {
        Self { id, blocks }
    }
}

impl Request {
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&self.id.to_le_bytes());
        buf.extend_from_slice(&(self.digests.len() as u32).to_le_bytes());
        for digest in &self.digests {
            buf.extend_from_slice(digest.as_ref());
        }
        buf
    }
}

impl Response {
    pub fn encode(&self) -> Vec<u8> {
        use commonware_codec::Encode;
        let mut buf = Vec::new();
        buf.extend_from_slice(&self.id.to_le_bytes());
        buf.extend_from_slice(&(self.blocks.len() as u32).to_le_bytes());
        for block in &self.blocks {
            buf.extend_from_slice(&block.encode().to_vec());
        }
        buf
    }
}

impl AncestorFetcher {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            AncestorFetcher::Request(req) => {
                let mut buf = vec![0u8]; // 0 = Request variant
                buf.extend_from_slice(&req.encode());
                buf
            }
            AncestorFetcher::Response(resp) => {
                let mut buf = vec![1u8]; // 1 = Response variant
                buf.extend_from_slice(&resp.encode());
                buf
            }
        }
    }
}

impl Request {
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() < 12 {
            return Err("Invalid request encoding: too short".to_string());
        }
        let id = u64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3],
            bytes[4], bytes[5], bytes[6], bytes[7],
        ]);
        let count = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize;
        let mut digests = Vec::new();
        let mut offset = 12;
        for _ in 0..count {
            if offset + 32 > bytes.len() {
                return Err("Invalid request encoding: incomplete digest".to_string());
            }
            let digest_bytes = &bytes[offset..offset + 32];
            let digest = Digest::decode(digest_bytes)
                .map_err(|_| "Invalid digest encoding".to_string())?;
            digests.push(digest);
            offset += 32;
        }
        Ok(Self { id, digests })
    }
}

impl Response {
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() < 12 {
            return Err("Invalid response encoding: too short".to_string());
        }
        let id = u64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3],
            bytes[4], bytes[5], bytes[6], bytes[7],
        ]);
        let count = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize;
        let mut blocks = Vec::new();
        let mut offset = 12;
        for _ in 0..count {
            use commonware_codec::{DecodeExt, Encode};
            match Block::decode(&bytes[offset..]) {
                Ok(block) => {
                    let block_bytes = block.encode();
                    offset += block_bytes.len();
                    blocks.push(block);
                }
                Err(e) => return Err(format!("Invalid block encoding: {:?}", e)),
            }
        }
        Ok(Self { id, blocks })
    }
}

impl AncestorFetcher {
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.is_empty() {
            return Err("Invalid AncestorFetcher encoding: empty".to_string());
        }
        match bytes[0] {
            0 => {
                let req = Request::decode(&bytes[1..])?;
                Ok(AncestorFetcher::Request(req))
            }
            1 => {
                let resp = Response::decode(&bytes[1..])?;
                Ok(AncestorFetcher::Response(resp))
            }
            _ => Err("Invalid AncestorFetcher variant".to_string()),
        }
    }
}

// Metrics
use prometheus_client::metrics::counter::Counter as PromCounter;
use prometheus_client::metrics::histogram::Histogram as PromHistogram;
use prometheus_client::metrics::histogram::exponential_buckets;

/// Genesis message to use during initialization.
const GENESIS: &[u8] = b"commonware is neat";

/// Milliseconds in the future to allow for block timestamps.
const SYNCHRONY_BOUND: u64 = 500;

/// Application actor.
pub struct Actor<R: Rng + CryptoRng + Spawner + Metrics + Clock> {
    context: R,
    hasher: Sha256,
    mailbox: mpsc::Receiver<Message>,
    engine_id: String,
    validator_index: usize,
    // Custom Prometheus metrics
    finalized_blocks_counter: PromCounter<u64>,
    block_latency_ms_histogram: PromHistogram,
    // Track finalized blocks we've already recorded to avoid double-counting
    finalized_seen: Arc<Mutex<HashSet<Vec<u8>>>>,
    // Time offset within each second for proposal timing (0-999ms)
    proposal_offset_ms: u64,
    // Channel to send finalized blocks to the gatling thread (if enabled)
    gatling_tx: Option<std::sync::mpsc::Sender<crate::engine::GatlingEvent>>,
    // Per-instance channel to buffer finalized blocks before forwarding to gatling (if enabled)
    buffer_tx: Option<futures::channel::mpsc::UnboundedSender<Block>>,
    // Instance ID for this consensus engine (1-based, used for gatling ordering)
    gatling_instance_id: usize,
    // Shared view tracking across all instances
    instance_views: Arc<Vec<AtomicU64>>,
    // Lag threshold for adaptive timing
    lag_threshold: u64,
    // Total number of consensus instances (K)
    total_instances: usize,
    // Genesis timestamp in seconds
    genesis_timestamp_secs: u64,
    // Ancestor fetching configuration
    ancestor_fetch_concurrent: usize,
    ancestor_fetch_rate_per_peer: governor::Quota,
    // Participants for peer selection
    participants: Vec<commonware_cryptography::ed25519::PublicKey>,
    // Shared mempool for all consensus instances on this validator
    mempool: Arc<Mutex<Mempool>>,
}

impl<R: Rng + CryptoRng + Spawner + Metrics + Clock> Actor<R> {
    /// Compute absolute proposal time in milliseconds for given view and instance.
    /// tproposal = genesis_timestamp + (v + (k-1)/K) * delta_ibt_ms
    pub fn tproposal(genesis_ts_secs: u64, k_total: u64, k: u64, v: u64) -> u128 {
        let genesis_ms = (genesis_ts_secs as u128) * 1000;
        let view_ms = (v as u128) * 3000;
        let slot_ms = if k_total == 0 {
            0
        } else {
            ((k.saturating_sub(1)) as u128 * 3000) / (k_total as u128)
        };
        genesis_ms + view_ms + slot_ms
    }
    /// Create a new application actor.
    pub fn new(context: R, config: Config) -> (Self, Supervisor, Mailbox) {
        let (sender, mailbox) = mpsc::channel(config.mailbox_size);
        // Create metrics and register with runtime registry
        // Counter for finalized blocks
        let finalized_blocks_counter = PromCounter::<u64>::default();
        // Histogram for block proposal-to-finalization latency (ms)
        // Buckets roughly from 1ms up to ~32s
        let buckets = exponential_buckets(1.0, 2.0, 16);
        let block_latency_ms_histogram = PromHistogram::new(buckets);

        // Metric names are prefixed by engine id to distinguish multiple engines per validator
        let finalized_name = format!("{}{}", config.engine_id, "_finalized_blocks_total");
        let latency_name = format!("{}{}", config.engine_id, "_block_finalization_latency_ms");

        // Register the metrics so they appear on the /metrics endpoint
        context.register(&finalized_name, "Total number of finalized blocks", finalized_blocks_counter.clone());
        context.register(&latency_name, "Block proposal-to-finalization latency in milliseconds", block_latency_ms_histogram.clone());
        
        // Find this validator's index in the sorted participants list
        let validator_index = config.participants
            .iter()
            .position(|p| p == &config.public_key)
            .expect("Public key not found in participants");
        
        (
            Self {
                context,
                hasher: Sha256::new(),
                mailbox,
                engine_id: config.engine_id,
                validator_index,
                finalized_blocks_counter,
                block_latency_ms_histogram,
                finalized_seen: Arc::new(Mutex::new(HashSet::new())),
                proposal_offset_ms: config.proposal_offset_ms,
                gatling_tx: config.gatling_tx,
                buffer_tx: config.buffer_tx,
                gatling_instance_id: config.gatling_instance_id,
                instance_views: config.instance_views,
                lag_threshold: config.lag_threshold,
                total_instances: config.total_instances,
                genesis_timestamp_secs: config.genesis_timestamp_secs,
                ancestor_fetch_concurrent: config.ancestor_fetch_concurrent,
                ancestor_fetch_rate_per_peer: config.ancestor_fetch_rate_per_peer,
                participants: config.participants.clone(),
                mempool: config.shared_mempool,
            },
            Supervisor::new(config.polynomial, config.participants, config.share),
            Mailbox::new(sender),
        )
    }

    pub fn start<AS, AR>(
        mut self,
        marshal: marshal::Mailbox<MinSig, Block>,
        ancestor_network: (AS, AR),
    ) -> Handle<()>
    where
        AS: Sender<PublicKey = commonware_cryptography::ed25519::PublicKey> + Clone + Send + 'static,
        AR: Receiver<PublicKey = commonware_cryptography::ed25519::PublicKey> + Send + 'static,
    {
        self.context.spawn_ref()(self.run(marshal, ancestor_network))
    }

    /// Run the application actor.
    async fn run<AS, AR>(
        mut self,
        mut marshal: marshal::Mailbox<MinSig, Block>,
        ancestor_network: (AS, AR),
    ) where
        AS: Sender<PublicKey = commonware_cryptography::ed25519::PublicKey> + Clone + Send + 'static,
        AR: Receiver<PublicKey = commonware_cryptography::ed25519::PublicKey> + Send + 'static,
    {
        let (ancestor_sender, mut ancestor_receiver) = ancestor_network;
        // Compute genesis digest
        self.hasher.update(GENESIS);
        let genesis_parent = self.hasher.finalize();
        let genesis = Block::new(genesis_parent, 0, 0, 0, Vec::new());
        let genesis_digest = genesis.digest();
        let built: Option<(View, Block)> = None;
        let built = Arc::new(Mutex::new(built));
        
        // Shared pending ancestor requests (request_id -> HashMap<digest -> oneshot sender>)
        // Multiple finalize_ancestors tasks can add requests here, message handler fulfills them
        let pending_ancestor_requests: Arc<Mutex<HashMap<u64, HashMap<Digest, oneshot::Sender<Block>>>>> = 
            Arc::new(Mutex::new(HashMap::new()));
        
        // Request ID counter
        let request_id_counter: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));
        
        // Spawn task to handle incoming ancestor messages (both requests and responses)
        // This runs continuously throughout the actor's lifetime
        let pending_requests_handler = pending_ancestor_requests.clone();
        let mut marshal_for_handler = marshal.clone();
        let engine_id_handler = self.engine_id.clone();
        let mut ancestor_sender_handler = ancestor_sender.clone();
        self.context.with_label("ancestor_message_handler").spawn(move |context| async move {
            loop {
                match ancestor_receiver.recv().await {
                    Ok((peer, message_bytes)) => {
                        // Try to decode as AncestorFetcher message
                        match AncestorFetcher::decode(message_bytes.as_ref()) {
                            Ok(AncestorFetcher::Request(request)) => {
                                debug!("[{}] Received ancestor request {} from peer for {} digests", 
                                       engine_id_handler, request.id, request.digests.len());
                                
                                // Look up requested blocks from marshal local storage
                                let mut found_blocks = Vec::new();
                                for digest in &request.digests {
                                    // Use 15ms timeout to check if block is immediately available locally
                                    let subscribe_fut = marshal_for_handler.subscribe(None, *digest).await;
                                    let check_result = select!(
                                        result = subscribe_fut => {
                                            match result {
                                                Ok(block) => Some(block),
                                                Err(_) => None,
                                            }
                                        },
                                        _ = context.sleep(Duration::from_millis(15)) => {
                                            // Timeout - block not in local storage
                                            None
                                        }
                                    );
                                    
                                    if let Some(block) = check_result {
                                        found_blocks.push(block);
                                    }
                                }
                                
                                // Send response with found blocks
                                let found_count = found_blocks.len();
                                if !found_blocks.is_empty() {
                                    let response = AncestorFetcher::Response(Response::new(request.id, found_blocks));
                                    let response_bytes = Bytes::from(response.encode());
                                    if let Err(e) = ancestor_sender_handler.send(Recipients::One(peer), response_bytes, true).await {
                                        warn!("[{}] Failed to send ancestor response to peer: {:?}", 
                                              engine_id_handler, e);
                                    } else {
                                        debug!("[{}] Sent ancestor response {} with {} blocks to peer", 
                                               engine_id_handler, request.id, found_count);
                                    }
                                } else {
                                    debug!("[{}] Don't have any requested ancestors in local storage, ignoring request {}", 
                                           engine_id_handler, request.id);
                                }
                            }
                            Ok(AncestorFetcher::Response(response)) => {
                                debug!("[{}] Received ancestor response {} with {} blocks", 
                                       engine_id_handler, response.id, response.blocks.len());
                                
                                // Check if we have a pending request for this ID
                                let mut pending = pending_requests_handler.lock().unwrap();
                                if let Some(mut digest_map) = pending.remove(&response.id) {
                                    drop(pending); // Drop lock before sending
                                    
                                    // Collect blocks and their corresponding senders
                                    let mut to_send: Vec<(oneshot::Sender<Block>, Block)> = Vec::new();
                                    for block in response.blocks {
                                        let block_digest = block.digest();
                                        if let Some(response_tx) = digest_map.remove(&block_digest) {
                                            to_send.push((response_tx, block));
                                        }
                                    }
                                    
                                    // Send all blocks
                                    for (tx, block) in to_send {
                                        let _ = tx.send(block);
                                    }
                                }
                            }
                            Err(e) => {
                                // Try old format for backward compatibility during transition
                                // Try to decode as a block first (old response format)
                                match Block::decode(message_bytes.as_ref()) {
                                    Ok(block) => {
                                        let block_digest = block.digest();
                                        debug!("[{}] Received old format ancestor response for block {}", 
                                               engine_id_handler, block_digest);
                                        
                                        // Check if we have a pending request for this digest (old format)
                                        // This is a fallback for old format - we'd need a different structure
                                        // For now, just log and ignore
                                        warn!("[{}] Received old format block response, but new format expected", 
                                              engine_id_handler);
                                    }
                                    Err(_) => {
                                        warn!("[{}] Failed to decode ancestor message: {:?}", 
                                              engine_id_handler, e);
                                    }
                                }
                            }
                        }
                    }
                    Err(_) => {
                        // Receiver closed, exit loop
                        break;
                    }
                }
            }
        });
        
        while let Some(message) = self.mailbox.next().await {
            match message {
                Message::Genesis { response } => {
                    // Use the digest of the genesis message as the initial
                    // payload.
                    let _ = response.send(genesis_digest);
                }
                Message::Propose {
                    view,
                    parent,
                    mut response,
                } => {
                    // First pickup: Collect transactions from mempool immediately
                    let mut transactions = Vec::new();
                    {
                        let mut mempool_guard = self.mempool.lock().unwrap();
                        while transactions.len() < MAX_BLOCK_TRANSACTIONS {
                            if let Some(tx) = mempool_guard.next() {
                                transactions.push(tx);
                            } else {
                                break;
                            }
                        }
                    }
                    let initial_tx_count = transactions.len();

                    // Get the parent block
                    let parent_request = if parent.1 == genesis_digest {
                        Either::Left(future::ready(Ok(genesis.clone())))
                    } else {
                        Either::Right(marshal.subscribe(Some(parent.0), parent.1).await)
                    };

                    // Wait for the parent block to be available or the request to be cancelled in a separate task (to
                    // continue processing other messages)
                    let mempool_clone = self.mempool.clone();
                    let initial_tx_count_clone = initial_tx_count;
                    self.context.with_label("propose").spawn({
                        let built = built.clone();
                        let engine_id = self.engine_id.clone();
                        let total_instances = self.total_instances as u64;
                        let instance_number = self.gatling_instance_id as u64;
                        let genesis_ts_secs = self.genesis_timestamp_secs;
                        move |context| async move {
                            let response_closed = OneshotClosedFut::new(&mut response);
                            select! {
                                parent = parent_request => {
                                    // Get the parent block
                                    let parent = parent.unwrap();

                                    // Create a new block
                                    let mut current = context.current().epoch_millis();
                                    if current <= parent.timestamp {
                                        current = parent.timestamp + 1;
                                    }
                                    let block_height = parent.height + 1;
                                    let block = Block::new(parent.digest(), block_height, current, view as u64, transactions.clone());
                                    let digest = block.digest();
                                    
                                    // Log each transaction included in the block
                                    let validator_idx = self.validator_index;
                                    for tx in &transactions {
                                        let tx_id = tx.digest();
                                        debug!("[{}] Validator {} included transaction {:?} (timestamp: {}) in block {} (view {})", 
                                              engine_id, validator_idx, tx_id, tx.timestamp, block_height, view);
                                    }
                                    
                                    {
                                        let mut built = built.lock().unwrap();
                                        *built = Some((view, block));
                                    }

                                    // Timely proposal: wait until tproposal if in the future, else propose immediately upon entering view
                                    let target_ms = Self::tproposal(genesis_ts_secs, total_instances, instance_number, view as u64);
                                    let now_ms = context.current().epoch_millis() as u128;
                                    let is_catchup = now_ms >= target_ms;
                                    if !is_catchup {
                                        let wait = (target_ms - now_ms) as u64;
                                        context.sleep(Duration::from_millis(wait)).await;
                                    }

                                    // Second pickup: Collect any additional transactions that arrived during the wait period
                                    {
                                        let mut mempool_guard = mempool_clone.lock().unwrap();
                                        while transactions.len() < MAX_BLOCK_TRANSACTIONS {
                                            if let Some(tx) = mempool_guard.next() {
                                                transactions.push(tx);
                                            } else {
                                                break;
                                            }
                                        }
                                    }
                                    
                                    // Rebuild block with updated transactions if any new ones were added
                                    let final_tx_count = transactions.len();
                                    if final_tx_count != initial_tx_count_clone {
                                        // Rebuild block with updated transactions
                                        let block = Block::new(parent.digest(), block_height, current, view as u64, transactions.clone());
                                        let new_digest = block.digest();
                                        
                                        // Log newly added transactions
                                        let validator_idx = self.validator_index;
                                        for tx in &transactions[initial_tx_count_clone..] {
                                            let tx_id = tx.digest();
                                            debug!("[{}] Validator {} included transaction {:?} (timestamp: {}) in block {} (view {}) [second pickup]", 
                                                  engine_id, validator_idx, tx_id, tx.timestamp, block_height, view);
                                        }
                                        
                                        // Update stored block
                                        {
                                            let mut built = built.lock().unwrap();
                                            *built = Some((view, block));
                                        }
                                        
                                        // Send the updated digest to the consensus
                                        let _result = response.send(new_digest);
                                    } else {
                                        // No new transactions, send original digest
                                        let _result = response.send(digest);
                                    }
                                    let proposal_timestamp_ms = context.current().epoch_millis();
                                    let cast_time_utc = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(proposal_timestamp_ms as i64)
                                        .map(|dt| dt.format("%H:%M:%S.%3f UTC").to_string())
                                        .unwrap_or_else(|| "invalid-ts".to_string());
                                    let validator_idx = self.validator_index;
                                    let computed_time_utc = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(target_ms as i64)
                                        .map(|dt| dt.format("%H:%M:%S.%3f UTC").to_string())
                                        .unwrap_or_else(|| "invalid-ts".to_string());
                                    let catchup_msg = if is_catchup { " (catch up proposal)" } else { "" };
                                    let second_pickup_msg = if final_tx_count > initial_tx_count_clone {
                                        format!(" ({} from second pickup)", final_tx_count - initial_tx_count_clone)
                                    } else {
                                        String::new()
                                    };
                                    info!("[{}] Validator {} proposed block {} (view {}) with {} transactions at {} (computed {}){}{}", 
                                          engine_id, validator_idx, block_height, view, final_tx_count, cast_time_utc, computed_time_utc, catchup_msg, 
                                          second_pickup_msg);
                                },
                                _ = response_closed => {
                                    // The response was cancelled
                                    warn!(engine_id=%engine_id, view, "propose aborted");
                                }
                            }
                        }
                    });
                }
                Message::Broadcast { payload } => {
                    // Check if the last built is equal
                    let Some(built) = built.lock().unwrap().clone() else {
                        let engine_id = self.engine_id.clone();
                        warn!(engine_id=%engine_id, ?payload, "missing block to broadcast");
                        continue;
                    };

                    // Send the block to the syncer
                    let engine_id = self.engine_id.clone();
                    debug!(
                        engine_id=%engine_id,
                        ?payload,
                        view = built.0,
                        height = built.1.height,
                        "broadcast requested"
                    );
                    marshal.broadcast(built.1.clone()).await;
                }
                Message::Verify {
                    view,
                    parent,
                    payload,
                    mut response,
                } => {
                    // Get the parent and current block
                    let parent_request = if parent.1 == genesis_digest {
                        Either::Left(future::ready(Ok(genesis.clone())))
                    } else {
                        Either::Right(marshal.subscribe(Some(parent.0), parent.1).await)
                    };

                    // Wait for the blocks to be available or the request to be cancelled in a separate task (to
                    // continue processing other messages)
                    self.context.with_label("verify").spawn({
                        let mut marshal = marshal.clone();
                        let engine_id = self.engine_id.clone();
                        move |mut context| async move {
                            let requester =
                                try_join(parent_request, marshal.subscribe(None, payload).await);
                            let response_closed = OneshotClosedFut::new(&mut response);
                            select! {
                                result = requester => {
                                    // Unwrap the results
                                    let (parent, block) = result.unwrap();

                                    // Verify the block
                                    if block.height != parent.height + 1 {
                                        let _ = response.send(false);
                                        return;
                                    }
                                    if block.parent != parent.digest() {
                                        let _ = response.send(false);
                                        return;
                                    }
                                    if block.timestamp <= parent.timestamp {
                                        let _ = response.send(false);
                                        return;
                                    }
                                    let current = context.current().epoch_millis();
                                    if block.timestamp > current + SYNCHRONY_BOUND {
                                        let _ = response.send(false);
                                        return;
                                    }

                                    // Batch verify transaction signatures
                                    let mut batcher = Batch::new();
                                    for tx in &block.transactions {
                                        tx.verify_batch(&mut batcher);
                                    }
                                    if !batcher.verify(&mut context) {
                                        let _ = response.send(false);
                                        return;
                                    }

                                    // Persist the verified block
                                    marshal.verified(view, block).await;

                                    // Send the verification result to the consensus
                                    let _ = response.send(true);
                                },
                                _ = response_closed => {
                                    // The response was cancelled
                                    warn!(engine_id=%engine_id, view, "verify aborted");
                                }
                            }
                        }
                    });
                }
                Message::Finalized { view, block } => {
                    // In an application that maintains state, you would compute the state transition function here.
                    //
                    // After an unclean shutdown, it is possible that the application may be asked to process a block it has already seen (which it can simply ignore).
                    let engine_id = self.engine_id.clone();
                    let tx_count = block.transactions.len();
                    
                    // Step 1: Persist this finalized block to marshal to ensure it's available for ancestor finalization.
                    // Since this block is finalized by consensus, it's safe to persist even if we didn't verify it locally.
                    // This runs in a separate task so it doesn't block the main message processing.
                    {
                        let mut marshal_persist = marshal.clone();
                        let block_view = view;
                        let block_clone = block.clone();
                        let engine_id_persist = engine_id.clone();
                        
                        // Spawn task to persist finalized block to marshal
                        self.context.with_label("persist_finalized").spawn(move |_| async move {
                            // Persist the finalized block to marshal storage
                            // This is safe because consensus already verified it (threshold of validators)
                            // Calling verified() is idempotent - if already persisted, it's a no-op
                            marshal_persist.verified(block_view, block_clone.clone()).await;
                            debug!("[{}] Ensured finalized block {} (view {}) is persisted in marshal", 
                                  engine_id_persist, block_clone.height, block_view);
                        });
                    }
                    
                    // Step 2: Recursively fetch and finalize any missing ancestors in a separate task
                    // This runs concurrently with the persistence task above
                    {
                        let mut marshal = marshal.clone();
                        let engine_id_clone = engine_id.clone();
                        let validator_index = self.validator_index;
                        let buffer_tx = self.buffer_tx.clone();
                        let finalized_seen = self.finalized_seen.clone();
                        let finalized_blocks_counter = self.finalized_blocks_counter.clone();
                        let block_latency_ms_histogram = self.block_latency_ms_histogram.clone();
                        let start_parent = block.parent;
                        let genesis_digest_clone = genesis_digest.clone();
                        let block_clone_for_log = block.clone();
                        let mut ancestor_sender_clone = ancestor_sender.clone();
                        let pending_ancestor_requests_clone = pending_ancestor_requests.clone();
                        let request_id_counter_clone = request_id_counter.clone();
                        let participants_clone = self.participants.clone();
                        
                        self.context.with_label("finalize_ancestors").spawn(move |context| async move {
                            // Constants
                            const PEER_RESPONSE_TIMEOUT_MS: u64 = 2000; // Timeout per peer response attempt
                            const LOCAL_CHECK_TIMEOUT_MS: u64 = 500; // Marshal local storage check timeout — must cover archive restoration (~200ms per engine)
                            const MAX_TOTAL_FETCH_MS: u64 = 60_000; // Give up after 60s (~2× LEADER_TIMEOUT)
                            const INITIAL_RETRY_DELAY_MS: u64 = 200; // Initial backoff delay
                            const MAX_RETRY_DELAY_MS: u64 = 5_000; // Cap backoff at 5s
                            
                            debug!("[{}] Starting ancestor finalization task for block {} (view {}) with parent: {:?}", 
                                  engine_id_clone, block_clone_for_log.height, block_clone_for_log.view, start_parent);
                            
                            // Step 1: Collect all missing ancestor digests and check local storage
                            let mut found_blocks: HashMap<Digest, Block> = HashMap::new();
                            let fetch_start_ms = context.current().epoch_millis() as u64;

                            // Walk the ancestor chain iteratively, fetching one level at a time.
                            // Each pass: walk through already-found blocks to the deepest unknown,
                            // check local storage, then fetch from a peer if still missing.
                            // After each successful fetch we know the next parent digest and loop back.
                            'fetch_loop: loop {
                                // Walk from start_parent through already-found blocks to find the frontier.
                                let mut walk_cursor = start_parent;
                                let mut frontier: Option<Digest> = None;

                                loop {
                                    if walk_cursor == genesis_digest_clone { break; }
                                    if finalized_seen.lock().unwrap().contains(&walk_cursor.to_vec()) { break; }

                                    if let Some(known) = found_blocks.get(&walk_cursor) {
                                        walk_cursor = known.parent;
                                    } else {
                                        // Check local marshal storage before going to the network.
                                        let subscribe_fut = marshal.subscribe(None, walk_cursor).await;
                                        let local_result = select!(
                                            result = subscribe_fut => { result.ok() },
                                            _ = context.sleep(Duration::from_millis(LOCAL_CHECK_TIMEOUT_MS)) => { None },
                                        );
                                        match local_result {
                                            Some(block) => {
                                                let parent = block.parent;
                                                found_blocks.insert(walk_cursor, block);
                                                walk_cursor = parent;
                                            }
                                            None => {
                                                frontier = Some(walk_cursor);
                                                break;
                                            }
                                        }
                                    }
                                }

                                let digest_to_fetch = match frontier {
                                    None => break 'fetch_loop, // Fully walked to genesis / finalized_seen
                                    Some(d) => d,
                                };

                                // Global timeout guard
                                if (context.current().epoch_millis() as u64).saturating_sub(fetch_start_ms) > MAX_TOTAL_FETCH_MS {
                                    warn!("[{}] Timed out fetching ancestors after {}ms",
                                          engine_id_clone, MAX_TOTAL_FETCH_MS);
                                    break 'fetch_loop;
                                }

                                // Fetch the frontier block from peers with exponential backoff.
                                let available_peers: Vec<commonware_cryptography::ed25519::PublicKey> = participants_clone.clone();
                                if available_peers.is_empty() {
                                    warn!("[{}] No peers available for ancestor fetching", engine_id_clone);
                                    break 'fetch_loop;
                                }

                                debug!("[{}] Need to fetch ancestor {:?}", engine_id_clone, digest_to_fetch);

                                let mut peer_cycle_idx = 0usize;
                                let mut retry_delay_ms = INITIAL_RETRY_DELAY_MS;
                                let mut fetched = false;

                                while !fetched {
                                    if (context.current().epoch_millis() as u64).saturating_sub(fetch_start_ms) > MAX_TOTAL_FETCH_MS {
                                        warn!("[{}] Timed out fetching ancestors after {}ms",
                                              engine_id_clone, MAX_TOTAL_FETCH_MS);
                                        break;
                                    }

                                    let peer = available_peers[peer_cycle_idx % available_peers.len()].clone();
                                    peer_cycle_idx += 1;

                                    let request_id = request_id_counter_clone.fetch_add(1, Ordering::Relaxed);
                                    let request = Request::new(request_id, vec![digest_to_fetch]);
                                    let (resp_tx, resp_rx) = oneshot::channel::<Block>();

                                    {
                                        let mut pending = pending_ancestor_requests_clone.lock().unwrap();
                                        pending.insert(request_id, HashMap::from([(digest_to_fetch, resp_tx)]));
                                    }

                                    let request_msg = AncestorFetcher::Request(request);
                                    let request_bytes = Bytes::from(request_msg.encode());
                                    let send_result = ancestor_sender_clone.send(Recipients::One(peer.clone()), request_bytes, true).await;

                                    if let Err(e) = send_result {
                                        warn!("[{}] Failed to send ancestor request {} to peer: {:?}",
                                              engine_id_clone, request_id, e);
                                        {
                                            let mut pending = pending_ancestor_requests_clone.lock().unwrap();
                                            pending.remove(&request_id);
                                        }
                                        context.sleep(Duration::from_millis(INITIAL_RETRY_DELAY_MS)).await;
                                        continue;
                                    }

                                    debug!("[{}] Sent ancestor request {} for 1 digest to peer",
                                           engine_id_clone, request_id);

                                    let response = select!(
                                        result = resp_rx => { result.ok() },
                                        _ = context.sleep(Duration::from_millis(PEER_RESPONSE_TIMEOUT_MS)) => {
                                            debug!("[{}] Timeout waiting for ancestor response {} from peer",
                                                   engine_id_clone, request_id);
                                            None
                                        }
                                    );

                                    {
                                        let mut pending = pending_ancestor_requests_clone.lock().unwrap();
                                        pending.remove(&request_id);
                                    }

                                    if let Some(block) = response {
                                        if block.digest() == digest_to_fetch {
                                            debug!("[{}] Received ancestor {} (view {}) from peer",
                                                   engine_id_clone, block.height, block.view);
                                            marshal.verified(block.view, block.clone()).await;
                                            found_blocks.insert(digest_to_fetch, block);
                                            fetched = true;
                                            retry_delay_ms = INITIAL_RETRY_DELAY_MS;
                                        } else {
                                            warn!("[{}] Received ancestor block with mismatched digest from peer",
                                                  engine_id_clone);
                                        }
                                    } else {
                                        debug!("[{}] No response from peer, backing off {}ms before retry",
                                               engine_id_clone, retry_delay_ms);
                                        context.sleep(Duration::from_millis(retry_delay_ms)).await;
                                        retry_delay_ms = (retry_delay_ms * 2).min(MAX_RETRY_DELAY_MS);
                                    }
                                }

                                if !fetched {
                                    warn!("[{}] Failed to fetch ancestor {:?}; reconstruction will be partial",
                                          engine_id_clone, digest_to_fetch);
                                    break 'fetch_loop;
                                }
                                // Block fetched — loop back to walk one level deeper
                            }
                            
                            // Step 4: Reconstruct final ancestor chain from all found blocks
                            let mut final_ancestors: Vec<Block> = Vec::new();
                            let mut final_cursor = start_parent;
                            
                            while final_cursor != genesis_digest_clone {
                                if finalized_seen.lock().unwrap().contains(&final_cursor.to_vec()) {
                                    break;
                                }
                                
                                if let Some(block) = found_blocks.get(&final_cursor) {
                                    let block_clone = block.clone();
                                    final_ancestors.push(block_clone.clone());
                                    final_cursor = block_clone.parent;
                                } else {
                                    warn!("[{}] Missing block {} in final reconstruction", 
                                          engine_id_clone, final_cursor);
                                    break;
                                }
                            }
                            
                            // Reverse to get ascending order
                            final_ancestors.reverse();
                            
                            // Finalize ancestors in ascending height order
                            let ancestor_count = final_ancestors.len();
                            for anc in final_ancestors {
                                let anc_tx_count = anc.transactions.len();
                                
                                // Forward to per-instance buffer if enabled
                                if let Some(ref tx) = buffer_tx {
                                    let _ = tx.unbounded_send(anc.clone());
                                }
                                
                                // Record metrics only once per unique block digest
                                let anc_digest = anc.digest();
                                let mut seen = finalized_seen.lock().unwrap();
                                let is_new = seen.insert(anc_digest.to_vec());
                                drop(seen);
                                
                                if is_new {
                                    finalized_blocks_counter.inc();
                                    let now_ms = context.current().epoch_millis();
                                    let latency_ms = now_ms.saturating_sub(anc.timestamp);
                                    block_latency_ms_histogram.observe(latency_ms as f64);
                                }

                                // Log ancestor finalization (treat like real finalization)
                                debug!(
                                    "[{}] Validator {} finalized block {} (view {}) with {} transactions (ancestor chain)",
                                    engine_id_clone,
                                    validator_index,
                                    anc.height,
                                    anc.view,
                                    anc_tx_count
                                );
                            }
                            
                            if ancestor_count > 0 {
                                debug!("[{}] Completed ancestor finalization chain: {} ancestors finalized",
                                      engine_id_clone, ancestor_count);
                            } else {
                                debug!("[{}] No missing ancestors found to finalize", engine_id_clone);
                            }

                            // Fix 1: send the current block last so run_buffer always sees heights
                            // in ascending order (ancestors first, then the block that triggered them).
                            if let Some(ref tx) = buffer_tx {
                                let _ = tx.unbounded_send(block_clone_for_log);
                            }
                        });
                    }
                    
                    // Update shared view tracking for lag detection
                    // Convert 1-based instance ID to 0-based index
                    let instance_idx = (self.gatling_instance_id - 1) as usize;
                    if instance_idx < self.instance_views.len() {
                        let old_view = self.instance_views[instance_idx].load(Ordering::Relaxed);
                        // Only update if this is a newer view
                        if view > old_view {
                            self.instance_views[instance_idx].store(view, Ordering::Relaxed);
                            debug!("[{}] Instance {} advanced to view {}", engine_id, self.gatling_instance_id, view);
                        }
                    }
                    
                    // Log finalized transactions (only if gatling is disabled)
                    if self.gatling_tx.is_none() && tx_count > 0 {
                        // Log each finalized transaction
                        for tx in &block.transactions {
                            let tx_id = tx.digest();
                            debug!("[{}] Transaction {:?} (timestamp: {} ms) is now final in block {} (view {})", 
                                  engine_id, tx_id, tx.timestamp, block.height, block.view);
                        }
                    }
                    
                    // Record metrics for finalized block only once per unique block digest
                    let digest = block.digest();
                    if self.finalized_seen.lock().unwrap().insert(digest.to_vec()) {
                        // Use block.timestamp as proposal time and current epoch as finalization time
                        self.finalized_blocks_counter.inc();
                        let now_ms = self.context.current().epoch_millis();
                        let latency_ms = now_ms.saturating_sub(block.timestamp);
                        self.block_latency_ms_histogram.observe(latency_ms as f64);
                    }
                    debug!("[{}] Validator {} finalized block {} (view {}) with {} transactions",
                          engine_id, self.validator_index, block.height, block.view, tx_count);
                }
                Message::SubmitTransaction { transaction } => {
                    // Verify transaction signature before adding to mempool
                    if transaction.verify() {
                        self.mempool.lock().unwrap().add(transaction);
                        debug!("transaction added to mempool");
                    } else {
                        warn!("rejected invalid transaction");
                    }
                }
            }
        }
    }
}

/// Per-instance buffer task ensuring height-contiguous forwarding to gatling.
/// Receives blocks finalized locally (direct or ancestor), buffers out-of-order arrivals,
/// and only forwards a block when all lower heights have been forwarded for this instance.
pub async fn run_buffer(
    mut rx: mpsc::UnboundedReceiver<Block>,
    gatling: std::sync::mpsc::Sender<crate::engine::GatlingEvent>,
    instance_id: usize,
) {
    let mut next_expected_height: u64 = 1;
    let mut pending: BTreeMap<u64, Block> = BTreeMap::new();
    let mut seen: HashSet<Vec<u8>> = HashSet::new();

    while let Some(block) = rx.next().await {
        let d = block.digest();
        if !seen.insert(d.to_vec()) {
            continue;
        }
        let h = block.height;
        pending.entry(h).or_insert(block);

        loop {
            if let Some(b) = pending.remove(&next_expected_height) {
                let event = crate::engine::GatlingEvent {
                    instance_id,
                    view: b.view as u64,
                    block: b,
                };
                // Ignore send errors (receiver may be gone)
                let _ = gatling.send(event);
                next_expected_height = next_expected_height.saturating_add(1);
                continue;
            }
            break;
        }
    }
}
