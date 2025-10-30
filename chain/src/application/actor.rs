use super::{
    ingress::{Mailbox, Message},
    mempool::Mempool,
    Config,
};
use crate::{supervisor::Supervisor, utils::OneshotClosedFut};
use alto_types::{Block, MAX_BLOCK_TRANSACTIONS};
use commonware_consensus::{marshal, threshold_simplex::types::View};
use commonware_cryptography::{
    bls12381::primitives::variant::MinSig, ed25519::Batch, BatchVerifier, Digestible, Hasher, Sha256,
    sha256::Digest,
};
use commonware_macros::select;
use commonware_runtime::{Clock, Handle, Metrics, Spawner};
use commonware_utils::SystemTimeExt;
use futures::StreamExt;
use futures::{channel::mpsc, future::try_join};
use futures::{future, future::Either};
use rand::{CryptoRng, Rng};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};
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
    finalized_seen: HashSet<Vec<u8>>,
    // Track transactions included in blocks across all consensus instances (shared)
    included_transactions: Arc<Mutex<HashSet<Digest>>>,
    // Time offset within each second for proposal timing (0-999ms)
    proposal_offset_ms: u64,
    // Channel to send finalized blocks to the gatling thread (if enabled)
    gatling_tx: Option<futures::channel::mpsc::UnboundedSender<crate::engine::GatlingEvent>>,
    // Instance ID for this consensus engine (1-based, used for gatling ordering)
    gatling_instance_id: usize,
    // Shared view tracking across all instances
    instance_views: Arc<Vec<AtomicU64>>,
    // Lag threshold for adaptive timing
    lag_threshold: u64,
}

