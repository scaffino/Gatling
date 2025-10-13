# Transaction Logging in Alto Validators

This document explains the transaction lifecycle logging in alto validators.

## Transaction Identifier

Each transaction has a unique identifier (`tx_id`) computed as the SHA-256 digest of the transaction. This identifier is used consistently across all log messages to track the transaction's journey through the system.

## Log Messages

### 1. Transaction Submission

**When**: Transaction is received via HTTP and added to mempool  
**Where**: HTTP server (`chain/src/http_server.rs`)

```
INFO alto_chain::http_server: Transaction submitted to mempool via HTTP tx_id=<DIGEST>
```

**Example**:
```
INFO alto_chain::http_server: Transaction submitted to mempool via HTTP tx_id=0x1a2b3c4d...
```

### 2. Transaction Included in Block

**When**: Validator proposes a block that includes the transaction  
**Where**: Application actor (`chain/src/application/actor.rs`)

```
INFO alto_chain::application::actor: Transaction included in block engine_id=<ENGINE> tx_id=<DIGEST> block_height=<HEIGHT>
```

**Example**:
```
INFO alto_chain::application::actor: Transaction included in block engine_id="engine_1" tx_id=0x1a2b3c4d... block_height=42
```

**Note**: This log appears when the block is **proposed**, not yet finalized.

### 3. Transaction is Final

**When**: Block containing the transaction is finalized by consensus  
**Where**: Application actor (`chain/src/application/actor.rs`)

```
INFO alto_chain::application::actor: Transaction is now final engine_id=<ENGINE> tx_id=<DIGEST> block_height=<HEIGHT>
```

**Example**:
```
INFO alto_chain::application::actor: Transaction is now final engine_id="engine_1" tx_id=0x1a2b3c4d... block_height=42
```

**Note**: This is when the transaction becomes **permanent and irreversible**.

## Complete Transaction Lifecycle Example

Here's what you'll see in the validator logs for a transaction:

```
# Step 1: Client submits transaction via HTTP
2025-10-13T10:30:15.123Z INFO alto_chain::http_server: Transaction submitted to mempool via HTTP tx_id=0x1a2b3c4d5e6f7890...

# Step 2: Validator proposes block including transaction
2025-10-13T10:30:16.234Z INFO alto_chain::application::actor: Transaction included in block engine_id="engine_1" tx_id=0x1a2b3c4d5e6f7890... block_height=42

# Step 3: Block proposal log
2025-10-13T10:30:16.235Z INFO alto_chain::application::actor: proposed new block engine_id="engine_1" view=100 digest=0xaabbccdd... txs=1 block_height=42 success=true

# Step 4: Block is finalized
2025-10-13T10:30:17.456Z INFO alto_chain::application::actor: Transaction is now final engine_id="engine_1" tx_id=0x1a2b3c4d5e6f7890... block_height=42

# Step 5: Block finalization log
2025-10-13T10:30:17.457Z INFO alto_chain::application::actor: processed block engine_id="engine_1" height=42 digest=0xaabbccdd... txs=1
```

## Timeline

```
Client               Validator 0              Validators 1-3            Consensus
  |                       |                           |                      |
  |--HTTP POST----------->|                           |                      |
  |  (tx submission)      |                           |                      |
  |                       |                           |                      |
  |                [LOG: Tx submitted]                |                      |
  |                       |                           |                      |
  |                  Add to Mempool                   |                      |
  |                       |                           |                      |
  |                  (wait for turn)                  |                      |
  |                       |                           |                      |
  |                       |<------Propose-------------|                      |
  |                       |                           |                      |
  |                [LOG: Tx included in block]        |                      |
  |                       |                           |                      |
  |                       |---Block Proposal--------->|                      |
  |                       |                           |                      |
  |                       |                      [Verify]                    |
  |                       |                           |                      |
  |                       |                           |---Vote-------------->|
  |                       |                           |                      |
  |                       |                           |              [Consensus]
  |                       |                           |                      |
  |                       |<-----Finalized----------------------|            |
  |                       |                           |                      |
  |                [LOG: Tx is now final]             |                      |
  |                       |                           |                      |
  |                [LOG: processed block]             |                      |
```

## Filtering Logs

### Show Only Transaction-Related Logs

```bash
# All transaction lifecycle events
cargo run --release --bin validator -- --config <CONFIG> 2>&1 | grep -E "(Transaction|tx_id)"

# Only submission events
cargo run --release --bin validator -- --config <CONFIG> 2>&1 | grep "submitted to mempool"

# Only inclusion events
cargo run --release --bin validator -- --config <CONFIG> 2>&1 | grep "included in block"

# Only finalization events
cargo run --release --bin validator -- --config <CONFIG> 2>&1 | grep "is now final"
```

