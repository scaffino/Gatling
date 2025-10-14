use alto_chain::{application, engine, indexer};
use alto_types::{Block, Transaction, NAMESPACE};
use commonware_consensus::{marshal, Viewable};
use commonware_cryptography::{
    bls12381::{
        dkg::ops,
        primitives::{poly, variant::MinSig},
    },
    ed25519::{PrivateKey, PublicKey},
    Digestible, PrivateKeyExt, Signer,
};
use commonware_macros::test_traced;
use commonware_p2p::simulated::{self, Link, Network, Oracle, Receiver, Sender};
use commonware_runtime::{
    deterministic::{self, Runner},
    Clock, Metrics, Runner as _, Spawner,
};
use commonware_utils::{quorum, SystemTimeExt};
use governor::Quota;
use std::{
    collections::{HashMap, HashSet},
    num::NonZeroU32,
    sync::{Arc, Mutex},
    time::Duration,
};
use tracing::info;

/// Limit the freezer table size to 1MB because the deterministic runtime stores
/// everything in RAM.
const FREEZER_TABLE_INITIAL_SIZE: u32 = 2u32.pow(14); // 1MB

/// Shared state to track finalized blocks
#[derive(Clone)]
struct BlockTracker {
    blocks: Arc<Mutex<Vec<Block>>>,
}

