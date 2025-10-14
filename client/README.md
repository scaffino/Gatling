# alto-client

[![Crates.io](https://img.shields.io/crates/v/alto-client.svg)](https://crates.io/crates/alto-client)
[![Docs.rs](https://docs.rs/alto-client/badge.svg)](https://docs.rs/alto-client)

Client library and transaction submission tool for interacting with alto validators.

## Status

`alto-client` is **ALPHA** software and is not yet recommended for production use. Developers should expect breaking changes and occasional instability.

## Quick Start

### Prerequisites

- Rust toolchain installed
- 4 running alto validators (local or remote)

### 1. Build Client

```bash
cd /path/to/alto
cargo build --release --bin submit_tx
```

Binary location: `target/release/submit_tx`

### 2. Setup Validators (Local)

Generate validator configurations:

```bash
cd chain
cargo run --release --bin setup -- generate \
  --peers 4 --bootstrappers 1 --worker-threads 3 \
  --log-level info --message-backlog 16384 \
  --mailbox-size 16384 --deque-size 10 \
  --output test local --start-port 3000
```

This creates 4 validators with transaction ports **8081-8084** (auto-generated).

Start validators using the commands printed by setup (in separate terminals):

```bash
cargo run --release --bin validator -- \
  --peers test/peers.yaml \
  --config test/<validator_hash>.yaml
```

Wait for startup logs:
```
INFO alto_chain::http_server: Starting transaction HTTP server addr=0.0.0.0:8081
INFO commonware_p2p: started network
```

### 3. Submit Transaction

Get a receiver public key (use any seed):

```bash
./target/release/submit_tx \
  --validator http://localhost:8081 \
  --sender-seed 999 \
  --receiver 0000000000000000000000000000000000000000000000000000000000000000 \
  --amount 1 2>&1 | grep "Sender public key" | awk '{print $4}'
```

Submit your transaction:

```bash
./target/release/submit_tx \
  --validator http://localhost:8081 \
  --sender-seed 100 \
  --receiver <RECEIVER_PUBLIC_KEY_HEX> \
  --amount 1000
```

**Expected output:**
```
✓ Transaction accepted by validator!
Transaction has been submitted to the mempool.
It will be included in an upcoming block.
```

### 4. Verify in Logs

Check validator terminal for:

```
INFO alto_chain::http_server: Transaction submitted to mempool via HTTP tx_id=0x...
INFO alto_chain::application::actor: Transaction included in block tx_id=0x... block_height=42
INFO alto_chain::application::actor: Transaction is now final tx_id=0x... block_height=42
INFO alto_chain::application::actor: finalized block height=42 txs=1
```

## Usage

### Command Structure

**Single validator:**
```bash
./target/release/submit_tx \
  --validator <VALIDATOR_URL> \
  --sender-seed <SEED> \
  --receiver <PUBLIC_KEY_HEX> \
  --amount <AMOUNT>
```

**All validators (ports 8081-8084):**
```bash
./target/release/submit_tx \
  --validator-all \
  --sender-seed <SEED> \
  --receiver <PUBLIC_KEY_HEX> \
  --amount <AMOUNT>
```

**Parameters:**
- `--validator`: Validator endpoint (e.g., `http://localhost:8081`) - mutually exclusive with `--validator-all`
- `--validator-all`: Submit to all validators (ports 8081-8084) - mutually exclusive with `--validator`
- `--sender-seed`: u64 seed for sender's private key (any number)
- `--receiver`: Receiver's public key (64-char hex)
- `--amount`: Amount to send (u64)

### Examples

**Submit to all validators at once:**
```bash
./target/release/submit_tx \
  --validator-all \
  --sender-seed 100 \
  --receiver <RECEIVER_HEX> \
  --amount 1000
```

**Multiple transactions to all validators:**
```bash
for i in {1..5}; do
  ./target/release/submit_tx \
    --validator-all \
    --sender-seed $i \
    --receiver <RECEIVER_HEX> \
    --amount $((i * 100))
done
```

**Different validators (single):**
```bash
# Validator 0 (port 8081)
./target/release/submit_tx --validator http://localhost:8081 ...

# Validator 1 (port 8082)
./target/release/submit_tx --validator http://localhost:8082 ...
```

## Transaction Lifecycle

```
1. HTTP POST → Validator receives transaction
2. Signature Verification → Added to local mempool
3. P2P Gossip → Transaction broadcast to all validators
4. All validators add transaction to their mempools
5. Block Proposal → Any validator can include transaction
6. Consensus Voting → Validators verify and vote
7. Finalization → Transaction confirmed (irreversible)
```

**Typical latency:** 100-2000ms

**Note:** With P2P gossip, you only need to submit to **one** validator, and all validators will receive it!

## Transaction Logging

Validators log four key events:

| Event | Log Message | Meaning |
|-------|-------------|---------|
| **Submission (HTTP)** | `Transaction submitted to mempool via HTTP` | Received via HTTP |
| **Gossip Broadcast** | `Transaction broadcast to peers` | Sent to other validators |
| **Gossip Received** | `Transaction received from peer and added to mempool` | Received from another validator |
| **Inclusion** | `Transaction included in block` | Proposed in a block |
| **Finalization** | `Transaction is now final` | Permanently confirmed ✅ |

Each log includes the transaction ID (`tx_id`) for tracking.

**Note:** When you submit to one validator, you'll see:
- **Validator 0**: "submitted via HTTP" + "broadcast to peers"
- **Validators 1-3**: "received from peer" (via gossip)

## Troubleshooting

### Connection Refused
- Check validator is running: `ps aux | grep validator`
- Verify `transaction_port` in config file
- Look for "Starting transaction HTTP server" in logs

### Invalid Signature
- Ensure receiver public key is valid 64-char hex
- Regenerate receiver key using the method above

### Transaction Not Included
- Check mempool: `grep "added to mempool" <validator_log>`
- Check proposals: `grep "proposed new block" <validator_log>`
- Mempool limits: 32,768 total, 16 per sender, 100 per block
- Wait a few blocks before concluding

## HTTP API

**Endpoint:** `POST http://<validator>:<transaction_port>/transaction`

**Content-Type:** `application/octet-stream`

**Body:** Binary encoded transaction (commonware_codec::Encode)

**Responses:**
- `200 OK` - Transaction accepted
- `400 Bad Request` - Invalid encoding or signature
- `500 Internal Server Error` - Submission failed

## Transaction Structure

```rust
pub struct Transaction {
    pub sender: PublicKey,      // 32 bytes
    pub receiver: PublicKey,    // 32 bytes
    pub amount: u64,            // 8 bytes
    pub timestamp: u64,         // Auto-generated
    pub signature: Signature,   // 64 bytes (Ed25519)
}
```

**Security:**
- Ed25519 digital signatures
- Verified twice: on submission and block verification
- Batch verification for efficiency
- Replay protection via timestamps

## Validator Configuration

The setup script automatically generates `transaction_port` in validator configs:

```yaml
port: 3000
metrics_port: 3001
transaction_port: 8081  # Auto-generated
```

**Port allocation:**
- Validator 0: 8081
- Validator 1: 8082
- Validator 2: 8083
- Validator 3: 8084

## Cleanup

Stop validators with `Ctrl+C`.

To reset and start fresh:

```bash
cd chain
rm -rf test/
# Re-run setup command from step 2
```

## Further Documentation

- `../docs/TRANSACTION_FLOW.md` - Complete transaction workflow
- `../docs/TRANSACTION_LOGGING.md` - Detailed logging reference
- `../docs/SETUP_CHANGES.md` - Configuration details

## Library Usage

The client library provides programmatic access to alto validators:

```rust
use alto_client::Client;
use alto_types::{Identity, Transaction};

let client = Client::new("http://localhost:8081", identity);

// Query consensus
let evaluation = client.consensus_evaluation().await?;

// Submit transaction (requires HTTP endpoint on validator)
client.transaction_submit(transaction).await?;
```

See [docs.rs](https://docs.rs/alto-client) for full API documentation.
