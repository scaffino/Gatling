# Gatling Finalization Rule

## Overview

The gatling finalization logic ensures strict global ordering of blocks across multiple consensus instances using a two-phase algorithm. Blocks are finalized by gatling in strict order: all instances for view 1, then all instances for view 2, and so on. Within each view, instances are finalized in ascending order (0, 1, 2, ..., K-1).

## Two-Phase Algorithm

The algorithm maintains two key variables:

- **v***: The maximum view number where ALL instances have been gatling-finalized. Starts at 0.
- **k***: The maximum instance (0-indexed) that has been gatling-finalized at view v* + 1. Starts as `None` (no instances finalized at v*+1 yet), becomes `Some(k)` when instances 0..k have been finalized.

When a consensus-finalized block arrives, the algorithm attempts to advance v* and k* using a two-phase approach:

### Phase 1: Advance v* (Complete View)

If all instances have consensus-finalized (or gatling-finalized) view v* + 1:

1. Gatling-finalize blocks for ALL instances (0 to K-1) at view v* + 1, in order
2. Set v* = v* + 1
3. Set k* = K - 1 (all instances have been finalized for this view)

This happens when the system is making synchronized progress across all instances.

### Phase 2: Advance k* (Partial Next View)

If we cannot advance v* (not all instances have view v* + 1 yet), find the maximum contiguous k where all instances k' ≤ k have finalized view v* + 1:

1. Starting from k* + 1 (or 0 if k* is None), find the maximum consecutive k where all instances have view >= v* + 1
2. Gatling-finalize all instances from (k* + 1) to the new k at view v* + 1 (in order)
3. Set k* = the new maximum k

This happens when some instances are ahead of others - we finalize all ready instances in a contiguous sequence at the current "head" view, not just one at a time.

## Output Order

The algorithm produces blocks in strict order:

1. All instances [0..K-1] for view 1
2. All instances [0..K-1] for view 2
3. ...
4. All instances [0..K-1] for view v*
5. Instances [0..k*] for view v* + 1

This creates the ordering: (0,1), (1,1), ..., (K-1,1), (0,2), (1,2), ..., (K-1,2), ..., (0,v*), (1,v*), ..., (K-1,v*), (0,v*+1), ..., (k*,v*+1)

## Implementation Details

### Data Structures

```rust
// Per-instance queues of consensus-finalized blocks, keyed by view number
let mut instance_queues: Vec<BTreeMap<u64, Block>> = 
    (0..num_instances).map(|_| BTreeMap::new()).collect();

// Track the highest view we've gatling-finalized for each instance
let mut gatling_finalized_views: Vec<u64> = vec![0; num_instances];

// v*: maximum view where all instances have been gatling-finalized
let mut v_star: u64 = 0;

// k*: maximum instance (0-indexed) that has been gatling-finalized at view v* + 1
// None means no instances finalized at v*+1 yet, Some(k) means instances 0..k have been finalized
let mut k_star: Option<usize> = None;
```

### Algorithm Pseudocode

```
When consensus-finalized block arrives for instance i at view v:
  1. Add block to instance_queues[i]
  
  2. Loop until no progress:
     a. Phase 1: Try to advance v*
        If all instances k have blocks at view v* + 1:
          - Gatling-finalize all instances at view v* + 1 (in order)
          - v* = v* + 1
          - k* = K - 1
          - Continue loop
     
     b. Phase 2: Try to advance k*
        Find the maximum contiguous k starting from k*+1 (or 0 if k* is None) 
        where all instances k' ≤ k have block at view v* + 1:
          - Finalize all instances from (k*+1) to the new k at view v* + 1 (in order)
          - k* = new maximum k
          - Continue loop
     
     c. No progress possible, break
```

### Checking for Blocks

An instance k is considered to have a block at view v if either:

1. There is a block in `instance_queues[k]` at view v (consensus-finalized but not yet gatling-finalized)
2. `gatling_finalized_views[k] >= v` (already gatling-finalized this view or higher, implicit finalization)

The second condition handles gap views - if an instance finalizes view 10, views 1-10 are all considered finalized.

## Examples

### Example 1: Synchronized Progress

**State:**
- All instances have finalized view 2
- v* = 0, k* = 0

**When all instances finalize view 3:**
1. Phase 1 check: All instances have view 3? YES
2. Gatling-finalize: instances 0, 1, 2, 3, 4 at view 3 (in order)
3. Update: v* = 3, k* = 4

**Output order:** (0,1), (1,1), ..., (4,1), (0,2), (1,2), ..., (4,2), (0,3), (1,3), ..., (4,3)

### Example 2: Partial Progress

**State:**
- v* = 2, k* = None (no instances finalized at view 3 yet)
- Instance 0 has view 4
- Instances 1-4 have view 3

