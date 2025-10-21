# Gatling View Tracking: Handling Non-Sequential Views

## Overview

This document explains how the gatling thread handles non-sequential view numbers when ordering blocks across multiple consensus instances.

## The Original Bug

The original gatling implementation assumed views would be **sequential starting from 1**:

```rust
// OLD CODE - BUGGY
for inst_idx in 0..num_instances {
    let queue = &instance_queues[inst_idx];
    
    // Always look for the NEXT sequential view
    let next_view = finalized_views[inst_idx] + 1;
    
    if let Some(block) = queue.get(&next_view) {
        // Can we finalize this view?
        ...
    }
}
```

**Problem:** If `finalized_views[0]` starts at `0`, the code looks for view `1`. But in practice:
- Instance 1's first finalized block might be at **view 2** (not view 1!)
- Instance 2's first finalized block might be at **view 5** (not view 1!)

The gatling thread would be **stuck waiting for view 1**, which never arrives, and no gatling finalization messages would appear.

## Why Views Aren't Sequential

Views can skip numbers for several legitimate reasons:

1. **Timeouts/failures** - If a view times out without reaching consensus, the system moves to the next view without producing a block
2. **Block restoration** - When a validator restarts, it may restore blocks from already-progressed views
3. **Consensus protocol behavior** - The first finalized block might legitimately be at view 2+ if view 1 didn't produce a block

### Real Example from Logs

```
consensus initialized current_view=1
[consensus_1] Validator 0 proposed block 1 (view 1) ...
[consensus_1] Validator 0 finalized block 1 (view 2) ...  ← First finalization at view 2!
```

The proposal happened at view 1, but by the time the block was finalized, the consensus had moved to view 2.

## The Fix

The new code **doesn't assume sequential views**. Instead, it looks at **what views are actually available** in the queues:

```rust
// NEW CODE - FIXED
for inst_idx in 0..num_instances {
    let queue = &instance_queues[inst_idx];
    
    // Find the SMALLEST view actually present in this queue (not sequential)
    if let Some((&view, block)) = queue.iter().next() {  // BTreeMap.iter() gives smallest key first
        // Check if all instances k < inst_idx have completed this view or higher
        let can_proceed = (0..inst_idx).all(|k| finalized_views[k] >= view);
        
        if can_proceed {
            // This block is eligible - check if it's the global minimum
            if view < min_view || (view == min_view && inst_idx < min_instance) {
                min_view = view;
                min_instance = inst_idx;
                can_finalize = Some((inst_idx, view, block.clone()));
            }
        }
    }
}
```

### Key Changes

1. **Look at actual views in queue** using `queue.iter().next()` (gets smallest view in BTreeMap)
2. **Find global minimum** across all instances that satisfy the gatling rule
3. **Track highest finalized view per instance** (not "next expected view")

This allows gatling to work with **any view numbers**, whether sequential or with gaps!

## Detailed Example Walkthrough

Let's trace through a real scenario with 2 consensus instances to see exactly how the gatling ordering works.

### Initial State

```
finalized_views = [0, 0]     // Nothing finalized yet
instance_queues[0] = {}      // Instance 1 queue empty
instance_queues[1] = {}      // Instance 2 queue empty
```

### Event 1: Instance 1 Finalizes Block at View 2

**State Update:**
```
instance_queues[0] = {2 -> Block{height: 1, ...}}
```

**Gatling Check:**
- Can finalize view 2 from instance 1?
- Check: Do all instances k < 1 have completed view 2?
  - No instances exist before instance 1 ✅ (vacuously true)
- **Decision: FINALIZE!**

**Output:**
```
[gatling] Validator 0 finalized block 1 from instance 1 (view 2) with 0 transactions
```

**State After:**
```
finalized_views = [2, 0]
instance_queues[0] = {}
```

---

### Event 2: Instance 2 Finalizes Block at View 5

**State Update:**
```
instance_queues[1] = {5 -> Block{height: 1, ...}}
```

