# Transaction Submission Guide for Local Deployment

This guide explains how to create, submit, and verify transactions in the alto blockchain during local deployment.

## Quick Start

### Method 1: Run the Integration Test (Recommended)

The easiest way to see transactions working is to run the integration test:

```bash
cd alto/chain
cargo test test_transaction_flow -- --nocapture
```

**Output Example:**
```
INFO transaction_test: Creating and submitting transactions
INFO alto_chain::application::actor: proposed new block ... txs=3 success=true
INFO transaction_test: Found tx1 in block at height 19
INFO transaction_test: Found tx2 in block at height 19
INFO transaction_test: Found tx3 in block at height 19
INFO transaction_test: All transactions found in blocks!
INFO transaction_test: Test completed successfully! Height reached: 36
```

**What this shows:**
- ✅ 3 transactions are created and submitted
- ✅ Validator includes all 3 in a single proposed block (txs=3)
- ✅ All transactions are found in the finalized block at height 19
- ✅ Network continues to produce blocks after transaction inclusion

### Method 2: Run the Standalone Example

```bash
cd alto/chain
cargo run --example submit_transaction
```

This demonstrates transaction submission programmatically.

## Step-by-Step: How to Submit Transactions

### 1. Get Access to a Validator's Application Mailbox

When creating an engine, store a reference to its application mailbox:

```rust
use alto_chain::engine;
use alto_types::Transaction;

// Create the engine
let engine = engine::Engine::new(context, config).await;

// Get mailbox BEFORE starting the engine
let mut mailbox = engine.application_mailbox().clone();

// Now start the engine
engine.start(pending, recovered, resolver, broadcast, backfill);
```

**Important:** Clone the mailbox before calling `start()` since `start()` consumes the engine.

### 2. Create a Transaction

```rust
use commonware_cryptography::{ed25519::PrivateKey, PrivateKeyExt, Signer};
use commonware_cryptography::Digestible;

// Create sender and receiver keys
let sender_key = PrivateKey::from_seed(100);
let receiver_key = PrivateKey::from_seed(101);

// Sign the transaction
let tx = Transaction::sign(
    &sender_key,
    receiver_key.public_key(),
    100  // amount
);

// Get the transaction digest for tracking
let tx_digest = tx.digest();
println!("Transaction digest: {:?}", tx_digest);
```

### 3. Submit Transaction to Validator

```rust
// Submit to validator's mempool
mailbox.submit_transaction(tx).await
    .expect("Failed to submit transaction");

println!("✓ Transaction submitted to mempool");
```

**What happens:**
1. Transaction is sent to the actor via the mailbox
2. Actor verifies the signature
3. If valid, added to mempool
4. When validator proposes next block, transaction is included

### 4. Verify Transaction Inclusion

To verify a transaction was included in a block, you need to track finalized blocks.

#### Option A: Using a Custom Indexer (Recommended for Production)

Implement a custom `Indexer` that tracks blocks:

```rust
use alto_chain::indexer;
use alto_types::{Block, Finalized, Notarized, Seed};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
struct BlockTracker {
    blocks: Arc<Mutex<Vec<Block>>>,
}

impl indexer::Indexer for BlockTracker {
    type Error = std::io::Error;
    
    fn new(_uri: &str, _identity: alto_types::Identity) -> Self {
        Self {
            blocks: Arc::new(Mutex::new(Vec::new())),
        }
    }
    
    async fn seed_upload(&self, _seed: Seed) -> Result<(), Self::Error> {
        Ok(())
    }
    
    async fn notarized_upload(&self, notarized: Notarized) -> Result<(), Self::Error> {
        self.blocks.lock().unwrap().push(notarized.block);
        Ok(())
    }
    
    async fn finalized_upload(&self, finalized: Finalized) -> Result<(), Self::Error> {
        self.blocks.lock().unwrap().push(finalized.block);
        Ok(())
    }
}

// Use in config
let tracker = BlockTracker::new("", identity);
let config = engine::Config {
    // ... other fields ...
    indexer: Some(tracker.clone()),
};

// Later, check for transaction
let blocks = tracker.blocks.lock().unwrap();
for block in blocks.iter() {
    for tx in &block.transactions {
        if tx.digest() == tx_digest {
            println!("Found transaction at height {}", block.height);
        }
    }
}
```

#### Option B: Monitor Logs

When validators propose blocks with transactions, you'll see in the logs:

```
INFO alto_chain::application::actor: proposed new block ... txs=3 success=true
```

The `txs=3` indicates 3 transactions were included in that block.

## Complete Working Example

See `alto/chain/tests/transaction_test.rs` for a complete working example that:

