# One Proposal Per Second with Rotating Leaders

## Goal

Achieve **one proposal per second** with **rotating leadership** where:
- ✅ Validators rotate as leaders (only leader proposes)
- ✅ Each proposal happens at exact second boundaries
- ✅ Total: 1 proposal per second across all validators
- ✅ Each validator proposes every N seconds (N = number of validators)

## Design

### With 4 Validators

**Timeline for single consensus instance:**
```
Second 0 (X+0.000s): View 0, Validator 0 is leader → proposes at X+0.000s
Second 1 (X+1.000s): View 1, Validator 1 is leader → proposes at X+1.000s
Second 2 (X+2.000s): View 2, Validator 2 is leader → proposes at X+2.000s
Second 3 (X+3.000s): View 3, Validator 3 is leader → proposes at X+3.000s
Second 4 (X+4.000s): View 4, Validator 0 is leader → proposes at X+4.000s
...
```

**Key points:**
- Each view lasts approximately **1 second**
- Only the **leader proposes** in each view
- Each validator proposes every **4 seconds** (when it's their turn)
- Total throughput: **1 proposal per second per instance**

### With 10 Consensus Instances (Staggered)

Each instance has a time offset to spread load:

```
X+0.000s: Instance 1, Validator A proposes (view V1)
X+0.100s: Instance 2, Validator B proposes (view V2)
X+0.200s: Instance 3, Validator C proposes (view V3)
...
X+0.900s: Instance 10, Validator J proposes (view V10)
X+1.000s: Instance 1, Validator A+1 proposes (view V1+1) ← next leader
X+1.100s: Instance 2, Validator B+1 proposes (view V2+1) ← next leader
...
```

**Result:** 
- 10 proposals per second across all instances
- Each instance: 1 proposal per second
- Perfect timing regularity

## Implementation

### Timeout Settings

Set in `chain/src/bin/validator.rs`:

```rust
const LEADER_TIMEOUT: Duration = Duration::from_millis(1500);
const NOTARIZATION_TIMEOUT: Duration = Duration::from_millis(1500);
```

**Why these values:**

1. **LEADER_TIMEOUT = 1.5s**
   - Allows leader to wait up to 1 second for their time slot
   - Plus 0.5s buffer for proposal propagation
   - Must be > 1s to avoid timeout while waiting for second boundary

2. **NOTARIZATION_TIMEOUT = 1.5s**  
   - Allows time to collect threshold signatures (typically 200-400ms)
   - Keeps view duration under 2 seconds
   - Ensures next view can start close to next second boundary

### How It Works

**Sequence for each view:**

1. **View starts** (e.g., at `X.200s`)
2. **Leader identified** (determined by view number % validator count)
3. **Leader waits** for next second boundary
   - Current time: `X.200s`
   - Target time: `X+1.000s`
   - Wait: `800ms`
4. **Proposal sent** at `X+1.000s` (exact second boundary)
5. **Signatures collected** (`X+1.000s` to `X+1.400s`)
6. **Block notarized** (by `X+1.400s`)
7. **Next view starts** (around `X+1.500s`)
8. **Repeat** with next leader

**Total view duration:** ~1-1.5 seconds

## Expected Behavior

### In Logs (Single Instance)

Perfect scenario:
```
12:00:00.000 | [consensus_1] Validator 0 proposed block 10 (view 40)
12:00:01.000 | [consensus_1] Validator 1 proposed block 11 (view 41)
12:00:02.000 | [consensus_1] Validator 2 proposed block 12 (view 42)
12:00:03.000 | [consensus_1] Validator 3 proposed block 13 (view 43)
12:00:04.000 | [consensus_1] Validator 0 proposed block 14 (view 44)
```

**Characteristics:**
- ✅ Exactly 1.000s between proposals
- ✅ Views increment by 1
- ✅ Validators rotate in order
- ✅ All proposals at `.000` boundary

### Analysis Results

Using the analysis script:
```bash
python3 measurements/proposal-times/check_proposal_times.py validator*.log
```

**Target metrics per instance:**
- **Median gap:** ~1000ms
- **Average gap:** 1000-1500ms
- **Min gap:** 800-1000ms
- **Max gap:** 1500-2000ms
- **Views between proposals:** ~1 per validator (4 total between a validator's proposals)

**Across all instances (10 instances):**
- **Average gap:** ~100ms (proposals spread evenly)
- **Total throughput:** ~10 proposals per second

## Tuning for Your Network

The 1.5-second timeouts work for most networks. Adjust if needed:

### Fast Network (< 50ms latency)

If you see proposals completing very quickly, you can be more aggressive:

```rust
const LEADER_TIMEOUT: Duration = Duration::from_millis(1200);
const NOTARIZATION_TIMEOUT: Duration = Duration::from_millis(1000);
```

**Result:** Views might complete in ~1 second exactly

### Slow Network (> 100ms latency)

If you see timeout errors or view changes without proposals:

```rust
const LEADER_TIMEOUT: Duration = Duration::from_millis(2000);
const NOTARIZATION_TIMEOUT: Duration = Duration::from_millis(2000);
```

**Result:** Views might take ~1.5-2 seconds, proposals every 1-2 seconds

### Check Your Network Latency

```bash
# From validator 0 to validator 1
ping <validator1_ip>

# Look for round-trip time (RTT)
# Example: 64 bytes from X.X.X.X: icmp_seq=0 ttl=64 time=45.2 ms
```

**Rule of thumb:**
- `LEADER_TIMEOUT` ≥ 1000ms + (2 × network_latency)
- `NOTARIZATION_TIMEOUT` ≥ 1000ms + (4 × network_latency)

## Troubleshooting

### Issue: Proposals NOT every second

**Symptoms:**
```
12:00:00.000 | Validator 0 proposed (view 40)
12:00:02.000 | Validator 1 proposed (view 41)  ← 2 seconds, not 1!
12:00:03.000 | Validator 2 proposed (view 42)  ← OK
12:00:05.000 | Validator 3 proposed (view 43)  ← 2 seconds again!
```

**Diagnosis:** Views are taking longer than 1 second

**Causes:**
1. Network latency too high for 1.5s timeouts
2. Signature collection taking too long
3. System overload (CPU/disk)

**Solutions:**
- Increase timeouts to 2s
- Check network latency between validators
- Reduce number of consensus instances
- Check CPU/disk usage

### Issue: Views racing ahead

**Symptoms:**
```
Analysis shows:
  Instance 1: Views between proposals: 5-10  ← Should be ~1
```

**Diagnosis:** Timeouts too aggressive, views changing before proposals complete

**Solution:** Increase timeouts:
```rust
const LEADER_TIMEOUT: Duration = Duration::from_millis(2000);
const NOTARIZATION_TIMEOUT: Duration = Duration::from_millis(2500);
```

### Issue: Some proposals delayed by 1+ seconds

**Symptoms:**
```
12:00:00.000 | Validator 0 proposed
12:00:01.000 | Validator 1 proposed  ← OK
12:00:03.000 | Validator 2 proposed  ← Delayed! Should be 12:00:02.000
```

**Possible causes:**
1. **Block building too slow:** If creating the block takes > 1s, validator misses time slot
2. **View started late:** Previous view took too long, new view started after time slot
3. **System load:** CPU/disk causing delays

**Solutions:**
- Profile block building time
- Reduce transaction processing per block
- Check system resources
- Consider slightly longer timeouts (1.8-2s)

### Issue: Perfect timing in some instances, irregular in others

**Symptoms:**
```
Instance 1: Perfect 1s cadence ✅
Instance 5: Irregular 1-3s gaps ❌
```

**Possible causes:**
1. **Resource contention:** Some instances competing for CPU/disk
2. **Different view states:** Some instances further behind in consensus
3. **Network issues:** Some messages delayed

**Solutions:**
- Reduce total number of instances (try 5 instead of 10)
- Check if specific validators are slower
- Monitor per-instance resource usage

## Testing Procedure

### 1. Build

```bash
cd chain
cargo build --release
cd ..
```

### 2. Run Validators

```bash
# Your usual validator startup commands
# Run for at least 60 seconds to collect enough data
```

### 3. Analyze Timing

```bash
# Quick check - look at timestamps
grep "consensus_1.*proposed" validator0.log | head -20

# Full analysis
python3 measurements/proposal-times/check_proposal_times.py validator*.log
```

### 4. Verify Per Instance

Expected output for each instance:
```
Instance 1:
  Total proposals (all validators): ~60 (if ran for 60s)
  Average gap: 1000-1200ms
  Median gap: 1000ms
  Min gap: 800-1000ms
  Max gap: 1500-2000ms
```

### 5. Check View Progression

Create a script to check views:
```bash
grep "consensus_1.*Validator 0 proposed" validator0.log | \
  sed 's/.*view \([0-9]*\)).*/\1/' | \
  awk 'NR>1{print $1-prev} {prev=$1}'
```

**Expected output:** Mostly `1` or `2` (views progressing steadily with proposals)

**Bad output:** Numbers like `10`, `15`, `20` (views racing ahead)

## Summary

**Configuration:**
- `LEADER_TIMEOUT = 1500ms`: Allows waiting + proposing
- `NOTARIZATION_TIMEOUT = 1500ms`: Allows signature collection

**Expected behavior:**
- 1 proposal per second per instance
- Validators rotate as leaders
- Each validator proposes every 4 seconds (with 4 validators)
- Proposals at exact second boundaries
- Views progress at ~1 second per view

**Next steps:**
1. Rebuild: `cargo build --release`
2. Test with your setup
3. Analyze logs for 1-second gaps
4. Adjust timeouts if needed based on network conditions

The key insight: **View duration must match proposal cadence**. With 1.5s timeouts, views complete fast enough that the next leader is ready to propose at their second boundary, maintaining the 1-proposal-per-second rhythm.

