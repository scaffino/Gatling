# Transaction Workflow in Alto

This document describes the complete end-to-end workflow of a transaction in the alto blockchain, from creation to finalization.

## Visual Flow Diagram

```
┌─────────────────────────────────────────────────────────────────┐
│ PHASE 1: TRANSACTION CREATION & SUBMISSION                      │
└─────────────────────────────────────────────────────────────────┘

[Client/Validator]
    │
    ├─> 1. Create Transaction
    │     • Transaction::sign(sender_key, receiver, amount)
    │     • Generates signature using ed25519
    │     • Returns Transaction with sender, receiver, amount, signature
    │
    ├─> 2. Submit to Validator
    │     • mailbox.submit_transaction(tx).await
    │     • Sends SubmitTransaction message to Actor
    │
    v
[Actor] (application/actor.rs)
    │
    ├─> 3. Receive & Verify (Message::SubmitTransaction)
    │     • Verify transaction signature: tx.verify()
    │     • If valid: mempool.add(tx)
    │     • If invalid: reject with warning
    │     • LOG: "transaction added to mempool"
    │
    v
[Mempool] (application/mempool.rs)
    │
    └─> 4. Store in Mempool
          • Store in HashMap by digest
          • Track by sender's public key
          • Add to round-robin queue
          • Metrics: transactions count, accounts count

┌─────────────────────────────────────────────────────────────────┐
│ PHASE 2: BLOCK PROPOSAL                                         │
└─────────────────────────────────────────────────────────────────┘

[Consensus Engine]
    │
    ├─> 5. Consensus Triggers Propose
    │     • When it's this validator's turn to propose
    │     • Sends Automaton::propose() → Message::Propose
    │
    v
[Actor] (Message::Propose handler)
    │
    ├─> 6. Pull Transactions from Mempool
    │     • while transactions.len() < MAX_BLOCK_TRANSACTIONS (100)
    │     •   mempool.next() → gets oldest transaction
    │     • Round-robin ensures fairness across senders
    │
    ├─> 7. Create Block
    │     • Get parent block
    │     • timestamp = max(current_time, parent.timestamp + 1)
    │     • Block::new(parent_digest, height, timestamp, transactions)
    │     • Compute block digest (includes tx digests)
    │     • LOG: "proposed new block txs=N"
    │
    ├─> 8. Store Built Block
    │     • Save in built = Arc<Mutex<Option<(View, Block)>>>
    │     • Return block digest to consensus
    │
    v
[Consensus Engine]
    │
    └─> 9. Consensus Votes on Proposal
          • Validators exchange votes
          • If quorum reached → notarization

┌─────────────────────────────────────────────────────────────────┐
│ PHASE 3: BLOCK BROADCAST & VERIFICATION                         │
└─────────────────────────────────────────────────────────────────┘

[Consensus Engine]
    │
    ├─> 10. Broadcast Trigger (if proposer)
    │      • Relay::broadcast() → Message::Broadcast
    │
    v
[Actor] (Message::Broadcast handler)
    │
    ├─> 11. Broadcast Block
    │      • Get built block from Arc<Mutex>
    │      • marshal.broadcast(block) → sends to all validators
    │      • LOG: "broadcast requested"
    │
    v
[Marshal] → [Network] → [Other Validators]
    │
    v
[Other Validators' Consensus]
    │
    ├─> 12. Receive Block Proposal
    │      • Consensus calls Automaton::verify()
    │      • Sends Message::Verify to Actor
    │
    v
[Actor] (Message::Verify handler)
    │
    ├─> 13. Verify Block
    │      • Check height = parent.height + 1
    │      • Check parent digest matches
    │      • Check timestamp > parent.timestamp
    │      • Check timestamp < current + SYNCHRONY_BOUND (500ms)
    │      
    ├─> 14. Batch Verify Transaction Signatures
    │      • let mut batcher = Batch::new()
    │      • for tx in block.transactions:
    │      •   tx.verify_batch(&mut batcher)
    │      • batcher.verify(&mut context)
    │      • If any signature invalid → reject entire block
    │
    ├─> 15. Persist Verified Block
    │      • marshal.verified(view, block)
    │      • Send true/false response to consensus
    │
    v
[Consensus Engine]
    │
    └─> 16. Collect Votes
          • If threshold validators verify → notarization
          • If threshold notarizations → finalization

┌─────────────────────────────────────────────────────────────────┐
│ PHASE 4: BLOCK FINALIZATION                                     │
└─────────────────────────────────────────────────────────────────┘

[Consensus Engine]
    │
    ├─> 17. Block Finalized
    │      • Quorum of validators finalized the block
    │      • Reporter::report() → Message::Finalized
    │
    v
[Actor] (Message::Finalized handler)
    │
    ├─> 18. Process Finalized Block
    │      • LOG: "processed block" with height, digest
    │      • Update metrics: finalized_blocks_counter++
    │      • Calculate block latency: now - block.timestamp
    │      • Record in block_latency_ms_histogram
    │      
    └─> 19. Transactions Now Confirmed! ✅
          • All transactions in block.transactions are now final
          • They will never be reversed
          • State can be updated based on these transactions
```

