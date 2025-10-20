# Proposal Timing Feature

## Overview

This feature ensures that consensus proposals are sent at precise times within each second, with configurable offsets for multiple consensus instances. This allows for controlled timing and staggering of proposals across multiple instances.

## Implementation

### Key Changes

1. **Config Field**: Added `proposal_offset_ms: u64` to both `application::Config` and `engine::Config`
   - Specifies the millisecond offset within each second (0-999)
   - Example: `0` means X.000s, `500` means X.500s

2. **Timing Logic** (in `actor.rs`): Before sending a proposal, the system:
   - Gets the current time in milliseconds
   - Calculates the position within the current second (`current_ms % 1000`)
   - Determines how long to wait until the target offset
   - Sleeps until the precise time
   - Sends the proposal exactly at X.{offset}s

3. **Automatic Staggering**: When running multiple consensus instances, they are automatically distributed evenly across the second:
   - 2 instances: 0ms, 500ms
   - 3 instances: 0ms, 333ms, 666ms
   - 4 instances: 0ms, 250ms, 500ms, 750ms
   - Formula: `proposal_offset_ms = (instance_index * 1000 / num_instances)`

## Example Usage

### Single Instance
With one consensus instance, proposals will be sent at X.000s (default offset = 0ms).

### Multiple Instances
When running with `--consensus-instances 2`:
- Consensus instance 1: proposals at X.000s
- Consensus instance 2: proposals at X.500s

When running with `--consensus-instances 4`:
- Consensus instance 1: proposals at X.000s
- Consensus instance 2: proposals at X.250s
- Consensus instance 3: proposals at X.500s
- Consensus instance 4: proposals at X.750s

## Logging

The system logs:
1. At startup: The configured offset for each instance
   ```
   Consensus instance 1 will send proposals at offset 0 ms within each second
   Consensus instance 2 will send proposals at offset 500 ms within each second
   ```

2. Before each proposal: The wait time
   ```
   [consensus_1] Waiting 234 ms to send proposal at precise time (offset: 500 ms)
   ```

3. After sending: Confirmation of the proposal
   ```
   [consensus_1] Validator 0 proposed block 5 (view 3) with 10 transactions
   ```

## Technical Details

- **Time Source**: Uses `context.current().epoch_millis()` for consistent timing
- **Sleep Function**: Uses `context.sleep(Duration::from_millis(wait_ms))` for precise delays
- **Calculation**: Handles wraparound when the target time is in the next second
- **Edge Case**: If `wait_ms == 0` (already at the target time), sends immediately

## Benefits

1. **Predictable Timing**: Proposals are sent at known, precise times
2. **Load Distribution**: Multiple instances don't send proposals simultaneously
3. **Measurement**: Easier to analyze timing and latency with consistent proposal times
4. **Testing**: Reproducible behavior for performance testing

