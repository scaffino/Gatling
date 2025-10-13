# Transaction Implementation for Alto

This document describes the transaction and mempool implementation added to the alto blockchain.

## Overview

The implementation adds support for:
1. **Simple Transactions**: Transactions with sender, receiver, and amount fields
2. **Mempool**: In-memory storage for pending transactions
3. **Transaction Submission**: Mechanism to submit transactions to validators
4. **Block Integration**: Validators include mempool transactions in proposed blocks

## Architecture

### Transaction Flow

```
Client → Validator.Mailbox.submit_transaction()
       → Actor receives SubmitTransaction message
       → Verifies signature & adds to mempool
       → During block proposal: pulls transactions from mempool
       → Includes in proposed block
       → Other validators verify transaction signatures
```

## Components

### 1. Transaction Type (`alto/types/src/transaction.rs`)

```rust
pub struct Transaction {
    pub sender: PublicKey,
    pub receiver: PublicKey,
    pub amount: u64,
    pub signature: ed25519::Signature,
}
```

**Features:**
- Ed25519 signature-based authentication
- Batch verification support for efficiency
- Serialization/deserialization
- Digest computation

**Usage:**
```rust
use alto_types::Transaction;
use commonware_cryptography::ed25519::PrivateKey;

let sender_key = PrivateKey::from_seed(1);
let receiver_key = PrivateKey::from_seed(2);
let receiver = receiver_key.public_key();

let tx = Transaction::sign(&sender_key, receiver, 100);
assert!(tx.verify());
```

### 2. Updated Block Type (`alto/types/src/block.rs`)

Blocks now include a `transactions` field:

```rust
pub struct Block {
    pub parent: Digest,
    pub height: u64,
    pub timestamp: u64,
    pub transactions: Vec<Transaction>,  // New field
    digest: Digest,
}
```

**Constants:**
- `MAX_BLOCK_TRANSACTIONS = 100`: Maximum transactions per block

### 3. Mempool (`alto/chain/src/application/mempool.rs`)

In-memory transaction pool with:
- **Capacity**: 32,768 total transactions
- **Per-account limit**: 16 pending transactions
- **Processing**: Round-robin to ensure fairness
- **Metrics**: Prometheus metrics for monitoring

**Methods:**
```rust
pub fn add(&mut self, tx: Transaction)  // Add transaction
pub fn next(&mut self) -> Option<Transaction>  // Get next transaction
```

### 4. Actor Integration (`alto/chain/src/application/actor.rs`)

The application actor now:
1. **Maintains a mempool** instance
2. **Handles SubmitTransaction messages** - verifies and adds to mempool
3. **Proposes blocks** - includes up to 100 transactions from mempool
4. **Verifies blocks** - batch-verifies all transaction signatures

### 5. Transaction Submission

#### Via Mailbox (Direct Access)

If you have access to the application mailbox:

```rust
use alto_types::Transaction;

// Get mailbox reference (from engine or application)
let mut mailbox: alto_chain::application::Mailbox = ...;

// Create and submit transaction
let tx = Transaction::sign(&private_key, receiver, amount);
mailbox.submit_transaction(tx).await?;
```

#### Via Client (HTTP - Requires External Indexer)

The client has a submission method, but **requires an external indexer/HTTP server** to handle the endpoint:

```rust
use alto_client::Client;

let client = Client::new("http://indexer.example.com", identity);
client.transaction_submit(transaction).await?;
```

