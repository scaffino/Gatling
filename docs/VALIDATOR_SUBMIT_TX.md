# Validator Submit-TX Thread

This document explains how to use the validator's built-in `--submit-tx` feature to automatically generate and submit transactions to the validator's own mempool at a specified rate.

## Overview

The validator can automatically generate transactions using a background thread that:
- Starts generating transactions at a specified time after genesis
- Generates transactions at a specified rate (transactions per second)
- Runs for a specified duration
- Submits each transaction immediately to all consensus instances' mempools

This feature is useful for:
- **Load testing**: Generate consistent transaction load for performance evaluation
- **Benchmarking**: Measure system performance under controlled transaction rates
- **Evaluation**: Test consensus behavior with predictable transaction patterns

## Prerequisites

1. **Running validator**: The validator must be configured and ready to start
2. **Genesis timestamp**: The validator must have a valid `genesis_timestamp` in its config
3. **Consensus instances**: Works with any number of consensus instances (submits to all)

## Command-Line Usage

### Basic Syntax

```bash
cargo run --release --bin validator -- \
  --config <CONFIG_FILE> \
  --hosts <HOSTS_FILE> \
  --submit-tx <RATE> <START_DELAY> <DURATION>
```

### Parameters

- `--submit-tx`: Enable transaction generation (optional)
  - `RATE`: Transactions per second (f64, e.g., `1.0`, `10.5`, `100`)
  - `START_DELAY`: Seconds after genesis to start generating (u64, e.g., `0`, `10`, `60`)
  - `DURATION`: How long to generate transactions in seconds (u64, e.g., `30`, `300`, `3600`)

### Examples

**Generate 10 transactions per second, starting 5 seconds after genesis, for 60 seconds:**
```bash
cargo run --release --bin validator -- \
  --config test/validator.yaml \
  --hosts hosts.yaml \
  --submit-tx 10 5 60
```

**Generate 0.5 transactions per second (1 every 2 seconds), starting immediately, for 5 minutes:**
```bash
cargo run --release --bin validator -- \
  --config test/validator.yaml \
  --hosts hosts.yaml \
  --submit-tx 0.5 0 300
```

**High-rate generation: 100 transactions per second for 30 seconds, starting 10 seconds after genesis:**
```bash
cargo run --release --bin validator -- \
  --config test/validator.yaml \
  --hosts hosts.yaml \
  --submit-tx 100 10 30
```

## How It Works

### Transaction Generation

The submit-tx thread creates transactions with the following fixed parameters:
- **Sender**: Validator's own private key (from config)
- **Receiver**: Validator's own public key (self-transaction)
- **Amount**: Fixed value of 1
- **Timestamp**: Current Unix time in milliseconds when transaction is created

### Rate Control with Randomness

To avoid perfectly uniform spacing (which could cause synchronization issues), the implementation adds random jitter to inter-transaction delays:

- **Target interval**: `1.0 / RATE` seconds between transactions
- **Jitter**: Uniform random factor between 0.5x and 1.5x of target interval
- **Result**: Average rate matches target while maintaining natural variation

Example: For rate = 10 tx/sec:
- Target interval: 0.1 seconds
- Actual intervals: Random between 0.05s and 0.15s
- Average: ~0.1s (10 tx/sec)

### Submission Flow

1. **Wait for start time**: Thread waits until `genesis_timestamp + START_DELAY`
2. **Generate loop**: 
   - Calculate jittered sleep duration
   - Sleep for that duration
   - Create transaction with current timestamp
   - Submit to **all consensus instances** via their mailboxes
   - Repeat until duration expires
3. **Stop**: Thread stops at `genesis_timestamp + START_DELAY + DURATION`

### Multi-Instance Support

When running multiple consensus instances (via `--consensus-instances N`), the submit-tx thread submits each transaction to **all instances' mempools**. This ensures:
- All instances receive the same transactions
- Consistent load across all consensus instances
- Proper evaluation of multi-instance performance

## Complete Example

### Step 1: Start Validator with Submit-TX

```bash
cd chain

cargo run --release --bin validator -- \
  --config test/validator.yaml \
  --hosts hosts.yaml \
  --submit-tx 5 10 120
```

This will:
- Start the validator normally
- Wait until 10 seconds after genesis
- Generate transactions at 5 tx/sec
- Continue for 120 seconds (2 minutes)
- Submit all transactions to all consensus instances

### Step 2: Monitor Logs

You should see logs like:

```
INFO submit-tx: waiting until start time rate=5.0 start_delay=10 duration=120 wait_secs=8
INFO submit-tx: starting transaction generation rate=5.0 start_delay=10 duration=120
INFO submit-tx: generated transactions tx_count=100 submitted_count=1 total_mailboxes=1
INFO submit-tx: finished transaction generation tx_count=600 elapsed_secs=120
```

### Step 3: Verify Transactions

Check validator logs for:
- `transaction added to mempool` - Transactions being accepted
- `proposed new block txs=N` - Transactions being included in blocks
- `processed block height=X` - Blocks being finalized

## Timing Details

### Start Time Calculation

The start time is calculated as:
```
start_time = genesis_timestamp + START_DELAY
```

