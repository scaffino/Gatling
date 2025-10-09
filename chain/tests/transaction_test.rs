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
use commonware_utils::quorum;
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

        // Create transaction signers (not validators)
        let alice = PrivateKey::from_seed(100);
        let bob = PrivateKey::from_seed(101);
        let charlie = PrivateKey::from_seed(102);

        info!("Creating and submitting transactions");

        // Create transactions
        let tx1 = Transaction::sign(&alice, bob.public_key(), 100);
        let tx2 = Transaction::sign(&bob, charlie.public_key(), 50);
        let tx3 = Transaction::sign(&charlie, alice.public_key(), 25);

        let tx1_digest = tx1.digest();
        let tx2_digest = tx2.digest();
        let tx3_digest = tx3.digest();

        info!("Transaction digests: tx1={:?}, tx2={:?}, tx3={:?}", tx1_digest, tx2_digest, tx3_digest);

        // Submit transactions to first validator
        let mut mailbox = mailboxes[0].clone();
        mailbox.submit_transaction(tx1.clone()).await.expect("Failed to submit tx1");
        mailbox.submit_transaction(tx2.clone()).await.expect("Failed to submit tx2");
        mailbox.submit_transaction(tx3.clone()).await.expect("Failed to submit tx3");

        info!("Transactions submitted to validator, waiting for inclusion");

        // Wait for transactions to be included in blocks
        let mut found_tx1 = false;
        let mut found_tx2 = false;
        let mut found_tx3 = false;
        let mut max_height = initial_height;
        let mut seen_heights = HashSet::new();

        for _ in 0..30 {
            context.sleep(Duration::from_secs(1)).await;
            
            let blocks = tracker.get_blocks();
            
            // Check new blocks for our transactions (deduplicate by height to avoid processing same block multiple times)
            for block in blocks.iter() {
                if block.height <= initial_height || seen_heights.contains(&block.height) {
                    continue;
                }
                
                seen_heights.insert(block.height);
                max_height = max_height.max(block.height);
                
                for tx in &block.transactions {
                    let digest = tx.digest();
                    if digest == tx1_digest {
                        found_tx1 = true;
                        info!("Found tx1 in block at height {}", block.height);
                    }
                    if digest == tx2_digest {
                        found_tx2 = true;
                        info!("Found tx2 in block at height {}", block.height);
                    }
                    if digest == tx3_digest {
                        found_tx3 = true;
                        info!("Found tx3 in block at height {}", block.height);
                    }
                }
            }
            
            if found_tx1 && found_tx2 && found_tx3 {
                info!("All transactions found in blocks!");
                break;
            }
        }

        // Verify all transactions were included
        assert!(found_tx1, "Transaction 1 not found in any block");
        assert!(found_tx2, "Transaction 2 not found in any block");
        assert!(found_tx3, "Transaction 3 not found in any block");
        
        info!("Test completed successfully! Height reached: {}", max_height);
    });
}