## Detailed Code Flow

### Step-by-Step Code Trace

#### Step 1: Transaction Creation
**File**: `types/src/transaction.rs`

```rust
// Client creates transaction
let tx = Transaction::sign(&sender_private_key, receiver, amount);

// Internally:
pub fn sign(private: &ed25519::PrivateKey, receiver: PublicKey, amount: u64) -> Self {
    let sender = private.public_key();
    let signature = private.sign(
        Some(&transaction_namespace(crate::NAMESPACE)),
        &Self::payload(&sender, &receiver, &amount),
    );
    Self { sender, receiver, amount, signature }
}
```

#### Step 2: Submission via Mailbox
**File**: `application/ingress.rs:50-56`

```rust
pub async fn submit_transaction(&mut self, transaction: Transaction) -> Result<(), String> {
    self.sender
        .send(Message::SubmitTransaction { transaction })
        .await
        .map_err(|_| "Failed to send transaction".to_string())
}
```

#### Step 3: Actor Receives Transaction
**File**: `application/actor.rs:273-281`

```rust
Message::SubmitTransaction { transaction } => {
    // Verify transaction signature before adding to mempool
    if transaction.verify() {
        mempool.add(transaction);
        debug!("transaction added to mempool");
    } else {
        warn!("rejected invalid transaction");
    }
}
```

#### Step 4: Mempool Storage
**File**: `application/mempool.rs:56-102`

```rust
pub fn add(&mut self, tx: Transaction) {
    // Check capacity (max 32,768 transactions)
    if self.transactions.len() >= MAX_TRANSACTIONS {
        return;
    }

    // Get digest and check for duplicates
    let digest = tx.digest();
    if self.transactions.contains_key(&digest) {
        return;
    }

    // Track by sender (max 16 per sender)
    let public = tx.sender.clone();
    let nonce = *self.nonces.get(&public).unwrap_or(&0);
    let entry = self.tracked.entry(public.clone()).or_default();
    
    // Store transaction
    entry.insert(nonce, digest);
    self.transactions.insert(digest, tx);
    self.nonces.insert(public.clone(), nonce + 1);
    
    // Add to round-robin queue
    if entry.len() == 1 {
        self.queue.push_back(public);
    }
    
    // Update metrics
    self.unique.set(self.transactions.len() as i64);
    self.accounts.set(self.tracked.len() as i64);
}
```

#### Step 5-6: Consensus Proposes, Actor Pulls from Mempool
**File**: `application/actor.rs:105-118`

```rust
Message::Propose { view, parent, mut response } => {
    // Collect transactions from mempool (up to 100)
    let mut transactions = Vec::new();
    while transactions.len() < MAX_BLOCK_TRANSACTIONS {
        let Some(tx) = mempool.next() else {
            break;
        };
        transactions.push(tx);
    }
    let tx_count = transactions.len();
    
    // ... continue to create block
}
```

**File**: `application/mempool.rs:108-141` (mempool.next())

```rust
pub fn next(&mut self) -> Option<Transaction> {
    loop {
        // Get from front of round-robin queue
        let address = self.queue.pop_front()?;
        let Some(tracked) = self.tracked.get_mut(&address) else {
            continue;
        };
        
        // Get transaction with lowest nonce
        let Some((_, digest)) = tracked.pop_first() else {
            continue;
        };

        // Re-queue if sender has more transactions
        if !tracked.is_empty() {
            self.queue.push_back(address);
        } else {
            self.tracked.remove(&address);
        }

        // Return the transaction
        let tx = self.transactions.remove(&digest).unwrap();
        break Some(tx);
    }
}
```

#### Step 7: Block Creation
**File**: `application/actor.rs:140-149`

```rust
// Create a new block with transactions
let mut current = context.current().epoch_millis();
if current <= parent.timestamp {
    current = parent.timestamp + 1;
}
let block = Block::new(parent.digest(), parent.height+1, current, transactions);
let digest = block.digest();

// Store built block
{
    let mut built = built.lock().unwrap();
    *built = Some((view, block));
}

// Return digest to consensus
let result = response.send(digest);
info!(engine_id=%engine_id, view, ?digest, txs=tx_count, success=result.is_ok(), 
     "proposed new block");
```