If the validator starts before this time, it will wait. If it starts after, generation begins immediately.

### Duration Control

The thread checks the current time before and after each sleep to ensure it stops exactly at:
```
end_time = genesis_timestamp + START_DELAY + DURATION
```

This ensures precise duration control even with variable sleep times due to jitter.

## Rate Considerations

### Low Rates (< 1 tx/sec)

For rates less than 1 transaction per second:
- Use fractional values: `0.5`, `0.1`, `0.25`
- Example: `--submit-tx 0.5 0 60` generates 1 transaction every 2 seconds

### Medium Rates (1-10 tx/sec)

Most common for testing:
- Provides steady load without overwhelming the system
- Good for evaluating normal operation
- Example: `--submit-tx 5 0 300` generates 1500 transactions over 5 minutes

### High Rates (> 10 tx/sec)

For stress testing:
- Can generate significant load
- Monitor system resources (CPU, memory, network)
- Example: `--submit-tx 100 0 30` generates 3000 transactions in 30 seconds

## Integration with Other Features

### Transaction Gossiping

If `--gossip-txs` is enabled (default), transactions generated by submit-tx will be:
1. Submitted to local mempools (all instances)
2. Broadcast to other validators via P2P
3. Received by other validators and added to their mempools

This allows load testing across the entire network.

### Multiple Consensus Instances

When using `--consensus-instances N`:
- Each transaction is submitted to all N instances
- All instances process transactions independently
- Useful for testing multi-instance consensus behavior

### Gatling Finalization

If `--gatling` is enabled, transactions will be tracked through the gatling finalization system, providing detailed logging of transaction finalization across all instances.

## Troubleshooting

### Transactions Not Appearing

**Symptom**: No transactions in mempool or blocks

**Possible causes**:
1. **Start delay too long**: Check if `START_DELAY` is reasonable relative to genesis
2. **Duration too short**: Verify `DURATION` allows enough time
3. **Rate too low**: Very low rates (< 0.1 tx/sec) may generate few transactions

**Solution**: Check logs for "submit-tx: starting transaction generation" and "submit-tx: finished transaction generation"

### Rate Mismatch

**Symptom**: Actual transaction rate doesn't match specified rate

**Possible causes**:
1. **System overload**: High rates may cause delays
2. **Network congestion**: P2P gossip may slow submission
3. **Mempool full**: Mempool limits (32,768 transactions) may reject transactions

**Solution**: 
- Monitor system resources
- Check mempool metrics
- Reduce rate if system is overloaded

### Timing Issues

**Symptom**: Transactions start at wrong time or duration is incorrect

**Possible causes**:
1. **Clock skew**: System clock not synchronized
2. **Genesis timestamp**: Incorrect genesis timestamp in config

**Solution**: 
- Verify system clock accuracy
- Check `genesis_timestamp` in config file
- Review logs for actual start/end times

## Implementation Details

### Code Location

The submit-tx implementation is in:
- `chain/src/bin/validator.rs`: Main validator binary
  - Argument parsing: Lines ~284-290
  - Config parsing: Lines ~320-337
  - Task spawning: Lines ~615-735
  - Task integration: Lines ~866-868

### Dependencies

- `rand`: For random jitter generation (already in Cargo.toml)
- `tokio::time`: For async sleep and timing
- `alto_types::Transaction`: Transaction type and signing

### Transaction Structure

Each generated transaction:
```rust
Transaction {
    sender: validator_private_key.public_key(),
    receiver: validator_public_key,  // Self-transaction
    amount: 1,
    timestamp: current_unix_time_ms,
    signature: ed25519_signature,
}
```

## Best Practices

1. **Start with low rates**: Begin testing with rates < 10 tx/sec to verify behavior
2. **Monitor resources**: Watch CPU, memory, and network usage during generation
3. **Use appropriate durations**: Short durations (30-60s) for quick tests, longer (300-600s) for sustained load
4. **Coordinate across validators**: If testing multiple validators, use different start delays to avoid synchronization
5. **Check logs**: Monitor submit-tx logs to verify expected behavior

## Limitations

1. **Fixed transaction parameters**: Sender, receiver, and amount are fixed (cannot be customized)
2. **Single rate per validator**: Only one submit-tx configuration per validator instance
3. **No dynamic rate changes**: Rate cannot be adjusted while validator is running
4. **Self-transactions only**: Transactions are always from validator to itself

## Future Enhancements

Potential improvements:
- Configurable sender/receiver/amount
- Multiple rate profiles (different rates at different times)
- Dynamic rate adjustment
- Transaction pattern customization
- File-based rate schedules

## See Also

- [SUBMIT_TX_CLIENT.md](./SUBMIT_TX_CLIENT.md): Manual transaction submission via client
- [TRANSACTION_FLOW.md](./TRANSACTION_FLOW.md): How transactions flow through the system
- [TRANSACTIONS_IMPLEMENTATION.md](./TRANSACTIONS_IMPLEMENTATION.md): Transaction implementation details
- [TRANSACTION_GOSSIP.md](./TRANSACTION_GOSSIP.md): Transaction gossiping between validators


