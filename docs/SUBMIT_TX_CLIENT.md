# Transaction Submission Client

This document explains how to use the `submit_tx` client to send transactions to an alto validator.

## Prerequisites

1. **Running validators**: You need at least one alto validator running locally or remotely
2. **Validator configuration**: Each validator must have the `transaction_port` configured (default: 8081-8084)
3. **Recipient public key**: You need the public key of the transaction receiver

## Building the Client

**Option 1: Build only**

```bash
cargo build --bin submit_tx --release
```

The compiled binary will be at: `target/release/submit_tx`

**Option 2: Build and run in one command**

Use `cargo run` to automatically build (if needed) and run:

```bash
cargo run --release --bin submit_tx -- <ARGS>
```

The `--` separates cargo arguments from your program arguments.

## Usage

### Basic Command Structure

**Using pre-built binary:**
```bash
./target/release/submit_tx \
  --validator <VALIDATOR_URL> \
  --sender-seed <SEED> \
  --receiver <PUBLIC_KEY_HEX> \
  --amount <AMOUNT>
```

**Using cargo run (builds automatically):**
```bash
cargo run --release --bin submit_tx -- \
  --validator <VALIDATOR_URL> \
  --sender-seed <SEED> \
  --receiver <PUBLIC_KEY_HEX> \
  --amount <AMOUNT>
```

### Parameters

- `--validator`: Full URL of the validator's transaction endpoint (e.g., `http://localhost:8081`)
- `--sender-seed`: A u64 seed to generate the sender's private key (any number)
- `--receiver`: The receiver's public key in hexadecimal format
- `--amount`: The amount to send (u64)

## Complete Example

### Step 1: Start Local Validators

First, run the setup to create the validator configuration:

```bash
cd alto/chain
cargo run --release --bin setup
```

This creates configuration files in `test/` directory with:
- Validator 0: transaction_port 8081
- Validator 1: transaction_port 8082
- Validator 2: transaction_port 8083
- Validator 3: transaction_port 8084

Start all validators in separate terminals:

```bash
# Terminal 1
cargo run --release --bin validator -- --config test/2b7f1b6fdceb1c823e9de9bd0957a5aaca9606f6e27af80a11c685a662164230.yaml

# Terminal 2
cargo run --release --bin validator -- --config test/56cd9d916a88d3494a18fb1fa7c0e6c1c03d2d9176465b0d5712510dc9702fd7.yaml

# Terminal 3
cargo run --release --bin validator -- --config test/b20aae220457a827de65bae6500e591a4000fe8b424342a1045b540704904085.yaml

# Terminal 4
cargo run --release --bin validator -- --config test/dcd7f6c76db2f563b4145beab5234dcb8fa8bf6bdef1c977dbf44f6eb3d6c720.yaml
```

Wait for validators to start and connect. You should see logs like:
```
Starting transaction HTTP server addr=0.0.0.0:8081
```

### Step 2: Get Receiver Public Key

You need a receiver's public key. You can:

**Option A**: Use a validator's public key (from the test config):
```bash
# Extract from config (this is validator 0's key)
echo "45977379db30dcdcbb33d5c05c65c9c182428f735e9ace9f92224517ad1dd0ee" | xxd -r -p | xxd -p -c 32
```