impl<R: Rng + CryptoRng + Spawner + Metrics + Clock> Actor<R> {
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
                finalized_seen: HashSet::new(),
                included_transactions: config.included_transactions,
                proposal_offset_ms: config.proposal_offset_ms,
                gatling_tx: config.gatling_tx,
                gatling_instance_id: config.gatling_instance_id,
                instance_views: config.instance_views,
                lag_threshold: config.lag_threshold,
            },
            Supervisor::new(config.polynomial, config.participants, config.share),
            Mailbox::new(sender),
        )
    }

    pub fn start(mut self, marshal: marshal::Mailbox<MinSig, Block>) -> Handle<()> {
        self.context.spawn_ref()(self.run(marshal))
    }

    /// Run the application actor.
    async fn run(mut self, mut marshal: marshal::Mailbox<MinSig, Block>) {
        // Compute genesis digest
        self.hasher.update(GENESIS);
        let genesis_parent = self.hasher.finalize();
        let genesis = Block::new(genesis_parent, 0, 0, 0, Vec::new());
        let genesis_digest = genesis.digest();
        let built: Option<(View, Block)> = None;
        let built = Arc::new(Mutex::new(built));
        
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
                    // Collect transactions from mempool, filtering out already-included ones
                    let mut transactions = Vec::new();
                    let included_txs = self.included_transactions.clone();
                    
                    while transactions.len() < MAX_BLOCK_TRANSACTIONS {
                        let Some(tx) = mempool.next() else {
                            break;
                        };
                        
                        // Skip if this transaction was already included in another consensus instance
                        let tx_digest = tx.digest();
                        {
                            let included = included_txs.lock().unwrap();
                            if included.contains(&tx_digest) {
                                continue;
                            }
                        }
                        
                        transactions.push(tx);
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
                        let proposal_offset_ms = self.proposal_offset_ms;
                        let instance_views = self.instance_views.clone();
                        let current_instance_id = self.gatling_instance_id;
                        let lag_threshold = self.lag_threshold;
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
                                    
                                    // Mark transactions as included globally
                                    {
                                        let mut included = included_txs.lock().unwrap();
                                        for tx in &transactions {
                                            included.insert(tx.digest());
                                        }
                                    }
                                    
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

                                    // Adaptive timing: Check if this instance is lagging behind others
                                    let current_view = view;
                                    let max_view = instance_views.iter()
                                        .map(|v| v.load(Ordering::Relaxed))
                                        .max()
                                        .unwrap_or(0);
                                    let lag = max_view.saturating_sub(current_view);
                                    
                                    // Track if this is a catch-up proposal
                                    let is_catchup = lag >= lag_threshold;
                                    
                                    // Skip wait if lagging by threshold or more
                                    if is_catchup {
                                        // Instance is catching up - skip wait, will be marked in proposal log
                                    } else {
                                        // Wait until the precise time (X.000s + offset) before sending the proposal
                                        // This ensures all proposals happen at exact second boundaries for regularity
                                        let current_ms = context.current().epoch_millis();
                                        let current_ms_in_second = current_ms % 1000;
                                        let target_ms_in_second = proposal_offset_ms;
                                        
                                        // Calculate how many milliseconds to wait
                                        let wait_ms = if current_ms_in_second <= target_ms_in_second {
                                            // Target is later in this second
                                            target_ms_in_second - current_ms_in_second
                                        } else {
                                            // Target is in the next second
                                            1000 - current_ms_in_second + target_ms_in_second
                                        };
                                        
                                        if wait_ms > 0 {
                                            context.sleep(std::time::Duration::from_millis(wait_ms)).await;
                                        }
                                    }

                                    // Send the digest to the consensus at the precise time
                                    let _result = response.send(digest);
                                    let proposal_timestamp = context.current().epoch_millis();
                                    let validator_idx = self.validator_index;
                                    let catchup_msg = if is_catchup { " (catch up proposal)" } else { "" };
                                    info!("[{}] Validator {} proposed block {} (view {}) with {} transactions at Unix timestamp {} ms{}", 
                                          engine_id, validator_idx, block_height, view, tx_count, proposal_timestamp, catchup_msg);
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
                        let included_txs = self.included_transactions.clone();
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

                                    // Mark transactions in this block as included globally
                                    {
                                        let mut included = included_txs.lock().unwrap();
                                        for tx in &block.transactions {
                                            included.insert(tx.digest());
                                        }
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

                    // Recursively finalize any missing ancestors before finalizing this block
                    // Determine chain of missing ancestors by walking parents until we hit an already finalized block
                    let mut missing_ancestors: Vec<Block> = Vec::new();
                    let mut cursor_digest = block.parent;
                    // Walk back through parents fetching by digest only
                    while cursor_digest != genesis_digest {
                        // Stop if we've already seen this ancestor finalized
                        if self.finalized_seen.contains(&cursor_digest.to_vec()) {
                            break;
                        }
                        // Fetch ancestor block
                        match marshal.subscribe(None, cursor_digest).await.await {
                            Ok(ancestor) => {
                                // If ancestor already counted, stop
                                if self.finalized_seen.contains(&ancestor.digest().to_vec()) {
                                    break;
                                }
                                missing_ancestors.push(ancestor.clone());
                                cursor_digest = ancestor.parent;
                            }
                            Err(_) => {
                                // Could not fetch ancestor; stop attempting to synthesize
                                break;
                            }
                        }
                    }
                    // Finalize ancestors in ascending height order
                    missing_ancestors.reverse();
                    for anc in missing_ancestors {
                        let anc_tx_count = anc.transactions.len();
                        // Send to gatling if enabled
                        if let Some(ref gatling_tx) = self.gatling_tx {
                            let event = crate::engine::GatlingEvent {
                                instance_id: self.gatling_instance_id,
                                view: anc.view as u64,
                                block: anc.clone(),
                            };
                            let _ = gatling_tx.unbounded_send(event);
                        }
                        // Record metrics only once per unique block digest
                        let anc_digest = anc.digest();
                        if self.finalized_seen.insert(anc_digest.to_vec()) {
                            // Use block.timestamp as proposal time and current epoch as finalization time
                            self.finalized_blocks_counter.inc();
                            let now_ms = self.context.current().epoch_millis();
                            let latency_ms = now_ms.saturating_sub(anc.timestamp);
                            self.block_latency_ms_histogram.observe(latency_ms as f64);
                        }
                        // Log synthetic finalization (treat like real)
                        info!(
                            "[{}] Validator {} finalized block {} (view {}) with {} transactions",
                            engine_id,
                            self.validator_index,
                            anc.height,
                            anc.view,
                            anc_tx_count
                        );
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
                    
                    // Send finalized block to gatling thread if enabled
                    if let Some(ref gatling_tx) = self.gatling_tx {
                        let event = crate::engine::GatlingEvent {
                            instance_id: self.gatling_instance_id,
                            view: block.view as u64,
                            block: block.clone(),
                        };
                        if let Err(e) = gatling_tx.unbounded_send(event) {
                            warn!(error=?e, "Failed to send finalized block to gatling thread");
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
                    if self.finalized_seen.insert(digest.to_vec()) {
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