**Gatling Check:**
- Can finalize view 5 from instance 2?
- Check: Do all instances k < 2 have completed view 5?
  - Instance 1 (k=0): `finalized_views[0] = 2`
  - Is 2 >= 5? ❌ NO
- **Decision: CANNOT FINALIZE - Must wait for instance 1 to reach view 5**

**Output:**
```
(no gatling log - block stays in queue)
```

**State After:**
```
finalized_views = [2, 0]
instance_queues[1] = {5 -> Block{height: 1, ...}}  // Still waiting
```

---

### Event 3: Instance 1 Finalizes Block at View 4

**State Update:**
```
instance_queues[0] = {4 -> Block{height: 2, ...}}
```

**Gatling Check:**
- Can finalize view 4 from instance 1?
- Check: Do all instances k < 1 have completed view 4?
  - No instances exist before instance 1 ✅
- **Decision: FINALIZE!**

**Output:**
```
[gatling] Validator 0 finalized block 2 from instance 1 (view 4) with 0 transactions
```

**State After:**
```
finalized_views = [4, 0]
instance_queues[0] = {}
instance_queues[1] = {5 -> Block{height: 1, ...}}  // Still waiting (4 < 5)
```

---

### Event 4: Instance 1 Finalizes Block at View 5

**State Update:**
```
instance_queues[0] = {5 -> Block{height: 3, ...}}
```

**Gatling Check - First for Instance 1:**
- Can finalize view 5 from instance 1?
- Check: Do all instances k < 1 have completed view 5?
  - No instances exist before instance 1 ✅
- **Decision: FINALIZE!**

**Output:**
```
[gatling] Validator 0 finalized block 3 from instance 1 (view 5) with 0 transactions
```

**Gatling Check - Now for Instance 2 (automatic retry):**
- Can finalize view 5 from instance 2?
- Check: Do all instances k < 2 have completed view 5?
  - Instance 1 (k=0): `finalized_views[0] = 5`
  - Is 5 >= 5? ✅ YES!
- **Decision: FINALIZE!**

**Output:**
```
[gatling] Validator 0 finalized block 1 from instance 2 (view 5) with 0 transactions
```

**State After:**
```
finalized_views = [5, 5]
instance_queues[0] = {}
instance_queues[1] = {}
```

---

### Event 5: Instance 2 Finalizes Block at View 7

**State Update:**
```
instance_queues[1] = {7 -> Block{height: 2, ...}}
```

**Gatling Check:**
- Can finalize view 7 from instance 2?
- Check: Do all instances k < 2 have completed view 7?
  - Instance 1 (k=0): `finalized_views[0] = 5`
  - Is 5 >= 7? ❌ NO
- **Decision: CANNOT FINALIZE - Must wait**

**Output:**
```
(no gatling log - block stays in queue)
```

---

## The Gatling Finalization Rule

A block at view V from instance N can be finalized by gatling when:

1. **The block exists** in instance N's queue
2. **All previous instances** (k < N) have finalized view V or higher: `finalized_views[k] >= V` for all k < N

This ensures a total ordering where:
- **Lower view numbers are finalized before higher view numbers**
- **Within the same view, lower instance numbers are finalized before higher instance numbers**

## Visual Representation

```
View Timeline:
             1   2   3   4   5   6   7   8
Instance 1:      ✓       ✓   ✓           
Instance 2:                  ✓           ⏳

Gatling Order:
1. Instance 1, View 2  ← Finalized immediately (no dependencies)
2. Instance 1, View 4  ← Finalized immediately (no dependencies)
3. Instance 1, View 5  ← Finalized immediately (no dependencies)
4. Instance 2, View 5  ← Finalized now (instance 1 reached view 5)
5. Instance 2, View 7  ← Waiting (instance 1 hasn't reached view 7)
```

## Implementation Notes

- **BTreeMap** is used for instance queues to automatically keep views sorted
- **`queue.iter().next()`** gives the smallest available view
- **No assumptions** are made about view numbering or gaps
- **Automatic retry** after each finalization checks if more blocks can now be finalized

This design makes gatling robust to any consensus behavior, including view skipping, timeouts, and network partitions.