**Note:** The HTTP endpoint `/transaction` must be implemented by an external indexer service that then forwards transactions to validators via WebSocket (similar to battleware's architecture).

## Implementation Details

### Message Flow

1. **SubmitTransaction Message** added to `application::ingress::Message` enum
2. **submit_transaction()** method added to `application::Mailbox`
3. **Actor handles message**: 
   - Verifies transaction signature
   - Adds to mempool if valid
   - Logs result

### Block Proposal

During consensus propose phase:
```rust
// Collect up to MAX_BLOCK_TRANSACTIONS from mempool
let mut transactions = Vec::new();
while transactions.len() < MAX_BLOCK_TRANSACTIONS {
    if let Some(tx) = mempool.next() {
        transactions.push(tx);
    } else {
        break;
    }
}

// Create block with transactions
let block = Block::new(parent_digest, height, timestamp, transactions);
```

### Block Verification

During consensus verify phase:
```rust
// Batch verify all transaction signatures
let mut batcher = Batch::new();
for tx in &block.transactions {
    tx.verify_batch(&mut batcher);
}
if !batcher.verify(&mut context) {
    return false;  // Block is invalid
}
```

## Testing

The implementation includes comprehensive tests:

### Transaction Tests
- Sign and verify
- Encode/decode
- Digest computation
- Batch verification

### Mempool Tests
- Add single/multiple transactions
- Duplicate handling
- Max backlog enforcement
- Round-robin processing
- Metrics updates

## Limitations & Future Work

1. **No HTTP Server in Validator**: Currently, transactions must be submitted directly via the application mailbox. For external clients, an HTTP server or indexer service is needed.

2. **No Nonce Management**: The current mempool uses an internal counter for ordering. A production system would use account nonces.

3. **No Transaction Execution**: Transactions are stored and verified but not executed. State transition logic would be added to the `Finalized` message handler.

4. **No Fee Market**: No transaction fees or priority mechanism.

## Git Branch

All changes are on the `transactions` branch:
```bash
cd alto
git checkout transactions
```

## Files Modified/Created

- `alto/types/src/transaction.rs` (new)
- `alto/types/src/block.rs` (modified - added transactions field)
- `alto/types/src/lib.rs` (modified - exports)
- `alto/chain/src/application/mempool.rs` (new)
- `alto/chain/src/application/actor.rs` (modified - mempool integration)
- `alto/chain/src/application/ingress.rs` (modified - SubmitTransaction message)
- `alto/chain/src/application/mod.rs` (modified - export mempool)
- `alto/client/src/lib.rs` (modified - transaction_submit method)

## Local Testing & Examples

### Running the Test

A comprehensive integration test is available:

```bash
cd alto/chain
cargo test test_transaction_flow -- --nocapture
```

This test:
1. Creates a 5-validator network
2. Submits 3 transactions to a validator
3. Verifies all transactions are included in blocks
4. Shows transaction digests and block heights

### Running the Example

A standalone example demonstrates the full flow:

```bash
cd alto
cargo run --example submit_transaction
```

Expected output:
```
=== Transaction Submission Example ===

✓ Started 5 validators
✓ Network synchronized

Step 1: Creating transaction participants...
  Alice:   PublicKey(...)
  Bob:     PublicKey(...)
  Charlie: PublicKey(...)

Step 2: Creating transactions...
  TX1: Alice -> Bob (100 units)
  TX2: Bob -> Charlie (50 units)
  TX3: Charlie -> Alice (25 units)

Step 3: Submitting transactions to validator 0...
  ✓ TX1 submitted
  ✓ TX2 submitted
  ✓ TX3 submitted

Step 4: Waiting for transactions to be included in blocks...
  ✓ TX1 included in block at height 7
  ✓ TX2 included in block at height 7
  ✓ TX3 included in block at height 8

✓ All transactions successfully included in blocks!

=== Example completed successfully! ===
```

### Manual Transaction Submission

If you have access to an engine instance:

```rust
use alto_types::Transaction;
use commonware_cryptography::{ed25519::PrivateKey, PrivateKeyExt, Signer};

// Create your engine
let engine = engine::Engine::new(context, config).await;

// Get application mailbox
let mut mailbox = engine.application_mailbox().clone();

// Create and sign transaction
let sender_key = PrivateKey::from_seed(1);
let receiver_key = PrivateKey::from_seed(2);
let tx = Transaction::sign(&sender_key, receiver_key.public_key(), 100);

// Submit to validator
mailbox.submit_transaction(tx).await?;
```

### Verifying Transaction Inclusion

To check if a transaction was included:

```rust
use commonware_cryptography::Digestible;

// Get transaction digest
let tx_digest = transaction.digest();

// Later, check blocks
for block in finalized_blocks {
    for tx in &block.transactions {
        if tx.digest() == tx_digest {
            println!("Transaction included at height {}", block.height);
        }
    }
}
```

## Summary

The implementation provides a working transaction system for alto with:
- ✅ Simple transaction structure (sender, receiver, amount)
- ✅ Cryptographic signatures for authenticity
- ✅ Mempool for pending transactions
- ✅ Block inclusion mechanism
- ✅ Signature verification during consensus
- ✅ Direct submission via application mailbox
- ✅ Comprehensive tests and examples
- ✅ Engine accessor method for mailbox

For production use, you would need to add:
- HTTP server to receive external transaction submissions
- Account nonce management
- State execution logic
- Transaction fee mechanism