**File**: `types/src/block.rs:43-53`

```rust
pub fn new(parent: Digest, height: u64, timestamp: u64, transactions: Vec<Transaction>) -> Self {
    assert!(transactions.len() <= MAX_BLOCK_TRANSACTIONS);
    let digest = Self::compute_digest(&parent, height, timestamp, &transactions);
    Self {
        parent,
        height,
        timestamp,
        transactions,  // ← Transactions embedded in block
        digest,
    }
}
```

#### Step 8-9: Consensus Voting
The consensus engine handles this automatically using the threshold simplex protocol.

#### Step 10-11: Broadcast
**File**: `application/actor.rs:163-180`

```rust
Message::Broadcast { payload } => {
    // Get the built block
    let Some(built) = built.lock().unwrap().clone() else {
        warn!(engine_id=%engine_id, ?payload, "missing block to broadcast");
        continue;
    };

    // Broadcast to all validators
    debug!(engine_id=%engine_id, ?payload, view = built.0, height = built.1.height,
           "broadcast requested");
    marshal.broadcast(built.1.clone()).await;
}
```

#### Step 12-15: Other Validators Verify
**File**: `application/actor.rs:182-250`

```rust
Message::Verify { view, parent, payload, mut response } => {
    // Get parent and current block
    // ...
    
    self.context.with_label("verify").spawn({
        let mut marshal = marshal.clone();
        let engine_id = self.engine_id.clone();
        move |mut context| async move {
            let requester = try_join(parent_request, marshal.subscribe(None, payload).await);
            let response_closed = OneshotClosedFut::new(&mut response);
            select! {
                result = requester => {
                    let (parent, block) = result.unwrap();

                    // Verify block structure
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
                        let _ = response.send(false);  // ← Invalid signatures = reject block
                        return;
                    }

                    // Persist verified block
                    marshal.verified(view, block).await;

                    // Send verification result
                    let _ = response.send(true);
                },
                _ = response_closed => {
                    warn!(engine_id=%engine_id, view, "verify aborted");
                }
            }
        }
    });
}
```

#### Step 16: Consensus Collects Votes
The consensus engine collects votes from validators:
- If ≥ threshold validators verify → **notarization**
- If ≥ threshold notarizations → **finalization**

#### Step 17-19: Block Finalization
**File**: `application/actor.rs:252-271`

```rust
Message::Finalized { block } => {
    // Block is now finalized!
    // All transactions in block.transactions are confirmed
    
    let engine_id = self.engine_id.clone();
    let digest = block.digest();
    
    // Update metrics (only once per unique block)
    if self.finalized_seen.insert(digest.to_vec()) {
        self.finalized_blocks_counter.inc();
        let now_ms = self.context.current().epoch_millis();
        let latency_ms = now_ms.saturating_sub(block.timestamp);
        self.block_latency_ms_histogram.observe(latency_ms as f64);
    }
    
    info!(
        engine_id=%engine_id,
        height = block.height,
        digest = ?block.commitment(),
        "processed block"  // ← Transactions are now FINAL!
    );
}
```

## Transaction States

### State Progression

```
Created → Submitted → In Mempool → Proposed → Verified → Notarized → Finalized ✅
   ↓          ↓           ↓            ↓          ↓          ↓           ↓
  Sign    Mailbox    HashMap      Block    Signatures  Quorum     Confirmed
                                           Checked     Reached    Irreversible
```

1. **Created**: Transaction object exists with valid signature
2. **Submitted**: Sent to validator's mailbox
3. **In Mempool**: Stored in HashMap, waiting for inclusion
4. **Proposed**: Included in a block proposal (not yet confirmed)
5. **Verified**: Other validators checked block + transaction signatures
6. **Notarized**: Quorum of validators voted for the block
7. **Finalized**: ✅ **CONFIRMED** - irreversible, permanent

## Verification Points

Transactions are verified **twice** for security:

### First Verification: On Submission
**Location**: `application/actor.rs:275`
```rust
if transaction.verify() {  // ← Single signature check
    mempool.add(transaction);
}
```

**Purpose**: Prevent invalid transactions from entering the mempool

### Second Verification: During Block Verification
**Location**: `application/actor.rs:228-236`
```rust
// Batch verify ALL transaction signatures in the block
let mut batcher = Batch::new();
for tx in &block.transactions {
    tx.verify_batch(&mut batcher);
}
if !batcher.verify(&mut context) {
    return false;  // ← Reject entire block if any signature invalid
}
```

**Purpose**: Ensure proposed blocks contain only valid transactions

