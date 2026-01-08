# Transaction System Summary

## Quick Reference

### 3 Key Log Messages You'll See

1. **"Transaction submitted to mempool via HTTP"** → Transaction received ✅
2. **"Transaction included in block"** → Transaction proposed in a block 📦
3. **"Transaction is now final"** → Transaction permanently confirmed 🎉

### Example Output

When you submit a transaction, watch the validator terminal for:

```bash
# Step 1: Submission (instant)
INFO alto_chain::http_server: Transaction submitted to mempool via HTTP tx_id=0x1a2b3c4d...

# Step 2: Block proposal (0-2 seconds later)
INFO alto_chain::application::actor: Transaction included in block tx_id=0x1a2b3c4d... block_height=42

# Step 3: Finalization (0.1-2 seconds after proposal)
INFO alto_chain::application::actor: Transaction is now final tx_id=0x1a2b3c4d... block_height=42
```

## Running a Quick Test

### Terminal 1: Start Validator

```bash
cd chain

# Generate configs (first time only)
cargo run --release --bin setup -- generate \
  --peers 4 \
  --bootstrappers 1 \
  --worker-threads 3 \
  --log-level info \
  --message-backlog 16384 \
  --mailbox-size 16384 \
  --deque-size 10 \
  --output test \
  local --start-port 3000

# Start first validator (repeat for all 4 in separate terminals)
cargo run --release --bin validator -- \
  --peers test/peers.yaml \
  --config test/<validator_hash>.yaml
```

### Terminal 2: Submit Transaction

```bash
# Get a receiver public key (use any seed)
RECEIVER=$(./target/release/submit_tx \
  --validator http://localhost:8081 \
  --sender-seed 999 \
  --receiver 0000000000000000000000000000000000000000000000000000000000000000 \
  --amount 1 2>&1 | grep "Sender public key" | awk '{print $4}')

# Submit transaction
./target/release/submit_tx \
  --validator http://localhost:8081 \
  --sender-seed 100 \
  --receiver $RECEIVER \
  --amount 1000
```

### Watch Validator Logs

In Terminal 1 (validator), you should immediately see:

```
INFO alto_chain::http_server: Transaction submitted to mempool via HTTP tx_id=...
INFO alto_chain::application::actor: Transaction included in block ... block_height=...
INFO alto_chain::application::actor: Transaction is now final ... block_height=...
```

## Transaction Lifecycle

```
┌─────────────────────────────────────────────────────────────┐
│                    TRANSACTION LIFECYCLE                     │
└─────────────────────────────────────────────────────────────┘

Client sends transaction
        │
        ├─> [HTTP POST /transaction]
        │
        ▼
┌──────────────────┐
│ VALIDATOR        │
│  Receives TX     │  LOG: "Transaction submitted to mempool via HTTP"
│  Verifies Sig    │       tx_id = 0x1a2b3c4d...
│  Adds to Mempool │
└────────┬─────────┘
         │
         │ (waits for turn to propose)
         │
         ▼
┌──────────────────┐
│ VALIDATOR        │
│  Proposes Block  │  LOG: "Transaction included in block"
│  Includes TX     │       tx_id = 0x1a2b3c4d...
│                  │       block_height = 42
└────────┬─────────┘
         │
         ├─> [Broadcasts to other validators]
         │
         ▼
┌──────────────────┐
│ OTHER VALIDATORS │
│  Verify Block    │
│  Verify TX Sigs  │
│  Vote            │
└────────┬─────────┘
         │
         ├─> [Consensus rounds]
         │
         ▼
┌──────────────────┐
│ ALL VALIDATORS   │
│  Finalize Block  │  LOG: "Transaction is now final"
│  TX is Final ✅  │       tx_id = 0x1a2b3c4d...
│                  │       block_height = 42
└──────────────────┘
```

## Key Points

### Transaction Identifier

- **tx_id** = SHA-256 digest of transaction
- Shown in client output as "Digest"
- Used consistently in all log messages
- Unique identifier to track transaction

### States

| State | When | Log Message |
|-------|------|-------------|
| **Pending** | In mempool | "submitted to mempool" |
| **Proposed** | In proposed block | "included in block" |
| **Final** | Block finalized | "is now final" ✅ |

### Timing

- **Submission → Proposal**: 0-3 seconds (depends on validator turn)
- **Proposal → Finalization**: 0.1-2 seconds (consensus)
- **Total**: ~0.1-5 seconds typically

## Useful Commands

### Filter Transaction Logs Only

```bash
# See all transaction events
cargo run --release --bin validator -- --config <CONFIG> 2>&1 | grep tx_id

# See only finalizations
cargo run --release --bin validator -- --config <CONFIG> 2>&1 | grep "is now final"
```

### Track Specific Transaction

```bash
# Use the digest from client output
cargo run --release --bin validator -- --config <CONFIG> 2>&1 | grep "0x1a2b3c4d"
```

### Clean Output (Remove Color Codes)

```bash
cargo run --release --bin validator -- --config <CONFIG> 2>&1 | \
  sed 's/\x1b\[[0-9;]*m//g' | \
  grep tx_id
```

## Files Modified

- `chain/src/http_server.rs` - Added tx_id logging on submission
- `chain/src/application/actor.rs` - Added tx_id logging on proposal and finalization
- Created `TRANSACTION_LOGGING.md` - Detailed logging documentation

## What Each Validator Sees

### Proposing Validator (the one who includes the TX)

```
INFO: Transaction submitted to mempool via HTTP tx_id=0xaaa...
INFO: Transaction included in block tx_id=0xaaa... block_height=42
INFO: proposed new block ... txs=1 block_height=42
INFO: Transaction is now final tx_id=0xaaa... block_height=42
INFO: processed block height=42 txs=1
```

### Other Validators (validators 1-3)

```
# They don't see submission (TX was sent to validator 0)
INFO: Transaction is now final tx_id=0xaaa... block_height=42
INFO: processed block height=42 txs=1
```

**Note**: Only the validator that receives the HTTP submission logs "submitted to mempool". All validators log "is now final" when the block is finalized.

## Full Documentation

For complete details, see:
- `TRANSACTION_LOGGING.md` - All logging details
- `TRANSACTION_FLOW.md` - Complete workflow
- `client/QUICK_START.md` - Getting started guide
- `client/README_SUBMIT_TX.md` - Client usage

