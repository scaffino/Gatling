# How to Test Transactions in Local Deployment

## Two Ways to Test

### Method 1: Run the Integration Test (BEST FOR VERIFICATION)

**Run this command to see transactions working:**

```bash
cd /Users/gscaffino/Workspace/superfastBFT/evaluation/alto/chain
cargo test test_transaction_flow -- --nocapture
```

## What You'll See

```
INFO transaction_test: Creating and submitting transactions
INFO transaction_test: Transaction digests: 
  tx1=0addce3d..., 
  tx2=e2818ea3..., 
  tx3=0f95caff...
INFO transaction_test: Transactions submitted to validator, waiting for inclusion

... [validators producing blocks] ...

INFO alto_chain::application::actor: proposed new block ... txs=3 success=true

INFO transaction_test: Found tx1 in block at height 19
INFO transaction_test: Found tx2 in block at height 19
INFO transaction_test: Found tx3 in block at height 19
INFO transaction_test: All transactions found in blocks!
INFO transaction_test: Test completed successfully! Height reached: 36
```

## What This Proves

✅ **Step 1**: 3 transactions are created with sender, receiver, and amount  
✅ **Step 2**: Transactions are submitted to a validator  
✅ **Step 3**: Validator adds transactions to its mempool  
✅ **Step 4**: When proposing block at view 20, validator includes all 3 transactions (`txs=3`)  
✅ **Step 5**: Block is finalized at height 19  
✅ **Step 6**: All validators verify the transaction signatures  
✅ **Step 7**: Transactions are found in the finalized block  

### Method 2: Run the Standalone Example

**Run this command for a simpler demonstration:**

```bash
cd /Users/gscaffino/Workspace/superfastBFT/evaluation/alto/chain
cargo run --release --example submit_transaction
```

**What you'll see:**
```
=== Transaction Submission Example ===

✓ Started 4 validators
✓ Network synchronized

Step 1: Creating transaction participants...
  Alice:   ae3a9bc6eb721b7b8a34d23aa0d5b1623e89bc6f092815ea13b92f79a39c7d38
  Bob:     80c02edc00c6b43231858aa5dd6c1911d7e489e470218da82193a470dfce50cf
  Charlie: 8e1ea87cdb41614693298cb65ebfa8c3e6d09dc9f358321d8e533a8813e75bc4

Step 2: Creating transactions...
  TX1: Alice -> Bob (100 units) - Digest: 0addce3d92811a904d639f4f5ff40a07fe0a0e97876aed38f7ee35365dd1a174
  TX2: Bob -> Charlie (50 units) - Digest: e2818ea31c7e2aec76bba9461f4535f0e6ecdf8f07a48f6d1e37d07ac9af0ab8
  TX3: Charlie -> Alice (25 units) - Digest: 0f95caff0a6cee6a5e339c699c1a98b11c89c761b5dbf370fc0405faccf68c9a

Step 3: Submitting transactions to validator 0...
  ✓ TX1 submitted
  ✓ TX2 submitted
  ✓ TX3 submitted

Step 4: Waiting for blocks to be produced with transactions...
  (Transactions should be included in upcoming blocks)

✓ Transactions submitted successfully!
  Transaction digests:
    TX1: 0addce3d92811a904d639f4f5ff40a07fe0a0e97876aed38f7ee35365dd1a174
    TX2: e2818ea31c7e2aec76bba9461f4535f0e6ecdf8f07a48f6d1e37d07ac9af0ab8
    TX3: 0f95caff0a6cee6a5e339c699c1a98b11c89c761b5dbf370fc0405faccf68c9a

  Note: To verify inclusion, use the test suite:
    cargo test test_transaction_flow -- --nocapture

=== Example completed successfully! ===
```

This example shows the submission process step-by-step. For full verification of inclusion in blocks, use **Method 1** (the integration test).

**To save clean output to a file (without ANSI color codes):**
```bash
cd /Users/gscaffino/Workspace/superfastBFT/evaluation/alto/chain
cargo run --release --example submit_transaction 2>&1 | sed $'s/\x1b\\[[0-9;]*m//g' > example.txt
```

Or to get only the main output without logs:
```bash
cargo run --release --example submit_transaction 2>&1 | sed $'s/\x1b\\[[0-9;]*m//g' | grep -E "(===|✓|Step|Alice|Bob|Charlie|TX|Note|Digest)" > example.txt
```

## Step-by-Step Code Flow

### 1. Create Transactions
```rust
let alice = PrivateKey::from_seed(100);
let bob = PrivateKey::from_seed(101);
let timestamp = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .expect("Time went backwards")
    .as_secs();
let tx = Transaction::sign(&alice, bob.public_key(), 100, timestamp);
```

### 2. Submit to Validator
```rust
let mut mailbox = engine.application_mailbox().clone();
mailbox.submit_transaction(tx).await?;
```

### 3. Transaction Goes to Mempool
```rust
// In actor.rs
Message::SubmitTransaction { transaction } => {
    if transaction.verify() {
        mempool.add(transaction);  // ← Transaction stored here
    }
}
```

### 4. Proposer Includes Transactions
```rust
// In actor.rs - Propose handler
let mut transactions = Vec::new();
while transactions.len() < MAX_BLOCK_TRANSACTIONS {
    if let Some(tx) = mempool.next() {
        transactions.push(tx);  // ← Pulled from mempool
    }
}
let block = Block::new(parent.digest(), height, timestamp, transactions);
```

### 5. Validators Verify
```rust
// In actor.rs - Verify handler
let mut batcher = Batch::new();
for tx in &block.transactions {
    tx.verify_batch(&mut batcher);  // ← All signatures verified
}
if !batcher.verify(&mut context) {
    return false;  // Block rejected if any signature invalid
}
```

## How to Use in Your Code

```rust
use alto_chain::engine;
use alto_types::Transaction;
use commonware_cryptography::{ed25519::PrivateKey, PrivateKeyExt, Signer};

// 1. Setup engine and get mailbox
let engine = engine::Engine::new(context, config).await;
let mut mailbox = engine.application_mailbox().clone();
engine.start(pending, recovered, resolver, broadcast, backfill);

// 2. Create transaction
let sender = PrivateKey::from_seed(1);
let receiver = PrivateKey::from_seed(2);
let timestamp = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .expect("Time went backwards")
    .as_secs();
let tx = Transaction::sign(&sender, receiver.public_key(), 100, timestamp);

// 3. Submit
mailbox.submit_transaction(tx).await?;

// 4. Transaction will be in next proposed block!
```

## Files to Review

- **Test**: `alto/chain/tests/transaction_test.rs` - Full integration test
- **Example**: `alto/chain/examples/submit_transaction.rs` - Standalone example
- **Implementation**: See `TRANSACTIONS_IMPLEMENTATION.md` for technical details
- **Architecture**: `TRANSACTION_GUIDE.md` for complete guide

## Quick Verification

Watch the logs for:
- `DEBUG alto_chain::application::actor: transaction added to mempool` - Transaction accepted
- `INFO alto_chain::application::actor: proposed new block ... txs=3` - Transactions included
- `WARN alto_chain::application::actor: rejected invalid transaction` - Invalid transaction rejected

## That's It!

The implementation is complete and working. Transactions are:
1. Created with sender, receiver, amount
2. Submitted to validators via mailbox
3. Stored in mempool
4. Included in proposed blocks
5. Verified by all validators
6. Present in finalized blocks

Run the test to see it in action! 🎉