**Option B**: Generate from a seed using the submit_tx client (it prints the sender's public key):
```bash
./target/release/submit_tx \
  --validator http://localhost:8081 \
  --sender-seed 999 \
  --receiver 0000000000000000000000000000000000000000000000000000000000000000 \
  --amount 1 2>&1 | grep "Sender public key"
```

This will fail (invalid receiver) but print the sender's public key that you can use.

### Step 3: Submit Transaction

Once you have a valid receiver public key, submit a transaction:

**Using pre-built binary:**
```bash
./target/release/submit_tx \
  --validator http://localhost:8081 \
  --sender-seed 100 \
  --receiver 1c5e996c515199048cf970a75f81d78d9b4bda4ad759abfce22e8e0d5c881cfa \
  --amount 1000
```

**Or using cargo run:**
```bash
cargo run --release --bin submit_tx -- \
  --validator http://localhost:8081 \
  --sender-seed 100 \
  --receiver 1c5e996c515199048cf970a75f81d78d9b4bda4ad759abfce22e8e0d5c881cfa \
  --amount 1000
```

Expected output:
```
Sender public key: <hex_of_sender_public_key>

=== Creating Transaction ===
From:   <sender_hex>
To:     <receiver_hex>
Amount: 1000
Digest: <transaction_digest>

=== Submitting to Validator ===
URL: http://localhost:8081/transaction
✓ Transaction accepted by validator!

Transaction has been submitted to the mempool.
It will be included in an upcoming block.
```

### Step 4: Verify Inclusion

The transaction will be included in the next block proposed by a validator. Check the validator logs to see:

```
transaction added to mempool
proposed new block txs=1
```

And when finalized:
```
processed block height=X digest=<block_digest>
```

## Advanced Usage

### Submit to Different Validators

You can submit to any running validator by changing the port:

```bash
# Submit to validator 1 (port 8082)
./target/release/submit_tx \
  --validator http://localhost:8082 \
  --sender-seed 100 \
  --receiver 1c5e996c515199048cf970a75f81d78d9b4bda4ad759abfce22e8e0d5c881cfa \
  --amount 2000
```

### Send Multiple Transactions

You can send multiple transactions from the same sender by running the command multiple times:

**Using pre-built binary:**
```bash
for i in {1..10}; do
  ./target/release/submit_tx \
    --validator http://localhost:8081 \
    --sender-seed 100 \
    --receiver 1c5e996c515199048cf970a75f81d78d9b4bda4ad759abfce22e8e0d5c881cfa \
    --amount $((i * 100))
  sleep 0.1
done
```

**Or using cargo run:**
```bash
for i in {1..10}; do
  cargo run --release --bin submit_tx -- \
    --validator http://localhost:8081 \
    --sender-seed 100 \
    --receiver 1c5e996c515199048cf970a75f81d78d9b4bda4ad759abfce22e8e0d5c881cfa \
    --amount $((i * 100))
  sleep 0.1
done
```

### Different Senders

Use different seeds to create transactions from different senders:

```bash
# Sender 1
./target/release/submit_tx \
  --validator http://localhost:8081 \
  --sender-seed 100 \
  --receiver <receiver_hex> \
  --amount 1000

# Sender 2
./target/release/submit_tx \
  --validator http://localhost:8081 \
  --sender-seed 200 \
  --receiver <receiver_hex> \
  --amount 2000
```

## Transaction Workflow

Once submitted, the transaction goes through these stages:

1. **HTTP POST**: Client sends transaction to validator's `/transaction` endpoint
2. **Signature Verification**: Validator verifies the transaction signature
3. **Mempool**: Valid transactions are added to the validator's mempool
4. **Block Proposal**: When it's a validator's turn to propose, it includes transactions from mempool
5. **Verification**: Other validators verify the block and transaction signatures
6. **Consensus**: Validators vote on the block
7. **Finalization**: Once quorum is reached, the block (and transactions) are finalized

**Typical latency**: 100ms - 2000ms depending on network conditions and consensus timing

## Troubleshooting

### Transaction Rejected
```
✗ Transaction rejected: 400
Response: Invalid signature
```
**Cause**: Transaction signature verification failed
**Solution**: Ensure you're using a valid seed and receiver public key

### Connection Refused
```
Failed to send request
```
**Cause**: Validator not running or wrong port
**Solution**: 
- Check validator is running: `ps aux | grep validator`
- Verify transaction_port in config file
- Ensure validator HTTP server started (check logs)

### Transaction Not Included

If the transaction is accepted but not included in blocks:

1. **Check mempool**: Look for "transaction added to mempool" in validator logs
2. **Check proposals**: Look for "proposed new block txs=N" to see if transactions are being included
3. **Check capacity**: Mempool has limits (32,768 total, 16 per sender)
4. **Wait longer**: It may take a few blocks before your transaction is included

### Getting Receiver Public Keys

To extract public keys from running validators:

```bash
# From config file (use the private_key and derive the public key)
# Or use a simple Rust script:
cat <<'EOF' > get_pubkey.rs
use commonware_cryptography::{ed25519::PrivateKey, PrivateKeyExt};
fn main() {
    let seed: u64 = std::env::args().nth(1).unwrap().parse().unwrap();
    let key = PrivateKey::from_seed(seed);
    println!("{}", hex::encode(key.public_key().as_ref()));
}
EOF

# Build and run
rustc get_pubkey.rs --extern commonware_cryptography --extern hex
./get_pubkey 100
```

## HTTP API Reference

### POST /transaction

**Endpoint**: `http://<validator>:<transaction_port>/transaction`

**Content-Type**: `application/octet-stream`

**Body**: Binary encoded transaction (using commonware_codec::Encode)

**Responses**:
- `200 OK`: Transaction accepted and added to mempool
- `400 Bad Request`: Invalid transaction encoding or signature
- `500 Internal Server Error`: Failed to submit to mempool

## Implementation Details

### Transaction Structure

```rust
pub struct Transaction {
    pub sender: PublicKey,      // 32 bytes
    pub receiver: PublicKey,    // 32 bytes
    pub amount: u64,            // 8 bytes
    pub timestamp: u64,         // 8 bytes (generated automatically)
    pub signature: Signature,   // 64 bytes
}
```

### Signature Verification

Transactions are verified **twice**:
1. On HTTP submission (by receiving validator)
2. During block verification (by all validators using batch verification)

### Security

- **Ed25519 signatures**: Cryptographically secure digital signatures
- **Replay protection**: Timestamps prevent duplicate submissions
- **Batch verification**: Efficient verification of multiple transactions

## Next Steps

- **Monitor transactions**: Implement a block explorer to view transaction history
- **Transaction queries**: Query if a transaction was included in a block
- **Account balances**: Track balances based on transaction history
- **Transaction fees**: Implement fee mechanism (not currently supported)

### Command to send txs via client
./target/release/submit_tx --validator-all --sender-seed 999 --receiver 3ecf551aeb957616c6c8aa603634ea55845f88712a58745e58a71fe988bb967a --amount 17