### Track a Specific Transaction

If you know the transaction ID (from client output):

```bash
# Track specific transaction
cargo run --release --bin validator -- --config <CONFIG> 2>&1 | grep "tx_id=0x1a2b3c4d"
```

## Transaction States

Based on the logs, you can determine the transaction state:

| State | Log Indicator | Meaning |
|-------|---------------|---------|
| **Pending** | "submitted to mempool" | Waiting to be included in a block |
| **Proposed** | "included in block" | Included in a proposed block, not yet final |
| **Finalized** | "is now final" | Permanently committed, irreversible |

## Getting Transaction ID from Client

When using the `submit_tx` client, the transaction digest is displayed:

```bash
./target/release/submit_tx \
  --validator http://localhost:8081 \
  --sender-seed 100 \
  --receiver <RECEIVER_HEX> \
  --amount 1000
```

**Output**:
```
=== Creating Transaction ===
From:   <sender_hex>
To:     <receiver_hex>
Amount: 1000
Digest: 0x1a2b3c4d5e6f7890...  ← This is the tx_id
```

Copy the `Digest` value to track this transaction in validator logs.

## Latency Calculation

You can calculate transaction latency by comparing timestamps:

1. **Submission Time**: Timestamp of "submitted to mempool" log
2. **Inclusion Time**: Timestamp of "included in block" log
3. **Finalization Time**: Timestamp of "is now final" log

**Typical latencies**:
- Submission → Inclusion: Variable (depends on validator rotation, can be 0ms to several seconds)
- Inclusion → Finalization: 100-2000ms (depends on consensus rounds)
- **Total (Submission → Finalization)**: 100ms - 3000ms

## Multiple Transactions

When multiple transactions are in a block, you'll see multiple log entries:

```
INFO alto_chain::application::actor: Transaction included in block ... tx_id=0xaaa... block_height=42
INFO alto_chain::application::actor: Transaction included in block ... tx_id=0xbbb... block_height=42
INFO alto_chain::application::actor: Transaction included in block ... tx_id=0xccc... block_height=42
INFO alto_chain::application::actor: proposed new block ... txs=3 block_height=42
```

And later:

```
INFO alto_chain::application::actor: Transaction is now final ... tx_id=0xaaa... block_height=42
INFO alto_chain::application::actor: Transaction is now final ... tx_id=0xbbb... block_height=42
INFO alto_chain::application::actor: Transaction is now final ... tx_id=0xccc... block_height=42
INFO alto_chain::application::actor: processed block height=42 ... txs=3
```

## Log Levels

All transaction logs use the `INFO` level by default. To see additional details:

- **DEBUG**: Mempool operations, detailed verification
- **TRACE**: Individual signature checks, message passing

Change log level in validator config:
```yaml
log_level: debug  # or trace for maximum verbosity
```

## Implementation Details

### Transaction Digest Calculation

```rust
use commonware_cryptography::Digestible;

let tx_id = transaction.digest();  // SHA-256 hash
```

The digest includes:
- Sender public key
- Receiver public key
- Amount
- Timestamp
- Signature

### Where Logs Are Generated

1. **Submission**: `chain/src/http_server.rs:50`
   ```rust
   info!(tx_id = ?tx_id, "Transaction submitted to mempool via HTTP");
   ```

2. **Inclusion**: `chain/src/application/actor.rs:149-152`
   ```rust
   for tx in &transactions {
       let tx_id = tx.digest();
       info!(engine_id=%engine_id, tx_id = ?tx_id, block_height, "Transaction included in block");
   }
   ```

3. **Finalization**: `chain/src/application/actor.rs:275-278`
   ```rust
   for tx in &block.transactions {
       let tx_id = tx.digest();
       info!(engine_id=%engine_id, tx_id = ?tx_id, block_height = block.height, "Transaction is now final");
   }
   ```

## Use Cases

### Monitoring Transaction Throughput

```bash
# Count finalized transactions per minute
cargo run --release --bin validator -- --config <CONFIG> 2>&1 | \
  grep "is now final" | \
  awk '{print $1}' | \
  cut -d'T' -f2 | \
  cut -d':' -f1-2 | \
  uniq -c
```

### Verify Transaction Inclusion

```bash
# Check if specific transaction was finalized
TX_ID="0x1a2b3c4d..."
cargo run --release --bin validator -- --config <CONFIG> 2>&1 | \
  grep "$TX_ID" | \
  grep "is now final"
```

### Measure Confirmation Latency

The test suite (`chain/tests/transaction_test.rs`) demonstrates automated latency measurement using these logs.

## Related Documentation

- `TRANSACTION_FLOW.md` - Complete transaction workflow
- `client/QUICK_START.md` - How to submit transactions
- `client/README_SUBMIT_TX.md` - Client API reference

