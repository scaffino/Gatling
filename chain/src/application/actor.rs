use super::{
    ingress::{Mailbox, Message},
    mempool::Mempool,
    Config,
};
use crate::{supervisor::Supervisor, utils::OneshotClosedFut};
use alto_types::{Block, MAX_BLOCK_TRANSACTIONS};
use commonware_consensus::{marshal, threshold_simplex::types::View};
use commonware_cryptography::{
    bls12381::primitives::variant::MinSig, ed25519::Batch, BatchVerifier, Committable, Digestible, Hasher, Sha256,
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
    // Custom Prometheus metrics
    finalized_blocks_counter: PromCounter<u64>,
    block_latency_ms_histogram: PromHistogram,
    // Track finalized blocks we've already recorded to avoid double-counting
    finalized_seen: HashSet<Vec<u8>>,
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
        (
            Self {
                context,
                hasher: Sha256::new(),
                mailbox,
                engine_id: config.engine_id,
                finalized_blocks_counter,
                block_latency_ms_histogram,
                finalized_seen: HashSet::new(),
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
        let genesis = Block::new(genesis_parent, 0, 0, Vec::new());
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
                    // Collect transactions from mempool
                    let mut transactions = Vec::new();
                    while transactions.len() < MAX_BLOCK_TRANSACTIONS {
                        let Some(tx) = mempool.next() else {
                            break;
                        };
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
                                    let block = Block::new(parent.digest(), parent.height+1, current, transactions);
                                    let digest = block.digest();
                                    {
                                        let mut built = built.lock().unwrap();
                                        *built = Some((view, block));
                                    }

                                    // Send the digest to the consensus
                                    let result = response.send(digest);
                                    info!(engine_id=%engine_id, view, ?digest, txs=tx_count, success=result.is_ok(), "proposed new block");
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
                Message::Finalized { block } => {
                    // In an application that maintains state, you would compute the state transition function here.
                    //
                    // After an unclean shutdown, it is possible that the application may be asked to process a block it has already seen (which it can simply ignore).
                    let engine_id = self.engine_id.clone();
                    // Record metrics for finalized block only once per unique block digest
                    let digest = block.digest();
                    if self.finalized_seen.insert(digest.to_vec()) {
                        // Use block.timestamp as proposal time and current epoch as finalization time
                        self.finalized_blocks_counter.inc();
                        let now_ms = self.context.current().epoch_millis();
                        let latency_ms = now_ms.saturating_sub(block.timestamp);
                        self.block_latency_ms_histogram.observe(latency_ms as f64);
                    }
                    info!(
                        engine_id=%engine_id,
                        height = block.height,
                        digest = ?block.commitment(),
                        "processed block"
                    );
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
