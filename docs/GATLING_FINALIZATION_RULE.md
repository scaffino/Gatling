# Gatling Finalization Rule

## Overview

Gatling combines blocks from multiple consensus instances into a single globally ordered sequence. It enforces strict ordering guarantees: a block at position (view v, instance k) can only be finalized after all blocks at positions (view ≤ v, instance ≤ k) have been finalized in lexicographic order.

**Critical distinction**: All blocks are finalized using the same finalization event. What differs is how **views** are finalized:
- **Directly finalized views**: Views that have a finalized block from consensus
- **Indirectly finalized views**: Views that are marked as finalized (without blocks) to fill gaps when instances skip views

## Critical Invariant: Height-Ordered Delivery via `run_buffer`

Before any block reaches the `gatling_thread`, it passes through a per-instance `run_buffer` task.
`run_buffer` starts at `next_expected_height = 1` and buffers blocks, only forwarding them once it
has a contiguous sequence from height 1 upward. It never skips heights.

**Consequence**: If height 1 for instance k is never delivered to `run_buffer`, the gatling cursor
will stall permanently at the first cell of instance k — even if all other instances are running
normally and `instance_queues[k]` has blocks at views 1, 2, 3, ...

**Why height 1 can be missing**: When a validator starts after genesis (`overrun_ms > 0`), it catches
up by finalizing a block at height N > 1. The `finalize_ancestors` task must successfully fetch and
deliver heights 1..N-1 before height N reaches `run_buffer`. Any failure in ancestor fetching (peer
unavailable, timeout, partial chain walk) will leave height 1 undelivered and permanently stall that
instance's slot in the cursor.

This is the reason ancestor fetching correctness is a liveness requirement, not merely a performance
concern. See `ANCESTOR_FETCHING.md` for the full fetch protocol.

## The Matrix View

Conceptually, Gatling maintains a 2D matrix where:
- **Rows** = consensus instances (0, 1, 2, ...)
- **Columns** = views (1, 2, 3, ...)
- **Cell (v, k)** = the block (if any) finalized by instance k at view v

The algorithm always processes cells in lexicographic order: (1,0), (1,1), ..., (1,K-1), (2,0), (2,1), ...

## Data Structures

```rust
// Per-instance queues of finalized blocks, keyed by view number (from block.view, not proof)
// - Some(block) = view that has a finalized block from consensus (directly finalized view)
// - None = view marked as finalized without a block (indirectly finalized view)
// Note: All blocks are finalized the same way - what differs is how VIEWS are finalized
let mut instance_queues: Vec<BTreeMap<u64, Option<Block>>> = 
    (0..num_instances).map(|_| BTreeMap::new()).collect();

// Per-instance highest directly finalized view (only tracks views with actual blocks from consensus)
// If finalized_up_to[k] >= v, then all views <= v for instance k are already finalized
let mut finalized_up_to: Vec<u64> = vec![0; num_instances];

// Global cursor for next top-leftmost unfinalized cell
let mut cursor_view: u64 = 1;
let mut cursor_instance: usize = 0;
```

### Key Points:
1. **`instance_queues[k]`**: Stores blocks and view markers for instance k, keyed by view number
   - `Some(block)` = directly finalized view (has a block from consensus)
   - `None` = indirectly finalized view (no block, but view is finalized)
2. **`finalized_up_to[k]`**: Highest view for instance k that has been directly finalized (has a block)
   - Only updated when finalizing a view that has a block
   - If `finalized_up_to[k] = V`, then all views ≤ V for instance k are finalized
3. **Cursor**: Always points to the next cell to process in lexicographic order

## Algorithm: Detailed Step-by-Step

### Phase 1: When a Block Arrives (GatlingEvent Processing)

When a `GatlingEvent` arrives for instance k with a block at view v:

#### Step 1.1: Extract View Number
```rust
let view = event.block.view;  // View comes from block itself, NOT from finalization proof
```

#### Step 1.2: Detect and Fill Gaps
- Find the highest view seen for this instance: `highest_seen_view = max(finalized_up_to[k], highest_queued_view)`
- If `view > highest_seen_view + 1`, there is a gap between the highest seen view and the new view
  - Example: Previously had view 5, now receiving view 8 → gap at views 6, 7
- Action: For each gap view, insert `None` into the queue to mark it as an indirectly finalized view
- Purpose: Ensures continuity - if an instance jumps from view 5 to 8, views 6-7 are marked as finalized (indirectly) even though they have no blocks

#### Step 1.3: Insert the Block
```rust
instance_queues[k].insert(view, Some(block));
```
- The block is stored at its view number
- This view is a directly finalized view (has a block from consensus)

### Phase 2: Cursor-Based Finalization Loop

After inserting the block, the algorithm attempts to finalize as many cells as possible by processing them in strict order:

#### Step 2.1: Cursor Safety Check
- Ensure cursor is within valid bounds
- If not, stop processing

#### Step 2.2: Check if View Already Finalized
```rust
if finalized_up_to[cursor_instance] >= cursor_view {
    // This view and all lower views are already finalized
    advance_cursor(...);
    continue;
}
```

**Why this works**: When a view V is directly finalized (has a block), `finalized_up_to[k] = V`. This means all views ≤ V for instance k have been processed. If the cursor is at view v ≤ V, we've already handled it.

#### Step 2.3: Process Current Cursor Cell

The algorithm checks what's at position `(cursor_view, cursor_instance)`:

**CASE A: Directly Finalized View (`Some(block)`)**
- Meaning: This view has a block from consensus that needs to be finalized
- Actions:
  1. Remove the block from the queue
  2. **Log the block and all its transactions** (this is when the block becomes globally finalized)
  3. Update `finalized_up_to[k] = cursor_view` (marks highest directly finalized view)
  4. Clean up: Remove all queue entries for views < cursor_view (already processed)
  5. Advance cursor to next position

