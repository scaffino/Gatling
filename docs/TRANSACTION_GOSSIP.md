# Transaction Gossip in Alto

## Overview

When a validator receives a transaction via HTTP from a client, it now **broadcasts** that transaction to all other validators via P2P gossip. This ensures all validators have the transaction in their mempool and can include it in their block proposals.

## Architecture

### Flow Diagram

```
Client
  │
  │ HTTP POST /transaction
  ├────────────────────────────> Validator 0 (receives transaction)
  │                                    │
  │                                    ├─> Verify signature
  │                                    ├─> Add to local mempool
  │                                    │   LOG: "Transaction submitted to mempool via HTTP"
  │                                    │
  │                                    ├─> P2P Broadcast to all validators
  │                                    │   LOG: "Transaction broadcast to peers"
  │                                    │
  │       ┌────────────────────────────┼────────────────────────────┐
  │       │                            │                            │
  │       ▼                            ▼                            ▼
  │  Validator 1                  Validator 2                  Validator 3
  │       │                            │                            │
  │       ├─> Receive via P2P          ├─> Receive via P2P          ├─> Receive via P2P
  │       ├─> Decode transaction       ├─> Decode transaction       ├─> Decode transaction
  │       ├─> Add to mempool           ├─> Add to mempool           ├─> Add to mempool
  │       │   LOG: "Transaction        │   LOG: "Transaction        │   LOG: "Transaction
  │       │    received from peer"     │    received from peer"     │    received from peer"
  │       ▼                            ▼                            ▼
  │  Now ALL validators have the transaction in their mempool!
  │
  └─> Any validator can now include it when they propose a block
```

## Implementation Details

### Components

#### 1. **HTTP Server** (`chain/src/http_server.rs`)

When a transaction is received:
- Verifies the signature
- Adds to local mempool
- Sends to broadcast channel (tokio mpsc)

```rust
// Submit to local mempool
mailbox.submit_transaction(tx.clone()).await
// ↓
LOG: "Transaction submitted to mempool via HTTP tx_id=..."

// Send to broadcast task
broadcast_tx.unbounded_send(tx)
```

#### 2. **P2P Broadcast Task** (`chain/src/bin/validator.rs:292-309`)

Receives transactions from HTTP server and broadcasts via P2P:

```rust
while let Some(tx) = broadcast_rx.next().await {
    let tx_bytes = Bytes::from(tx.encode().to_vec());
    tx_sender.send(Recipients::All, tx_bytes, true).await
    // ↓
    LOG: "Transaction broadcast to peers tx_id=..."
}
```

**P2P Channel**: Channel 5 (`TRANSACTION_CHANNEL`)  
**Rate Limit**: 256 transactions/second per validator  
**Recipients**: `Recipients::All` (all connected validators)

#### 3. **P2P Receive Task** (`chain/src/bin/validator.rs:311-342`)

Listens for incoming transactions from peers:

```rust
loop {
    match tx_receiver.recv().await {
        Ok((peer, tx_bytes)) => {
            // Decode transaction
            let tx = Transaction::decode(tx_bytes.as_ref())
            
            // Add to local mempool
            mailbox.submit_transaction(tx).await
            // ↓
            LOG: "Transaction received from peer and added to mempool tx_id=..."
        }
    }
}
```

### Channel Configuration

```rust
const TRANSACTION_CHANNEL: u32 = 5;

// Rate limiting
let transaction_limit = Quota::per_second(NonZeroU32::new(256).unwrap());

// Channel registration
let (tx_sender, tx_receiver) = network.register(
    TRANSACTION_CHANNEL, 
    transaction_limit, 
    message_backlog
);
```

## Log Messages

### On Validator Receiving HTTP Transaction

```
INFO alto_chain::http_server: Transaction submitted to mempool via HTTP tx_id=0xabc...
INFO alto_chain::application::actor: Transaction broadcast to peers tx_id=0xabc...
```

### On Other Validators Receiving Gossip

```
INFO alto_chain::application::actor: Transaction received from peer and added to mempool tx_id=0xabc...
```

### Complete Sequence (4 Validators)

```
# Validator 0 (receives HTTP)
INFO: Transaction submitted to mempool via HTTP tx_id=0xabc...
INFO: Transaction broadcast to peers tx_id=0xabc...

# Validator 1 (receives gossip)
INFO: Transaction received from peer and added to mempool tx_id=0xabc...

# Validator 2 (receives gossip)
INFO: Transaction received from peer and added to mempool tx_id=0xabc...

# Validator 3 (receives gossip)
INFO: Transaction received from peer and added to mempool tx_id=0xabc...
```

## Benefits

### ✅ Fault Tolerance
- Transaction survives even if the receiving validator crashes
- Any validator can propose the transaction in their block

### ✅ Load Distribution
- All validators have the same transaction pool
- Block proposals can include any transaction from the global mempool

### ✅ Fairness
- Every validator has equal opportunity to include transactions
- No single validator has monopoly on transaction ordering

### ✅ Redundancy
- Even if one validator's mempool is full, others may have space
- Multiple validators can propose blocks with the same transactions

## Deduplication

### Mempool Handles Duplicates

The mempool automatically handles duplicate transactions:

```rust
// In mempool.rs
pub fn add(&mut self, tx: Transaction) {
    let digest = tx.digest();
    if self.transactions.contains_key(&digest) {
        return;  // ← Already have this transaction
    }
    // ... add transaction
}
```

**Result**: If the same transaction is submitted to multiple validators (or gossipped multiple times), each validator's mempool will only store it once.

## Performance

### Network Overhead

For N validators and 1 transaction:
- **HTTP submission**: 1 request (client → validator)
- **P2P gossip**: N-1 messages (validator → other validators)
- **Total messages**: N

