# Gatling Finalization Rule

## Overview

The gatling finalization logic ensures strict global ordering of blocks across multiple consensus instances. Blocks are finalized by gatling in increasing view number first, then by increasing instance number within the same view.

## Core Rules

### Rule 1: Instance K at View V Finalizes After All Lower Instances Have Moved Past View V

**Definition:** Block at view V from instance K can only be finalized when all instances with index < K have their next pending block at view > V (they've moved past this view).

**Implementation:**
```rust
let all_lower_instances_ahead = (0..inst_idx).all(|k| {
    if let Some((&pending_view, _)) = instance_queues[k].iter().next() {
        pending_view > view  // Lower instance's next block must be beyond this view
    } else {
        true  // Empty queue means "ahead"
    }
});
```

**Example:**
- Instance 0 has block at view 4 (next pending: view 4)
- Instance 1 has block at view 3 (next pending: view 3)
- Instance 1 can finalize view 3 because instance 0's next block (view 4) is > 3 ✓
- Instance 0 can then finalize view 4 ✓

**Key difference from old rule:** Lower instances don't need to have *finalized* view V, they just need to have *moved past* it (their next block is at a higher view).

### Rule 2: Implicit Finalization of Gap Views

**Definition:** When instance N finalizes view V, all views from the previously finalized view + 1 to V are implicitly considered finalized for that instance.

**Implementation:**
```rust
// If instance 2 finalizes view 5, and previously finalized view 2:
finalized_views[2] = 5;  // Views 3, 4, 5 are now all finalized

// Any pending blocks with views < 5 are removed from the queue
instance_queues[2].retain(|&v, _| v >= 5);
```

**Example:**
- Instance 1 has finalized views 2, 3
- Instance 1 finalizes block at view 7 (skipping 4, 5, 6)
- Views 4, 5, 6 are implicitly finalized
- `finalized_views[1] = 7`
- Any pending blocks at views 4, 5, 6 are removed from the queue

## Implementation Details

### Data Structures

**Instance Queues:**
```rust
let mut instance_queues: Vec<BTreeMap<u64, Block>> = (0..num_instances).map(|_| BTreeMap::new()).collect();
```
- One queue per consensus instance
- BTreeMap ensures views are automatically sorted
- Key = view number, Value = Block

**Finalized Views Tracking:**
```rust
let mut finalized_views: Vec<u64> = vec![0; num_instances];
```
- One entry per instance
- Tracks the highest view finalized by that instance
- Used to check ordering constraints

### Finalization Algorithm

The gatling thread iteratively finalizes blocks using a **(view, instance) ordering approach**:

#### Step 1: Collect All Pending Blocks
```rust
let mut pending_blocks: Vec<(usize, u64, Block)> = Vec::new();
for inst_idx in 0..num_instances {
    if let Some((&view, block)) = instance_queues[inst_idx].iter().next() {
        pending_blocks.push((inst_idx, view, block.clone()));
    }
}
```

Collects the smallest pending (instance_idx, view, block) tuple from each instance (if any).

#### Step 2: Sort by (view, instance_idx)
```rust
pending_blocks.sort_by_key(|(inst_idx, view, _)| (*view, *inst_idx));
```

Creates a sorted list of blocks prioritized by view first (ascending), then instance index (ascending). This ensures the lowest view of the lowest instance is considered first.

#### Step 3: Try Finalization in Sorted Order
```rust
for (inst_idx, view, block) in pending_blocks {
    let all_lower_instances_ahead = (0..inst_idx).all(|k| {
        if let Some((&pending_view, _)) = instance_queues[k].iter().next() {
            pending_view > view  // Lower instance's next block must be beyond this view
        } else {
            true  // Empty queue means "ahead"
        }
    });
    
    if all_lower_instances_ahead {
        can_finalize = Some((inst_idx, view, block));
        break;
    }
}
```

For each (instance, view, block) in sorted order:
- Check if all lower-indexed instances have their next pending block at view > current view
- If yes, finalize this block (it's the first eligible in sorted order)
- This ensures blocks finalize in strict (view, instance) ascending order

#### Step 4: Update Tracking
```rust
finalized_views[inst_idx] = view;
instance_queues[inst_idx].remove(&view);

// Remove implicitly finalized blocks (views < current)
let views_to_remove: Vec<_> = instance_queues[inst_idx]
    .range(..view)
    .map(|(&v, _)| v)
    .collect();
    
for v in views_to_remove {
    instance_queues[inst_idx].remove(&v);
}
```

When a block is finalized:
- Update `finalized_views` to mark all views up to current as finalized
- Remove the finalized block from queue
- Remove any blocks at views below the current (implicitly finalized)

## Detailed Walkthrough Example

### Scenario Setup
```
Instance 0 (index 0):
  finalized_views[0] = 4
  queue = {view 7 -> BlockA}

Instance 1 (index 1):
  finalized_views[1] = 3
  queue = {view 5 -> BlockB, view 8 -> BlockC}
```

### Iteration 1: Finding Block to Finalize
```
Step 1: pending_blocks = [(0, 7, BlockA), (1, 5, BlockB)]
Step 2: sorted = [(1, 5, BlockB), (0, 7, BlockA)]  // view 5 comes before view 7

Step 3: Try (1, 5) - Instance 1 at view 5
  Check: lower instances ahead?
    Instance 0: next pending view = 7, is 7 > 5? YES ✓
  Result: FINALIZE view 5 from instance 1

After finalization: finalized_views[1] = 5, instance 1 queue: {8 -> BlockC}

Step 3: Try (0, 7) - Instance 0 at view 7
  Check: lower instances ahead?
    No lower instances for instance 0 → YES
  Result: FINALIZE view 7 from instance 0
```

### After Second Finalization
```
finalized_views[0] = 7
finalized_views[1] = 5

Step 1: pending_blocks = [(1, 8, BlockC)]
Step 3: Try (1, 8) - Instance 1 at view 8
  Check: lower instances ahead?
    Instance 0: no pending blocks (empty queue) → "ahead" ✓
  Result: FINALIZE view 8 from instance 1
```

**Key observation:** With the new algorithm, instance 1's view 5 finalizes *before* instance 0's view 7 because views are sorted first, then instances. The constraint ensures lower instances have moved past the view.

## Key Properties

### 1. Global (View, Instance) Ordering
Blocks are finalized in strictly ascending (view, instance) order. The lowest view number finalizes first, and within the same view, the lowest instance index finalizes first.

### 2. Constraint-Based Finalization
Instance K at view V can finalize only if all instances with index < K have their next pending block at view > V. This ensures lower-indexed instances have "moved past" the view before higher-indexed instances can finalize it.

### 3. Gap Tolerance
The system handles view gaps gracefully:
- If an instance finalizes view 5 after view 2, views 3, 4, 5 are all marked as finalized
- This allows the system to handle consensus timeouts, restarts, and protocol variations

### 4. Deadlock Prevention
The range-based approach ensures progress:
- Even if instance 1 is at view 100 and instance 0 is stuck at view 5, the system will finalize from instance 1 once it reaches eligible views
- The system doesn't wait forever for one stuck instance

## Edge Cases

### Case 1: All Views Not Yet Received
```
Instance 0: finalized_views[0] = 10, queue = {}
Instance 1: finalized_views[1] = 10, queue = {15 -> Block}
```
View 15 can finalize because instance 0 has no pending blocks (empty queue is considered "ahead"). The gap (views 11-14) is implicitly finalized.

### Case 2: Synchronized Finalization
```
Both instances at same view:
Instance 0: finalized_views[0] = 5, queue = {6 -> Block}
Instance 1: finalized_views[1] = 5, queue = {6 -> Block}
```
View 6 from instance 0 finalizes first (lower index), then view 6 from instance 1.

### Case 3: Large Gaps
```
Instance 0: finalized_views[0] = 2, queue = {100 -> Block}
Instance 1: finalized_views[1] = 50, queue = {51 -> Block, ...}
```
Views 3-99 for instance 0 are implicitly finalized. View 100 can finalize once instance 1 also reaches >= 100.

## Code Location

The gatling finalization logic is implemented in:
- File: `alto/chain/src/bin/validator.rs`
- Function: `gatling_thread()` (lines 62-181)
- Key logic: lines 95-129 ((view, instance) sorting and constraint-based finalization algorithm)