impl BlockTracker {
    fn new() -> Self {
        Self {
            blocks: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn add_block(&self, block: Block) {
        self.blocks.lock().unwrap().push(block);
    }

    fn get_blocks(&self) -> Vec<Block> {
        self.blocks.lock().unwrap().clone()
    }
}

/// Mock indexer that tracks blocks
#[derive(Clone)]
struct TrackingIndexer {
    tracker: BlockTracker,
    _identity: alto_types::Identity,
}

impl TrackingIndexer {
    fn new(_uri: &str, identity: alto_types::Identity, tracker: BlockTracker) -> Self {
        Self {
            tracker,
            _identity: identity,
        }
    }
}

impl indexer::Indexer for TrackingIndexer {
    type Error = std::io::Error;

    fn new(uri: &str, identity: alto_types::Identity) -> Self {
        Self::new(uri, identity, BlockTracker::new())
    }

    async fn seed_upload(&self, _seed: alto_types::Seed) -> Result<(), Self::Error> {
        Ok(())
    }

    async fn notarized_upload(&self, notarized: alto_types::Notarized) -> Result<(), Self::Error> {
        self.tracker.add_block(notarized.block.clone());
        Ok(())
    }

    async fn finalized_upload(&self, finalized: alto_types::Finalized) -> Result<(), Self::Error> {
        self.tracker.add_block(finalized.block.clone());
        Ok(())
    }
}

/// Registers all validators using the oracle.
async fn register_validators(
    oracle: &mut Oracle<PublicKey>,
    validators: &[PublicKey],
) -> HashMap<
    PublicKey,
    (
        (Sender<PublicKey>, Receiver<PublicKey>),
        (Sender<PublicKey>, Receiver<PublicKey>),
        (Sender<PublicKey>, Receiver<PublicKey>),
        (Sender<PublicKey>, Receiver<PublicKey>),
        (Sender<PublicKey>, Receiver<PublicKey>),
    ),
> {
    let mut registrations = HashMap::new();
    for validator in validators.iter() {
        let (pending_sender, pending_receiver) = oracle.register(validator.clone(), 0).await.unwrap();
        let (recovered_sender, recovered_receiver) =
            oracle.register(validator.clone(), 1).await.unwrap();
        let (resolver_sender, resolver_receiver) =
            oracle.register(validator.clone(), 2).await.unwrap();
        let (broadcast_sender, broadcast_receiver) =
            oracle.register(validator.clone(), 3).await.unwrap();
        let (backfill_sender, backfill_receiver) =
            oracle.register(validator.clone(), 4).await.unwrap();
        registrations.insert(
            validator.clone(),
            (
                (pending_sender, pending_receiver),
                (recovered_sender, recovered_receiver),
                (resolver_sender, resolver_receiver),
                (broadcast_sender, broadcast_receiver),
                (backfill_sender, backfill_receiver),
            ),
        );
    }
    registrations
}

/// Links validators using the oracle.
async fn link_validators(
    oracle: &mut Oracle<PublicKey>,
    validators: &[PublicKey],
    link: Link,
) {
    for v1 in validators.iter() {
        for v2 in validators.iter() {
            if v2 == v1 {
                continue;
            }
            oracle
                .add_link(v1.clone(), v2.clone(), link.clone())
                .await
                .unwrap();
        }
    }
}

#[test_traced("INFO")] //To see the DEBUG logs, change #[test_traced("INFO")] into #[test_traced("DEBUG")] 
fn test_transaction_flow() {
    // Create context
    let n = 4;
    let threshold = quorum(n);
    let executor = Runner::timed(Duration::from_secs(30));
    
    executor.start(|mut context| async move {
        info!("Starting transaction flow test with {} validators", n);

        // Create simulated network
        let (network, mut oracle) = Network::new(
            context.with_label("network"),
            simulated::Config {
                max_size: 1024 * 1024,
            },
        );
        network.start();

        // Register participants
        let mut signers = Vec::new();
        let mut validators = Vec::new();
        for i in 0..n {
            let signer = PrivateKey::from_seed(i as u64);
            let pk = signer.public_key();
            signers.push(signer);
            validators.push(pk);
        }
        validators.sort();
        signers.sort_by_key(|s| s.public_key());
        let mut registrations = register_validators(&mut oracle, &validators).await;

        // Link all validators
        let link = Link {
            latency: Duration::from_millis(10),
            jitter: Duration::from_millis(1),
            success_rate: 1.0,
        };
        link_validators(&mut oracle, &validators, link).await;

        // Derive threshold
        let (polynomial, shares) =
            ops::generate_shares::<_, MinSig>(&mut context, None, n, threshold);
        let identity = *poly::public::<MinSig>(&polynomial);

        // Create block tracker
        let tracker = BlockTracker::new();

        // Create shared state for tracking included transactions
        let included_transactions = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));

        // Store engines with their mailboxes
        let mut engines = Vec::new();
        let mut mailboxes = Vec::new();

        // Create instances
        for (idx, signer) in signers.iter().enumerate() {
            let public_key = signer.public_key();
            let uid = format!("validator-{public_key}");
            
            let config = engine::Config {
                blocker: oracle.control(public_key.clone()),
                partition_prefix: uid.clone(),
                namespace: NAMESPACE.to_vec(),
                blocks_freezer_table_initial_size: FREEZER_TABLE_INITIAL_SIZE,
                finalized_freezer_table_initial_size: FREEZER_TABLE_INITIAL_SIZE,
                signer: signer.clone(),
                polynomial: polynomial.clone(),
                share: shares[idx].clone(),
                participants: validators.clone(),
                mailbox_size: 1024,
                deque_size: 10,
                backfill_quota: Quota::per_second(NonZeroU32::new(10).unwrap()),
                leader_timeout: Duration::from_secs(1),
                notarization_timeout: Duration::from_secs(2),
                nullify_retry: Duration::from_secs(10),
                fetch_timeout: Duration::from_secs(1),
                activity_timeout: 10,
                skip_timeout: 5,
                max_fetch_count: 10,
                max_fetch_size: 1024 * 512,
                fetch_concurrent: 10,
                fetch_rate_per_peer: Quota::per_second(NonZeroU32::new(10).unwrap()),
                indexer: Some(TrackingIndexer::new("", identity, tracker.clone())),
                included_transactions: included_transactions.clone(),
            };
            
            let engine = engine::Engine::new(context.with_label(&uid), config).await;
            
            // Get a clone of the mailbox before starting the engine
            let mailbox = engine.application_mailbox().clone();
            mailboxes.push(mailbox);
            
            // Get networking
            let (pending, recovered, resolver, broadcast, backfill) =
                registrations.remove(&public_key).unwrap();
            
            // Start engine
            engine.start(pending, recovered, resolver, broadcast, backfill);
            engines.push(());
        }

        info!("All validators started, waiting for initial blocks");

        // Wait for some blocks to be produced
        let mut initial_height = 0;
        loop {
            context.sleep(Duration::from_secs(1)).await;
            let blocks = tracker.get_blocks();
            if !blocks.is_empty() {
                initial_height = blocks.iter().map(|b| b.height).max().unwrap();
                if initial_height >= 5 {
                    info!(height = initial_height, "Initial blocks produced");
                    break;
                }
            }
        }

        info!("Creating and submitting transactions from ALL validators");

        // Record creation timestamps
        let mut tx_creation_times = HashMap::new();
        let mut all_tx_digests = Vec::new();

        // Each validator creates transactions to other validators
        let submission_start = context.current().epoch_millis();
        
        for (validator_idx, mailbox) in mailboxes.iter_mut().enumerate() {
            // Each validator sends to the next validator (round-robin)
            let sender = &signers[validator_idx];
            let receiver_idx = (validator_idx + 1) % n as usize;
            let receiver = signers[receiver_idx].public_key();
            
            // Create 3 transactions per validator with different amounts
            for amount in [100, 200, 300] {
                let created_at = context.current().epoch_millis();
                let tx = Transaction::sign(sender, receiver.clone(), amount);
                let tx_digest = tx.digest();
                
                tx_creation_times.insert(tx_digest, created_at);
                all_tx_digests.push(tx_digest);
                
                // Submit transaction
                mailbox.submit_transaction(tx).await
                    .expect(&format!("Failed to submit tx from validator {}", validator_idx));
                
                info!("Validator {} created and submitted TX (amount: {}, digest: {:?})", 
                      validator_idx, amount, tx_digest);
            }
        }
        
        let submission_end = context.current().epoch_millis();
        let total_txs = all_tx_digests.len();

        info!("Total transactions created: {}", total_txs);
        info!("Total submission took {} ms", submission_end - submission_start);
        info!("Waiting for all transactions to be included in blocks...");

        // Wait for transactions to be included in blocks
        let mut found_txs = HashSet::new();
        let mut tx_finalization_times = HashMap::new();
        let mut block_finalization_times = HashMap::new();
        let mut max_height = initial_height;

        for iteration in 0..75 {  // Increased iterations for finer checking
            context.sleep(Duration::from_millis(40)).await;  // Check every 40ms
            
            let blocks = tracker.get_blocks();
            let current_time = context.current().epoch_millis();
            
            // Track when we first see each block height (finalization time)
            for block in blocks.iter() {
                if block.height <= initial_height {
                    continue;
                }
                
                // Record when we first see this block finalized
                block_finalization_times.entry(block.height)
                    .or_insert(current_time);
                
                max_height = max_height.max(block.height);
                
                for tx in &block.transactions {
                    let digest = tx.digest();
                    
                    // Only process if this is one of our transactions and we haven't seen it yet
                    if tx_creation_times.contains_key(&digest) && !found_txs.contains(&digest) {
                        found_txs.insert(digest);
                        
                        // Use the block's finalization time, not current iteration time
                        let finalized_at = block_finalization_times.get(&block.height).unwrap();
                        tx_finalization_times.insert(digest, *finalized_at);
                        
                        if let Some(created_at) = tx_creation_times.get(&digest) {
                            let latency_ms = finalized_at - created_at;
                            info!("TX CONFIRMED: digest={:?}, height={}, latency={} ms", 
                                  digest, block.height, latency_ms);
                        }
                    }
                }
            }
            
            if found_txs.len() == total_txs {
                info!("All {} transactions found in blocks!", total_txs);
                break;
            }
            
            if iteration == 74 {
                info!("Warning: Not all transactions found after 3 seconds. Found {}/{}", 
                      found_txs.len(), total_txs);
            }
        }

        // Verify all transactions were included
        assert_eq!(found_txs.len(), total_txs, 
                   "Not all transactions found. Expected {}, found {}", total_txs, found_txs.len());
        
        // Calculate summary statistics for all confirmed transactions
        let mut latencies = Vec::new();
        
        for (digest, created_at) in tx_creation_times.iter() {
            if let Some(finalized_at) = tx_finalization_times.get(digest) {
                let latency = finalized_at - created_at;
                latencies.push(latency);
            }
        }
        
        if !latencies.is_empty() {
            let avg_latency = latencies.iter().sum::<u64>() / latencies.len() as u64;
            let min_latency = latencies.iter().min().unwrap();
            let max_latency = latencies.iter().max().unwrap();
            
            info!("");
            info!("========================================");
            info!("=== TRANSACTION CONFIRMATION METRICS ===");
            info!("Total validators: {}", n);
            info!("Transactions per validator: 3");
            info!("Total transactions created: {}", total_txs);
            info!("Transactions confirmed: {}", latencies.len());
            info!("Average confirmation latency: {} ms", avg_latency);
            info!("Min confirmation latency: {} ms", min_latency);
            info!("Max confirmation latency: {} ms", max_latency);
            info!("========================================");
            info!("");
        }
        
        info!("Test completed successfully! Height reached: {}", max_height);
    });
}