**Example (4 validators)**:
- Client → Validator 0: 1 message
- Validator 0 → Validators 1,2,3: 3 messages
- **Total**: 4 messages

### Rate Limiting

Each validator can receive up to **256 transactions/second** from peers via gossip.

### Message Size

Each gossip message contains:
- Transaction: ~144 bytes
  - sender: 32 bytes
  - receiver: 32 bytes
  - amount: 8 bytes
  - timestamp: 8 bytes
  - signature: 64 bytes

## Example Usage

### Single Submission, All Validators Get It

```bash
# Submit to validator 0 only
cargo run --release --bin submit_tx -- \
  --validator http://localhost:8081 \
  --sender-seed 100 \
  --receiver <RECEIVER_HEX> \
  --amount 1000
```

**Result**:
- Validator 0: Receives via HTTP, broadcasts to others
- Validators 1, 2, 3: Receive via P2P gossip
- **All 4 validators** now have the transaction in mempool
- **Any validator** can include it when they propose

### Verify Gossip Worked

Check **all 4 validator terminals**:

```bash
# Validator 0 (received via HTTP)
INFO: Transaction submitted to mempool via HTTP tx_id=0xabc...
INFO: Transaction broadcast to peers tx_id=0xabc...

# Validators 1, 2, 3 (received via gossip)
INFO: Transaction received from peer and added to mempool tx_id=0xabc...
```

### Which Validator Includes It?

Watch the logs to see which validator proposes the block:

```bash
# Could be any validator (whoever's turn it is to propose)
INFO: Transaction included in block tx_id=0xabc... block_height=42
INFO: proposed new block ... txs=1 block_height=42
```

## Comparison: Before vs After

### Before (No Gossip)

```
Client → Validator 0
         ↓
    Validator 0's mempool only
         ↓
    Only Validator 0 can propose blocks with this transaction
         ↓
    If Validator 0 crashes, transaction is lost
```

### After (With Gossip)

```
Client → Validator 0
         ↓
    Validator 0's mempool
         ↓
    P2P Broadcast
         ↓
    All 4 validators' mempools
         ↓
    Any validator can propose blocks with this transaction
         ↓
    Transaction survives validator crashes
```

## Security Considerations

### Signature Verification

Transactions are verified **before** being added to mempool:

1. **HTTP submission**: Verified by receiving validator
2. **P2P gossip**: Verified by application actor before mempool insertion

Invalid transactions are rejected and not propagated further.

### Rate Limiting

Each P2P channel has rate limiting to prevent spam:
- 256 transactions/second per validator
- Prevents malicious flooding of the network

### Deduplication

The mempool's built-in deduplication (using transaction digest) prevents:
- Storage bloat from duplicate transactions
- Wasted processing on the same transaction

## Implementation Files

| File | Purpose | Key Functions |
|------|---------|---------------|
| `chain/src/bin/validator.rs:232-233` | Channel registration | `network.register(TRANSACTION_CHANNEL, ...)` |
| `chain/src/bin/validator.rs:276` | Tokio channel creation | `mpsc::unbounded_channel()` |
| `chain/src/bin/validator.rs:292-309` | P2P broadcast task | Receives from HTTP, broadcasts via P2P |
| `chain/src/bin/validator.rs:311-342` | P2P receive task | Receives from P2P, adds to mempool |
| `chain/src/http_server.rs:54-57` | HTTP → broadcast | Sends tx to broadcast channel |
| `chain/src/application/mempool.rs:56-102` | Deduplication | Checks digest before adding |

## Monitoring

### Check Transaction Propagation

Submit a transaction and grep all validator logs:

```bash
# Submit transaction
TX_ID=$(cargo run --release --bin submit_tx -- \
  --validator http://localhost:8081 \
  --sender-seed 100 \
  --receiver <RECEIVER> \
  --amount 1000 2>&1 | grep "Digest:" | awk '{print $2}')

# Check all validators (run in 4 terminals)
# Terminal 1
cargo run --release --bin validator -- ... 2>&1 | grep "$TX_ID"

# Terminal 2
cargo run --release --bin validator -- ... 2>&1 | grep "$TX_ID"

# Terminal 3
cargo run --release --bin validator -- ... 2>&1 | grep "$TX_ID"

# Terminal 4
cargo run --release --bin validator -- ... 2>&1 | grep "$TX_ID"
```

You should see the transaction logged in **all 4 validators**.

## Troubleshooting

### Transaction Not Gossiped

**Symptom**: Transaction only appears in receiving validator's logs

**Check**:
1. P2P network is connected: `grep "started network" <validator_log>`
2. Transaction channel is registered: `grep "TRANSACTION_CHANNEL" <source_code>`
3. No errors in broadcast: `grep "broadcast to peers" <validator_0_log>`

### Duplicate Transaction Warnings

**Symptom**: Mempool rejects transaction

**Expected behavior**: This is normal! The mempool deduplicates by digest.

### Gossip Too Slow

**Check**:
- Network latency between validators
- Rate limiting (256 tx/s should be sufficient for most cases)
- Message backlog size in config

## Future Enhancements

Potential improvements to transaction gossip:

1. **Selective gossip**: Only gossip to validators likely to propose next
2. **Bloom filters**: Reduce redundant gossip using probabilistic filters
3. **Priority gossip**: Higher-fee transactions propagated faster
4. **Adaptive rate limiting**: Adjust based on network conditions

## Related Documentation

- `TRANSACTION_FLOW.md` - Complete transaction workflow (now includes gossip)
- `TRANSACTION_LOGGING.md` - All log messages including gossip logs
- `SUBMIT_TX_CLIENT.md` - How to submit transactions

