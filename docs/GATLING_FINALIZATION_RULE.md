# Gatling Finalization Rule

## Overview

The gatling finalization logic ensures strict global ordering of blocks across multiple consensus instances. Blocks are finalized by gatling in increasing view number first, then by increasing instance number within the same view.

## Core Rules

### Rule 1: View V of Instance K Finalizes After All Lower Instances Have Finalized View V

**Definition:** Block at view V from instance K can only be finalized when all instances with index < K have already finalized view V.

**Implementation:**
```rust
let all_lower_instances_finalized = (0..inst_idx).all(|k| finalized_views[k] >= view);
```

**Example:**
- Instance 0 finalizes view 5 → `finalized_views[0] = 5`
- Instance 1 can now finalize view 5 (checked: instance 0 has 5 >= 5 ✓)
- Instance 2 can now finalize view 5 (checked: instances 0, 1 both have >= 5 ✓)

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

The gatling thread iteratively finalizes blocks using a **range-based approach**:

#### Step 1: Collect All Pending Views
```rust
let mut all_pending_views: Vec<u64> = Vec::new();
for k in 0..num_instances {
    if let Some((&view, _)) = instance_queues[k].iter().next() {
        all_pending_views.push(view);
    }
}
```

Collects the smallest pending view from each instance (if any).

#### Step 2: Sort and Deduplicate
```rust
all_pending_views.sort_unstable();
all_pending_views.dedup();
```

Creates a sorted list of unique view numbers to try finalizing.

#### Step 3: Try Finalization in Order
```rust
for view in all_pending_views {
    for inst_idx in 0..num_instances {
        if pending_view == view {
            let all_lower_instances_finalized = 
                (0..inst_idx).all(|k| finalized_views[k] >= view);
            
            if all_lower_instances_finalized {
                can_finalize = Some((inst_idx, view, block.clone()));
                break;
            }
        }
    }
    if can_finalize.is_some() { break; }
}
```

For each view (ascending order):
- Check each instance (lowest to highest index)
- If instance has a block at this view
- Verify all lower-indexed instances have finalized this view
- Finalize the first eligible instance

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

### Iteration 1: Finding View to Finalize
```
Step 1: all_pending_views = [7, 5]  (from instances 0, 1)
Step 2: sorted = [5, 7]

Step 3: Try view 5
  Instance 0: pending_view = 7, 7 != 5 → skip
  Instance 1: pending_view = 5, 5 == 5
    Check: lower instances finalized >= 5?
      Instance 0: finalized_views[0] = 4, 4 < 5 → NO
    Result: BLOCKED (instance 0 hasn't reached view 5)

Step 3: Try view 7
  Instance 0: pending_view = 7, 7 == 7
    Check: lower instances finalized >= 7?
      No lower instances for instance 0 → YES
    Result: FINALIZE view 7 from instance 0
```

### After Finalization
```
finalized_views[0] = 7
Instance 0 queue: {} (empty)

Step 1: all_pending_views = [5, 8]  (from instance 1)
Step 2: sorted = [5, 8]

Step 3: Try view 5
  Instance 1: pending_view = 5, 5 == 5
    Check: lower instances finalized >= 5?
      Instance 0: finalized_views[0] = 7, 7 >= 5 → YES
    Result: FINALIZE view 5 from instance 1
```

### After Second Finalization
```
finalized_views[0] = 7
finalized_views[1] = 5

Step 1: all_pending_views = [8]
Step 3: Try view 8
  Instance 1: pending_view = 8, 8 == 8
    Check: lower instances finalized >= 8?
      Instance 0: finalized_views[0] = 7, 7 < 8 → NO
    Result: BLOCKED (instance 0 hasn't reached view 8)

Iteration complete, waiting for more blocks...
```

## Key Properties

### 1. Global View Ordering
Blocks are finalized in strictly ascending view order. View V cannot be finalized until all instances have progressed past V-1.

### 2. Per-View Instance Ordering
Within the same view, instances finalize in ascending index order (0, 1, 2, ...).

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
View 15 can finalize because all instances have finalized >= 15. The gap (views 11-14) is implicitly finalized.

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
- Function: `gatling_thread()` (lines 62-171)
- Key logic: lines 93-144 (range-based finalization algorithm)