**When instance 1 finalizes view 4:**
1. Phase 1 check: All instances have view 3? YES (already done)
2. Phase 1 check: All instances have view 4? NO (instances 2-4 don't)
3. Phase 2: Find max contiguous k starting from 0 where all have view 3
   - Instance 0 has view >= 3? YES (has view 4)
   - Instance 1 has view >= 3? YES (has view 4)
   - Instance 2 has view >= 3? YES (has view 3)
   - Instance 3 has view >= 3? YES (has view 3)
   - Instance 4 has view >= 3? YES (has view 3)
   - new_k* = 4
4. Gatling-finalize: all instances 0-4 at view 3 (in order)
5. Update: k* = Some(4)

**Note:** With the current implementation, once all instances have view 3, k* advances to include all of them at once, rather than one at a time.

### Example 3: User's Scenario

**State:**
- Instance 4 finalizes view 2
- Instance 5 finalizes view 2
- Instance 1 finalizes view 4
- v* = 0, k* = None initially

**Execution:**
1. When instance 4 (index 3) finalizes view 2:
   - Phase 1: All instances have view 1? Check each instance...
   - Phase 2: Instance 4 has view 1? If yes, gatling-finalize and k* = 3
   
2. When instance 5 (index 4) finalizes view 2:
   - Phase 1: All instances have view 1? Continue checking...
   - Phase 2: Instance 5 has view 1? If yes, gatling-finalize and k* = 4
   
3. When all instances have view 1:
   - Phase 1: All instances have view 1? YES
   - Gatling-finalize: all instances at view 1
   - v* = 1, k* = 4

4. When all instances have view 2:
   - Phase 1: All instances have view 2? YES
   - Gatling-finalize: all instances at view 2
   - v* = 2, k* = 4

**Output order:** (0,1), (1,1), (2,1), (3,1), (4,1), (0,2), (1,2), (2,2), (3,2), (4,2), ...

## Edge Cases

### Case 1: Gap Views (Implicit Finalization)

**Scenario:** Instance 1 finalizes view 10 after view 5 (views 6-9 were skipped).

**Behavior:**
- When instance 1 gatling-finalizes view 10, `gatling_finalized_views[1] = 10`
- The algorithm considers instance 1 to have blocks at views 1-10
- Views 6-9 are implicitly finalized (no blocks need to exist for them)

**Key insight:** The algorithm handles view gaps gracefully through implicit finalization.

### Case 2: Out-of-Order Arrival

**Scenario:** Blocks arrive at different times:
- t=0: Instance 4 view 87 arrives
- t=1: Instance 2 view 86 arrives
- t=2: Instance 1 view 86 arrives

**Behavior:**
1. Blocks are added to queues as they arrive
2. Algorithm checks if v* can advance (needs all instances at view v*+1)
3. When instance 0 view 86 arrives, v* advances and all instances at view 86 are finalized
4. Block ordering is determined by v*/k*, not arrival time

**Key insight:** Queues buffer out-of-order arrivals, algorithm processes in correct global order.

### Case 3: Initial Bootstrap

**Scenario:** System just started, v* = 0, k* = None, no blocks finalized yet.

**Behavior:**
- When first block arrives (e.g., instance 0 view 1):
  - Phase 1: All instances have view 1? NO
  - Phase 2: Find max contiguous k starting from 0 where all have view 1
    - Instance 0 has view >= 1? YES
    - Instance 1 has view >= 1? Check queue... (may or may not have it yet)
    - If only instance 0 has it: k* = Some(0), finalize instance 0
- When more instances finalize view 1:
  - Phase 2 runs again, finds more consecutive instances with view >= 1
  - k* advances to include all ready instances in contiguous range
- When all instances have view 1:
  - Phase 1: All instances have view 1? YES
  - Gatling-finalize: all instances at view 1
  - v* = 1, k* = Some(K-1)

**Key insight:** System starts with partial progress (advancing k*), then makes full progress (advancing v*).

### Case 4: Stalled Instance

**Scenario:** Instance 0 stops making progress at view 5, while others continue.

**Behavior:**
- v* cannot advance beyond 5 until instance 0 reaches view 6+
- k* can still advance for view 6 if instances 1+ have view 6
- But v* remains at 5, so only view 6 blocks for instances beyond instance 0 can be finalized

**Key insight:** By design, one stalled instance prevents v* from advancing, ensuring strict ordering.

## Key Properties

### 1. Strict Global Ordering

Blocks are finalized in strictly ascending (view, instance) order. The lowest view number finalizes first, and within the same view, the lowest instance index finalizes first.

### 2. Two-Phase Progress

- **Phase 1 (Complete views):** When all instances are synchronized, entire views are finalized at once
- **Phase 2 (Partial views):** When instances are at different views, we finalize the next ready instance incrementally

### 3. Implicit Finalization

When an instance finalizes view V, all views 1 to V are considered finalized for that instance. This handles:
- View gaps (timeouts, nullifications)
- Consensus protocol variations
- System restarts

### 4. Deadlock Prevention

The algorithm doesn't create deadlocks - it only orders blocks that consensus has already finalized. If consensus makes progress (which it's designed to do), gatling will too.

## Code Location

The gatling finalization logic is implemented in:
- File: `alto/chain/src/bin/validator.rs`
- Function: `gatling_thread()` (lines 64-220)
- Key logic: Two-phase v*/k* algorithm (lines 92-216)
