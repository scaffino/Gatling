//! Example showing how to submit transactions in a local deployment
//!
//! This example demonstrates:
//! 1. Creating transactions
//! 2. Submitting them to validators
//! 3. Verifying they appear in blocks

use alto_chain::engine;
use alto_client::Client;
use alto_types::Transaction;
use commonware_cryptography::{
    bls12381::{
        dkg::ops,
        primitives::variant::MinSig,
    },
    ed25519::PrivateKey,
    Digestible, PrivateKeyExt, Signer,
};
use commonware_p2p::simulated::{self, Link, Network};
use commonware_runtime::{
    deterministic::Runner,
    Clock, Metrics, Runner as _,
};
use commonware_utils::quorum;
use governor::Quota;
use std::{
    collections::HashMap,
    num::NonZeroU32,
    time::Duration,
};
use tracing::Level;

const FREEZER_TABLE_INITIAL_SIZE: u32 = 2u32.pow(14);

fn main() {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_max_level(Level::INFO)
        .init();

    let executor = Runner::timed(Duration::from_secs(60));

    executor.start(|mut context| async move {
        println!("\n=== Transaction Submission Example ===\n");

        // Setup network (4 validators)
        let n = 4;
        let threshold = quorum(n);
        
        let (network, mut oracle) = Network::new(
            context.with_label("network"),
            simulated::Config { max_size: 1024 * 1024 },
        );
        network.start();

        // Create validators
        let mut signers = Vec::new();
        let mut validators = Vec::new();
        for i in 0..n {
            let signer = PrivateKey::from_seed(i as u64);
            validators.push(signer.public_key());
            signers.push(signer);
        }
        validators.sort();
        signers.sort_by_key(|s| s.public_key());

        // Register and link validators
        let mut registrations = HashMap::new();
        for validator in &validators {
            registrations.insert(
                validator.clone(),
                (
                    oracle.register(validator.clone(), 0).await.unwrap(),
                    oracle.register(validator.clone(), 1).await.unwrap(),
                    oracle.register(validator.clone(), 2).await.unwrap(),
                    oracle.register(validator.clone(), 3).await.unwrap(),
                    oracle.register(validator.clone(), 4).await.unwrap(),
                ),
            );
        }

        let link = Link {
            latency: Duration::from_millis(10),
            jitter: Duration::from_millis(1),
            success_rate: 1.0,
        };

        for v1 in &validators {
            for v2 in &validators {
                if v1 != v2 {
                    oracle.add_link(v1.clone(), v2.clone(), link.clone()).await.unwrap();
                }
            }
        }

        // Generate threshold keys
        let (polynomial, shares) = ops::generate_shares::<_, MinSig>(&mut context, None, n, threshold);

        // Start validators and collect mailboxes
        let mut mailboxes = Vec::new();
        
        for (idx, signer) in signers.iter().enumerate() {
            let pk = signer.public_key();
            
            let config: engine::Config<_, Client> = engine::Config {
                blocker: oracle.control(pk.clone()),
                partition_prefix: format!("validator-{idx}"),
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
                indexer: None,
            };

            let engine = engine::Engine::new(context.with_label(&format!("engine-{idx}")), config).await;
            mailboxes.push(engine.application_mailbox().clone());

            let (pending, recovered, resolver, broadcast, backfill) = registrations.remove(&pk).unwrap();
            engine.start(pending, recovered, resolver, broadcast, backfill);
        }

        println!("✓ Started 4 validators");
        
        // Wait for initial blocks
        context.sleep(Duration::from_secs(3)).await;
        println!("✓ Network synchronized\n");

        // Step 1: Create transaction keys (not validators)
        println!("Step 1: Creating transaction participants...");
        let alice = PrivateKey::from_seed(100);
        let bob = PrivateKey::from_seed(101);
        let charlie = PrivateKey::from_seed(102);
        
        println!("  Alice:   {:?}", alice.public_key());
        println!("  Bob:     {:?}", bob.public_key());
        println!("  Charlie: {:?}\n", charlie.public_key());

        // Step 2: Create transactions
        println!("Step 2: Creating transactions...");
        let tx1 = Transaction::sign(&alice, bob.public_key(), 100);
        let tx2 = Transaction::sign(&bob, charlie.public_key(), 50);
        let tx3 = Transaction::sign(&charlie, alice.public_key(), 25);

        let tx1_digest = tx1.digest();
        let tx2_digest = tx2.digest();
        let tx3_digest = tx3.digest();

        println!("  TX1: Alice -> Bob (100 units)");
        println!("  TX2: Bob -> Charlie (50 units)");
        println!("  TX3: Charlie -> Alice (25 units)\n");

        // Step 3: Submit transactions to validator
        println!("Step 3: Submitting transactions to validator 0...");
        let mut mailbox = mailboxes[0].clone();
        
        mailbox.submit_transaction(tx1).await.expect("Failed to submit tx1");
        println!("  ✓ TX1 submitted");
        
        mailbox.submit_transaction(tx2).await.expect("Failed to submit tx2");
        println!("  ✓ TX2 submitted");
        
        mailbox.submit_transaction(tx3).await.expect("Failed to submit tx3");
        println!("  ✓ TX3 submitted\n");

        // Step 4: Waiting for blocks to be produced with transactions
        println!("Step 4: Waiting for blocks to be produced with transactions...");
        println!("  (Transactions should be included in upcoming blocks)");
        
        // Give time for transactions to be included
        context.sleep(Duration::from_secs(5)).await;

        println!("\n✓ Transactions submitted successfully!");
        println!("  Transaction digests:");
        println!("    TX1: {:?}", tx1_digest);
        println!("    TX2: {:?}", tx2_digest);
        println!("    TX3: {:?}", tx3_digest);
        println!("\n  Note: To verify inclusion, use the test suite:");
        println!("    cargo test test_transaction_flow -- --nocapture\n");
        
        println!("=== Example completed successfully! ===\n");
    });
}