1. Sets up a 5-validator network
2. Creates 3 transactions (Alice→Bob, Bob→Charlie, Charlie→Alice)
3. Submits them to validator 0
4. Verifies all 3 are included in blocks
5. Reports the block height where they were found

**Key Code Snippet:**
```rust
// Create transaction
let tx = Transaction::sign(&alice, bob.public_key(), 100);
let tx_digest = tx.digest();

// Submit to validator
let mut mailbox = mailboxes[0].clone();
mailbox.submit_transaction(tx).await?;

// Later... verify in blocks
for block in finalized_blocks {
    for tx in &block.transactions {
        if tx.digest() == tx_digest {
            println!("Found at height {}", block.height);
        }
    }
}
```

## Architecture Overview

```
┌─────────┐                    ┌──────────────┐
│ Client  │                    │  Validator   │
│  Code   │                    │              │
└────┬────┘                    │  ┌────────┐  │
     │                         │  │ Actor  │  │
     │  1. Create Transaction  │  │        │  │
     │     Transaction::sign() │  │ ┌────┐ │  │
     │                         │  │ │Mem │ │  │
     │  2. Submit via Mailbox  │  │ │pool│ │  │
     ├────────────────────────►│  │ └────┘ │  │
     │     mailbox.submit_     │  │        │  │
     │     transaction(tx)     │  └────────┘  │
     │                         │              │
     │  3. Actor Processes     │  - Verify    │
     │                         │  - Add to    │
     │                         │    mempool   │
     │                         │              │
     │  4. Consensus Propose   │  - Pull from │
     │                         │    mempool   │
     │                         │  - Include   │
     │                         │    in block  │
     │                         │              │
     │  5. Block Finalized     │  - Txs in    │
     │                         │    block!    │
     └─────────────────────────┴──────────────┘
```

## Important Notes

### Synchronous Submission
Transaction submission is synchronous - when `submit_transaction()` returns, the transaction is in the mempool. However, inclusion in a block depends on:
- When the next block is proposed
- Who the proposer is (any validator can include it)
- Network conditions

### Transaction Signature Verification
Transactions are verified at TWO points:
1. **On submission**: Actor verifies signature before adding to mempool
2. **During consensus**: All validators batch-verify signatures when verifying blocks

Invalid transactions are rejected at submission time and never reach the mempool.

### Mempool Characteristics
- **Capacity**: 32,768 total transactions
- **Per-sender limit**: 16 pending transactions  
- **Processing**: Round-robin (fair ordering)
- **Block limit**: Up to 100 transactions per block

### Current Limitations

1. **No HTTP Endpoint**: This implementation requires direct access to the validator's mailbox. For external clients, you'd need to add an HTTP server that accepts POST requests and forwards to the mailbox.

2. **No State Execution**: Transactions are included in blocks but not executed. You would add execution logic in the `Message::Finalized` handler.

3. **Simplified Nonce Management**: The mempool uses an internal counter rather than account-based nonces.

## Testing in Different Scenarios

### Scenario 1: Submit to Multiple Validators

```rust
// Submit same transaction to multiple validators
for mailbox in &mut mailboxes {
    mailbox.submit_transaction(tx.clone()).await?;
}
```

All validators will have the transaction in their mempool. The first to propose will include it.

### Scenario 2: High Transaction Volume

```rust
// Create many transactions
for i in 0..1000 {
    let tx = Transaction::sign(&sender, receiver.public_key(), i);
    mailbox.submit_transaction(tx).await?;
}
```

Transactions will be:
- Added to mempool (up to capacity)
- Included across multiple blocks (100 per block)
- Processed in round-robin order

### Scenario 3: Invalid Transaction

```rust
// Create transaction with wrong signature
let mut tx = Transaction::sign(&sender, receiver.public_key(), 100);
tx.signature = wrong_signature;  // Corrupt it

// This will fail verification and be rejected
mailbox.submit_transaction(tx).await?;
// Transaction will NOT be added to mempool
```

Check logs for: `WARN alto_chain::application::actor: rejected invalid transaction`

## Summary

**To submit transactions in local deployment:**

1. ✅ Get `application_mailbox` from engine before starting
2. ✅ Create and sign transaction with `Transaction::sign()`  
3. ✅ Call `mailbox.submit_transaction(tx).await`
4. ✅ Transaction enters mempool and will be included in next proposed block
5. ✅ Verify by tracking finalized blocks via custom indexer

**To verify inclusion:**
- Watch for `txs=N` in proposal logs (N > 0)
- Implement custom indexer to track blocks
- Search blocks for transaction by digest
- Run integration test: `cargo test test_transaction_flow`