## Mempool Characteristics

### Storage Strategy
- **Primary index**: HashMap<Digest, Transaction>
- **Sender tracking**: HashMap<PublicKey, BTreeMap<Nonce, Digest>>
- **Processing queue**: VecDeque<PublicKey> (round-robin)

### Capacity Limits
- **Total transactions**: 32,768
- **Per sender**: 16 transactions max
- **Per block**: 100 transactions max (`MAX_BLOCK_TRANSACTIONS`)

### Ordering
- **FIFO per sender**: Transactions from same sender processed in order
- **Round-robin across senders**: Fair processing across all senders
- **Prevents spam**: Per-sender limit prevents one sender from filling mempool

### Retrieval (mempool.next())
```rust
// 1. Pop sender from front of queue
// 2. Get their oldest transaction (lowest nonce)
// 3. If sender has more, re-add to back of queue
// 4. Return transaction
```

## Consensus Integration

The transaction workflow is embedded in the consensus flow:

### Message Flow
```
Consensus → Propose → Actor pulls txs from mempool
          ↓
        Create block with transactions
          ↓
        Broadcast → Other validators
          ↓
        Verify → Check structure + batch verify signatures
          ↓
        Vote → If valid, vote to notarize
          ↓
        Finalize → When quorum reached
          ↓
        Actor processes finalized block
```

### Consensus Messages to Application
1. **Genesis**: Get initial state
2. **Propose**: Create new block (includes transactions)
3. **Broadcast**: Broadcast built block
4. **Verify**: Verify received block (checks transaction signatures)
5. **Finalized**: Process finalized block (transactions confirmed)

## Key Files in Workflow

| File | Purpose | Key Functions |
|------|---------|---------------|
| `types/src/transaction.rs` | Transaction definition | `sign()`, `verify()`, `verify_batch()` |
| `types/src/block.rs` | Block with transactions | `new()`, `compute_digest()` |
| `application/mempool.rs` | Transaction storage | `add()`, `next()` |
| `application/ingress.rs` | Message types | `SubmitTransaction`, `Propose`, `Verify`, `Finalized` |
| `application/actor.rs` | Workflow orchestration | All message handlers |
| `engine.rs` | Consensus integration | `Engine::new()`, `start()` |

## Latency Components

The total confirmation latency consists of:

1. **Submission time**: Client → Mailbox → Actor → Mempool (~0-1ms)
2. **Mempool wait**: Until validator proposes (variable, depends on view rotation)
3. **Proposal time**: Creating block (~1ms)
4. **Network time**: Broadcasting to validators (~10-100ms depending on network)
5. **Verification time**: Batch signature verification (~10-50ms for 100 txs)
6. **Consensus time**: Voting rounds to reach finalization (~100-1000ms)

**Total typical latency**: 100ms - 2000ms depending on network conditions and validator rotation

## Transaction Security Guarantees

### Before Finalization
- ✅ Signature verified on submission
- ✅ Signature verified again during block verification
- ✅ Block structure validated
- ⚠️ **Not yet final** - could be in competing fork

### After Finalization
- ✅ Quorum of validators agreed
- ✅ All signatures verified by multiple validators
- ✅ Block permanently committed to chain
- ✅ **Cannot be reversed** - transaction is final
- ✅ Safe to execute state transitions

## What Happens on Failure

### Invalid Signature on Submission
```rust
if transaction.verify() {
    mempool.add(transaction);
} else {
    warn!("rejected invalid transaction");  // ← Transaction dropped
}
```
**Result**: Transaction never enters mempool, never proposed

### Invalid Signature in Proposed Block
```rust
if !batcher.verify(&mut context) {
    let _ = response.send(false);  // ← Block rejected
    return;
}
```
**Result**: Entire block rejected, validator may be viewed as faulty

### Transaction Not Included
If mempool is full or block limit reached:
- Transaction remains in mempool
- Will be included in next proposal by any validator
- Eventually gets included (assuming valid)

## Summary

**The complete transaction workflow:**

1. ✅ **Create** transaction with signature
2. ✅ **Submit** to validator mailbox
3. ✅ **Verify** signature, add to mempool
4. ✅ **Wait** in mempool until proposal
5. ✅ **Include** in block (up to 100 per block)
6. ✅ **Broadcast** block to network
7. ✅ **Verify** signatures by all validators
8. ✅ **Vote** in consensus rounds
9. ✅ **Finalize** when quorum reached
10. ✅ **Confirmed** - transaction is permanent!

**Key insight**: Transactions go through the full Byzantine consensus process, ensuring they're cryptographically verified by multiple validators before being confirmed.