**CASE B: Indirectly Finalized View (`None`)**
- Meaning: This view was marked as finalized (gap filler) but has no block
- Actions:
  1. Remove the `None` entry from queue
  2. Advance cursor (no logging, no `finalized_up_to` update)
  
**Why no logging or update?**: 
- No block exists, so nothing to log
- `finalized_up_to` only tracks views with actual blocks (directly finalized views)

**CASE C: No Entry, But Instance Has Moved Past**
- Check: Does this instance have any entry at view > cursor_view?
- If yes: The instance has finalized views beyond cursor_view, so cursor_view is a gap
  - Action: Insert `None` at cursor_view to mark it as an indirectly finalized view
  - Advance cursor (no logging, no `finalized_up_to` update)
- If no: Cannot make progress - waiting for a block at cursor_view or evidence the instance has moved past it
  - Action: Break and wait for next event

#### Step 2.4: Cursor Advancement
```rust
fn advance_cursor(cursor_view, cursor_instance, num_instances) {
    cursor_instance += 1;
    if cursor_instance >= num_instances {
        cursor_instance = 0;
        cursor_view += 1;
    }
}
```

The cursor moves lexicographically: (1,0) → (1,1) → ... → (1,K-1) → (2,0) → (2,1) → ...

## Detailed Example Walkthrough

**Initial State:**
- Instance 0: queue = {}, `finalized_up_to[0] = 0`
- Instance 1: queue = {}, `finalized_up_to[1] = 0`
- Cursor: (1, 0)

**Event 1: Instance 1 finalizes block at view 3**
- Step 1: Extract view = 3
- Step 2: Gap detection: highest_seen_view = 0, new view = 3
  - Gap detected at views 1, 2
  - Insert indirect finalizations: queue[1] = {1: None, 2: None}
- Step 3: Insert block: queue[1] = {1: None, 2: None, 3: Some(block)}
- Cursor loop:
  - Cursor (1,0): No entry, no higher view → break (waiting)
  - Final state: queue[1] = {1: None, 2: None, 3: Some(block)}

**Event 2: Instance 0 finalizes block at view 1**
- Step 1: Extract view = 1
- Step 2: No gap (highest_seen_view = 0, new view = 1)
- Step 3: Insert block: queue[0] = {1: Some(block)}
- Cursor loop:
  - Cursor (1,0): Has `Some(block)` → Finalize block (log block + transactions), set `finalized_up_to[0] = 1`
  - Cursor (1,1): Has `None` → Advance cursor (no logging, views 1 is indirectly finalized)
  - Cursor (2,0): No entry, no higher view → break
  - Finalized: View 1 for instance 0 ✓ (directly), View 1 for instance 1 ✓ (indirectly)

**Event 3: Instance 1 finalizes block at view 5**
- Step 1: Extract view = 5
- Step 2: Gap detection: highest_seen_view = 3, new view = 5
  - Gap detected at view 4
  - Insert indirect finalization: queue[1] = {3: Some(block), 4: None, 5: Some(block)}
- Step 3: Insert block
- Cursor loop:
  - Cursor (2,0): No entry, no higher view → break
  - Final state: queue[1] = {3: Some(block), 4: None, 5: Some(block)}

**Event 4: Instance 0 finalizes block at view 2**
- Step 1: Extract view = 2
- Step 2: No gap (highest_seen_view = 1, new view = 2)
- Step 3: Insert block: queue[0] = {2: Some(block)}
- Cursor loop:
  - Cursor (2,0): Has `Some(block)` → Finalize block (log), set `finalized_up_to[0] = 2`
  - Cursor (2,1): No entry at view 2, but has view 3 > 2 → Create indirect at view 2, advance
  - Cursor (3,0): No entry, no higher view → break
  - Finalized: View 2 for instance 0 ✓ (directly), View 2 for instance 1 ✓ (indirectly)

## Key Properties and Invariants

1. **Strict Ordering**: A block at (v', k') is finalized only after all blocks at (v, k) ≤ (v', k') (lexicographically) are finalized
2. **View Finalization Types**:
   - Directly finalized views: Have a block from consensus, are logged, update `finalized_up_to`
   - Indirectly finalized views: No block, are not logged, don't update `finalized_up_to`
3. **All Blocks Treated Equally**: All blocks are finalized using the same finalization event and logging mechanism
4. **Gap Handling**: Views between observed finalizations are automatically marked as indirectly finalized
5. **Progress Guarantee**: Always processes the top-leftmost unfinalized cell first
6. **finalized_up_to Semantics**: Only tracks directly finalized views (views with blocks); indirectly finalized views don't affect it

## Why This Design Works

- **Cursor ensures strict ordering**: By always processing the top-leftmost cell, we guarantee lexicographic order
- **Gap detection maintains continuity**: When blocks arrive, gaps are immediately filled, preventing inconsistencies
- **finalized_up_to tracks actual progress**: Only views with blocks update it, providing an accurate measure of real progress
- **Indirect finalizations are lightweight**: They maintain state consistency without logging or tracking overhead

## Logging Behavior

- **Blocks at directly finalized views**: Fully logged (block info + all transactions)
- **Indirectly finalized views**: No logs (no block exists to log)

## Code Location

- `chain/src/bin/validator.rs`: `gatling_thread()` implements the cursor-based rule
- `chain/src/engine.rs`: `GatlingEvent` carries `instance_id` and `block`; view is taken from `block.view`
- `chain/src/application/actor.rs`: Sends `GatlingEvent` when blocks are finalized by consensus