use super::{
    ingress::{Mailbox, Message},
    mempool::Mempool,
    Config,
};
use crate::{supervisor::Supervisor, utils::OneshotClosedFut};
use alto_types::{Block, MAX_BLOCK_TRANSACTIONS};
use commonware_codec::{DecodeExt, Encode};
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
    gatling_tx: Option<futures::channel::mpsc::UnboundedSender<crate::engine::GatlingEvent>>,
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
}

impl<R: Rng + CryptoRng + Spawner + Metrics + Clock> Actor<R> {
    /// Compute absolute proposal time in milliseconds for given view and instance.
    /// tproposal = genesis_timestamp + v + (k-1)/K
    pub fn tproposal(genesis_ts_secs: u64, k_total: u64, k: u64, v: u64) -> u128 {
        let base_ms = (genesis_ts_secs + v) as u128 * 3000;
        let frac_ms = ((k.saturating_sub(1)) as u128 * 3000) / (k_total as u128);
        base_ms + frac_ms
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
        
        // Shared pending ancestor requests (digest -> oneshot sender for response)
        // Multiple finalize_ancestors tasks can add requests here, message handler fulfills them
        let pending_ancestor_requests: Arc<Mutex<HashMap<Digest, oneshot::Sender<Block>>>> = 
            Arc::new(Mutex::new(HashMap::new()));
        
        // Spawn task to handle incoming ancestor messages (both requests and responses)
        // This runs continuously throughout the actor's lifetime
        let pending_requests_handler = pending_ancestor_requests.clone();
        let mut marshal_for_handler = marshal.clone();
        let engine_id_handler = self.engine_id.clone();
        let mut ancestor_sender_handler = ancestor_sender.clone();
        self.context.with_label("ancestor_message_handler").spawn(move |_| async move {
            loop {
                match ancestor_receiver.recv().await {
                    Ok((_peer, message_bytes)) => {
                        // Try to decode as a block first (response)
                        match Block::decode(message_bytes.as_ref()) {
                            Ok(block) => {
                                // This is a block response
                                let block_digest = block.digest();
                                debug!("[{}] Received ancestor response for block {}", 
                                       engine_id_handler, block_digest);
                                
                                // Check if we have a pending request for this digest
                                let mut pending = pending_requests_handler.lock().unwrap();
                                if let Some(response_tx) = pending.remove(&block_digest) {
                                    drop(pending); // Drop lock before sending
                                    // Send the block to the waiting task
                                    let _ = response_tx.send(block);
                                }
                            }
                            Err(_) => {
                                // Not a block, try decoding as a digest (request)
                                match Digest::decode(message_bytes.as_ref()) {
                                    Ok(requested_digest) => {
                                        debug!("[{}] Received ancestor request from peer for digest: {:?}", 
                                               engine_id_handler, requested_digest);
                                        
                                        // Try to get the block from marshal LOCAL STORAGE ONLY (no backfill)
                                        // Use very short timeout to check if block is immediately available locally
                                        let subscribe_fut = marshal_for_handler.subscribe(None, requested_digest).await;
                                        
                                        // Note: We can't use context.sleep here since we don't have context in this handler
                                        // Instead, we'll let marshal.subscribe return quickly if not in local storage
                                        // The 10ms timeout check happens via a tokio sleep in a separate task if needed
                                        // For now, we rely on marshal.subscribe's internal timeout behavior
                                        match subscribe_fut.await {
                                            Ok(block) => {
                                                // We have the block in local storage - send it to all peers (they'll filter by digest)
                                                let block_bytes = Bytes::from(block.encode().to_vec());
                                                if let Err(e) = ancestor_sender_handler.send(Recipients::All, block_bytes, true).await {
                                                    warn!("[{}] Failed to send ancestor block to peer: {:?}", 
                                                          engine_id_handler, e);
                                                } else {
                                                    debug!("[{}] Sent ancestor block {} to peers", 
                                                           engine_id_handler, requested_digest);
                                                }
                                            }
                                            Err(_) => {
                                                // We don't have the block in local storage - ignore the request
                                                debug!("[{}] Don't have requested ancestor {} in local storage, ignoring request", 
                                                       engine_id_handler, requested_digest);
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        warn!("[{}] Failed to decode ancestor message (neither block nor digest): {:?}", 
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
        
        // Initialize mempool
        let mut mempool = Mempool::new(self.context.with_label("mempool"));
        
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
                    // Collect transactions from mempool
                    let mut transactions = Vec::new();
                    while transactions.len() < MAX_BLOCK_TRANSACTIONS {
                        if let Some(tx) = mempool.next() {
                            transactions.push(tx);
                        } else {
                            break;
                        }
                    }
                    let tx_count = transactions.len();

                    // Get the parent block
                    let parent_request = if parent.1 == genesis_digest {
                        Either::Left(future::ready(Ok(genesis.clone())))
                    } else {
                        Either::Right(marshal.subscribe(Some(parent.0), parent.1).await)
                    };

                    // Wait for the parent block to be available or the request to be cancelled in a separate task (to
                    // continue processing other messages)
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
                                        info!("[{}] Validator {} included transaction {:?} (timestamp: {}) in block {} (view {})", 
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

                                    // Send the digest to the consensus (no further delay)
                                    let _result = response.send(digest);
                                    let proposal_timestamp_ms = context.current().epoch_millis();
                                    let cast_time_utc = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(proposal_timestamp_ms as i64)
                                        .map(|dt| dt.format("%H:%M:%S.%3f UTC").to_string())
                                        .unwrap_or_else(|| "invalid-ts".to_string());
                                    let validator_idx = self.validator_index;
                                    let computed_time_utc = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(target_ms as i64)
                                        .map(|dt| dt.format("%H:%M:%S.%3f UTC").to_string())
                                        .unwrap_or_else(|| "invalid-ts".to_string());
                                    let catchup_msg = if is_catchup { " (catch up proposal)" } else { "" };
                                    info!("[{}] Validator {} proposed block {} (view {}) with {} transactions at {} (computed {}){}", 
                                          engine_id, validator_idx, block_height, view, tx_count, cast_time_utc, computed_time_utc, catchup_msg);
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
                        
                        self.context.with_label("finalize_ancestors").spawn(move |context| async move {
                            // Constants for retry logic
                            const MAX_RETRIES: usize = 5;
                            const INITIAL_RETRY_DELAY_MS: u64 = 100;
                            const MAX_RETRY_DELAY_MS: u64 = 1000;
                            
                            debug!("[{}] Starting ancestor finalization task for block {} (view {}) with parent: {:?}", 
                                  engine_id_clone, block_clone_for_log.height, block_clone_for_log.view, start_parent);
                            
                            // Determine chain of missing ancestors by walking parents until we hit an already finalized block
                            let mut missing_ancestors: Vec<Block> = Vec::new();
                            let mut cursor_digest = start_parent;
                            
                            while cursor_digest != genesis_digest_clone {
                                // Stop if we've already seen this ancestor finalized
                                if finalized_seen.lock().unwrap().contains(&cursor_digest.to_vec()) {
                                    debug!("[{}] Ancestor {} already finalized, stopping ancestor chain", 
                                           engine_id_clone, cursor_digest);
                                    break;
                                }
                                
                                // Fetch ancestor block with retry logic
                                let mut ancestor_opt: Option<Block> = None;
                                let mut retry_count = 0;
                                
                                debug!("[{}] Attempting to fetch ancestor {}", engine_id_clone, cursor_digest);
                                
                                // Try to fetch ancestor with exponential backoff retries
                                while ancestor_opt.is_none() && retry_count <= MAX_RETRIES {
                                    // First, try to get from marshal LOCAL STORAGE ONLY (no backfill)
                                    // Use very short timeout to check if block is immediately available locally
                                    // If timeout triggers quickly, block is not in local storage, skip to peer request
                                    let subscribe_fut = marshal.subscribe(None, cursor_digest).await;
                                    
                                    let local_check_result = select!(
                                        result = subscribe_fut => {
                                            match result {
                                                Ok(ancestor) => Some(ancestor),
                                                Err(_) => None,
                                            }
                                        },
                                        _ = context.sleep(Duration::from_millis(10)) => {
                                            // Timeout quickly - block not in local storage, skip backfill
                                            None
                                        }
                                    );
                                    
                                    match local_check_result {
                                        Some(ancestor) => {
                                            // Successfully fetched ancestor from marshal local storage
                                            ancestor_opt = Some(ancestor);
                                            if let Some(ref ancestor_ref) = ancestor_opt {
                                                debug!(
                                                    "[{}] Successfully fetched ancestor {} (view {}) from marshal local storage",
                                                    engine_id_clone, ancestor_ref.height, ancestor_ref.view
                                                );
                                            }
                                            break;
                                        }
                                        None => {
                                            // Marshal doesn't have it in local storage - try requesting from peers via ancestor channel
                                            debug!("[{}] Ancestor {} not in marshal, requesting from peers", 
                                                  engine_id_clone, cursor_digest);
                                            
                                            // Create oneshot channel for response
                                            let (response_tx, response_rx) = oneshot::channel();
                                            {
                                                let mut pending = pending_ancestor_requests_clone.lock().unwrap();
                                                pending.insert(cursor_digest, response_tx);
                                            }
                                            
                                            // Send request to all peers
                                            let request_bytes = Bytes::from(cursor_digest.encode().to_vec());
                                            let send_result = ancestor_sender_clone.send(Recipients::All, request_bytes, true).await;
                                            
                                            if send_result.is_err() {
                                                warn!("[{}] Failed to send ancestor request to peers: {:?}", 
                                                      engine_id_clone, send_result.err());
                                                // Remove from pending
                                                let mut pending = pending_ancestor_requests_clone.lock().unwrap();
                                                pending.remove(&cursor_digest);
                                            } else {
                                                // Wait for response (no timeout)
                                                let response_result = response_rx.await.ok();
                                                
                                                // Remove from pending (drop lock before processing result)
                                                {
                                                    let mut pending = pending_ancestor_requests_clone.lock().unwrap();
                                                    pending.remove(&cursor_digest);
                                                }
                                                
                                                match response_result {
                                                    Some(block) => {
                                                        // Verify digest matches
                                                        if block.digest() == cursor_digest {
                                                            debug!("[{}] Received ancestor {} (view {}) from peer, verifying digest", 
                                                                  engine_id_clone, block.height, block.view);
                                                            // Store in marshal
                                                            marshal.verified(block.view, block.clone()).await;
                                                            ancestor_opt = Some(block);
                                                            break;
                                                        } else {
                                                            warn!("[{}] Received ancestor block with mismatched digest", 
                                                                  engine_id_clone);
                                                        }
                                                    }
                                                    None => {
                                                        // No response from peers (channel closed or cancelled)
                                                        debug!("[{}] No response received for ancestor {} from peers", 
                                                               engine_id_clone, cursor_digest);
                                                    }
                                                }
                                            }
                                            
                                            // Failed to fetch from peers - retry with exponential backoff
                                            if ancestor_opt.is_none() {
                                            if retry_count < MAX_RETRIES {
                                                let delay_ms = (INITIAL_RETRY_DELAY_MS * (1 << retry_count))
                                                    .min(MAX_RETRY_DELAY_MS);
                                                debug!("[{}] Retry {} for ancestor {} (waiting {}ms)", 
                                                      engine_id_clone, retry_count + 1, cursor_digest, delay_ms);
                                                context.sleep(Duration::from_millis(delay_ms)).await;
                                                retry_count += 1;
                                            } else {
                                                // Max retries reached - give up on this ancestor
                                                debug!("[{}] Could not fetch ancestor {} after {} retries - stopping ancestor chain", 
                                                      engine_id_clone, cursor_digest, MAX_RETRIES);
                                                break;
                                                }
                                            }
                                        }
                                    }
                                }
                                
                                // Process ancestor if found
                                if let Some(ancestor) = ancestor_opt {
                                    // Persist ancestor to marshal so it's available for its own ancestors
                                    // This is safe because if it exists in marshal (via subscribe), it's valid
                                    // We persist it to ensure it's definitely there for future lookups
                                    let anc_view = ancestor.view;
                                    let anc_clone = ancestor.clone();
                                    marshal.verified(anc_view, anc_clone.clone()).await;
                                    
                                    // If ancestor already counted, stop
                                    if finalized_seen.lock().unwrap().contains(&ancestor.digest().to_vec()) {
                                        break;
                                    }
                                    
                                    // Continue walking up the chain
                                    cursor_digest = ancestor.parent;
                                    missing_ancestors.push(ancestor);
                                } else {
                                    // Failed to fetch after retries - stop walking the chain
                                    break;
                                }
                            }
                            

                            // Finalize ancestors in ascending height order
                            let ancestor_count = missing_ancestors.len();
                            missing_ancestors.reverse();
                            for anc in missing_ancestors {
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
                    
                    // Forward finalized block to per-instance buffer if enabled
                    if let Some(ref tx) = self.buffer_tx {
                        if let Err(e) = tx.unbounded_send(block.clone()) {
                            warn!(error=?e, "Failed to send finalized block to buffer");
                        }
                    }
                    
                    // Log finalized transactions (only if gatling is disabled)
                    if self.gatling_tx.is_none() && tx_count > 0 {
                        // Log each finalized transaction
                        for tx in &block.transactions {
                            let tx_id = tx.digest();
                            info!("[{}] Transaction {:?} (timestamp: {} ms) is now final in block {} (view {})", 
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
                    info!("[{}] Validator {} finalized block {} (view {}) with {} transactions",
                          engine_id, self.validator_index, block.height, block.view, tx_count);
                }
                Message::SubmitTransaction { transaction } => {
                    // Verify transaction signature before adding to mempool
                    if transaction.verify() {
                        mempool.add(transaction);
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
    gatling: futures::channel::mpsc::UnboundedSender<crate::engine::GatlingEvent>,
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
                let _ = gatling.unbounded_send(event);
                next_expected_height = next_expected_height.saturating_add(1);
                continue;
            }
            break;
        }
    }
}
