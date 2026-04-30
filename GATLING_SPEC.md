# Gatling over Alto — Implementation Specification

This document is a precise implementation guide for porting Gatling on top of a new version of
alto. Read it in full before touching any implementation file.

---

## 0. Monorepo Dependency

All `commonware-*` crates are sourced from the **scaffino monorepo fork** at:

```
https://github.com/scaffino/monorepo.git
```

The current pinned tag is **`v0.0.62`**. In `Cargo.toml` every commonware dependency looks like:

```toml
commonware-consensus = { git = "https://github.com/scaffino/monorepo.git", tag = "v0.0.62" }
```

**When porting to a new alto version:** find the tag of the scaffino monorepo that the new alto
version was built against, and update every `tag = "..."` entry in `[workspace.dependencies]`
to match. Do not mix tags across crates — all commonware crates must use the same tag or you
will get type-mismatch compilation errors at the crate boundaries.

The crates used by Gatling are:
`commonware-broadcast`, `commonware-codec`, `commonware-consensus`, `commonware-cryptography`,
`commonware-deployer`, `commonware-macros`, `commonware-p2p`, `commonware-resolver`,
`commonware-runtime`, `commonware-storage`, `commonware-stream`, `commonware-utils`.

---

## 1. What Gatling Is

Gatling is an atomic broadcast protocol that achieves arbitrarily small inter-proposal times by
running **K independent threshold-Simplex consensus instances in parallel** on the same validator
set. Each instance produces its own per-instance finalized block sequence. A single
**gatling thread** merges all K sequences into one globally-ordered log by imposing a
deterministic interleaving rule based on `(view, instance)` pairs.

In the implementation the underlying consensus library is `commonware_consensus::threshold_simplex`
and the block-storage/sync layer is `commonware_consensus::marshal`. Gatling adds:

- a shared mempool across all K instances,
- per-instance `proposal_offset_ms` staggering,
- a `GatlingEvent` channel from each application actor to a single gatling thread,
- a per-instance height-ordered buffer between the application actor and the gatling thread,
- an ancestor-fetching sub-system to fill gaps caused by finalization proof skipping blocks,
- a `submit-tx` task for self-generated transactions,
- optional P2P gossip for those transactions.

---

## 2. Starting K Consensus Instances

### 2.1 Channel layout

Each instance gets its **own independent set of six P2P channels** allocated as:

```
base_channel = instance_index * 10    (instance_index is 0-based)
pending      = base_channel + 0
recovered    = base_channel + 1
resolver     = base_channel + 2
broadcaster  = base_channel + 3
backfill     = base_channel + 4
ancestor     = base_channel + 5
```

There is one **shared transaction channel** (channel id `TRANSACTION_CHANNEL = 5` — fixed, not
per-instance) used for P2P gossip of raw transactions.

**Why:** Channels are namespaced by integer ID in the authenticated P2P layer. If two instances
share a channel they corrupt each other's consensus messages. The stride of 10 leaves room for
future channels without reassignment.

**Mistake to avoid:** Do not reuse any channel across instances. Do not derive `base_channel`
with a stride smaller than the number of channels per instance.

### 2.2 Engine initialisation

All K `Engine::new(...)` calls are issued **concurrently** via `futures::future::join_all` so that
storage initialisation (which opens freezer files) happens in parallel. Each engine receives:

| Field | Value |
|---|---|
| `partition_prefix` | `"consensus_{chain_id}"` where `chain_id = i+1` (1-based) |
| `namespace` | `"{NAMESPACE}_{chain_id}"` (bytes) |
| `proposal_offset_ms` | `i * 3000 / K` (evenly spaced offsets within the 3-second inter-block time) |
| `gatling_tx` | clone of the single `UnboundedSender<GatlingEvent>` (or `None`) |
| `gatling_instance_id` | `chain_id` (1-based) |
| `shared_mempool` | `Arc<Mutex<Mempool>>` — **same object for all K engines** |
| `instance_views` | `Arc<Vec<AtomicU64>>` — **same object for all K engines** |
| `total_instances` | K |

After `join_all`, the runtime waits for genesis before starting engines. This is done **inside**
the async runtime (using `context.sleep`) so that P2P can bootstrap during the wait window.
**Mistake to avoid:** Do not use a blocking `thread::sleep` before the runtime starts.

After the genesis wait, `engine.start(pending, recovered, resolver, broadcaster, backfill, ancestor)`
is called for each engine. The engine internally starts four sub-actors:

1. **application actor** — builds/verifies/finalises blocks, owns the mempool reference
2. **buffer task** — height-orders blocks before forwarding to gatling
3. **marshal actor** — stores and syncs blocks with peers
4. **consensus actor** — runs threshold-Simplex

All four are joined with `try_join_all`; if any fails the engine exits.

---

## 3. Transaction Pipeline

### 3.1 Shared Mempool

There is a **single `Arc<Mutex<Mempool>>`** created once in `main` and cloned into every engine
and every task that needs to add or read transactions. All K instances on a given validator read
from the same pool.

`Mempool` is a FIFO structure backed by a `HashMap<Digest, Transaction>` for deduplication and a
`VecDeque<Digest>` for arrival order. Key methods:

- `add(tx)` — inserts only if digest is not already present; updates the gauge metric.
- `next()` — pops and returns the front transaction (FIFO); returns `None` if empty.
- `take_all()` — drains and returns all transactions in arrival order.

**Why a shared pool:** Each instance can independently include transactions in its block. Because
all K instances share the same pool and call `next()` destructively, each transaction is included
in **exactly one** block across all instances on this validator. This prevents duplicate
inclusions from the same validator while maximising throughput.

**Mistake to avoid:** Do not give each instance its own separate mempool. If you do, the same
transaction will appear in multiple blocks.

### 3.2 Self-generated Transactions (`submit-tx`)

Enabled via `--submit-tx <rate> <start_delay> <duration>`. The `submit_tx` task:

1. Sleeps until `genesis_timestamp + start_delay` seconds.
2. Enters a loop: generates a transaction every `1/rate` seconds with ±50 % uniform jitter
   (`sleep_duration = target_interval * uniform(0.5, 1.5)`).
3. Creates `Transaction::sign(&signer, receiver=own_public_key, amount=1, timestamp=now_ms)`.
4. Calls `shared_mempool.lock().unwrap().add(tx.clone())`.
5. If gossip is enabled, sends `tx` to the `broadcast_channel` (an `UnboundedSender<Transaction>`).
6. Stops after `duration` seconds.

The transaction carries a wall-clock millisecond timestamp in its `timestamp` field. This is used
later to compute end-to-end latency at finalisation.

**Mistake to avoid:** Do not generate transactions before genesis. Do not use a blocking sleep;
use `ctx.sleep` (the runtime-provided async sleep).

### 3.3 Optional P2P Gossip

Controlled by `--gossip-txs` / `--no-gossip-txs` (default: enabled).

When enabled, two background tasks are spawned:

**tx_broadcast task:** reads from `broadcast_rx` (the receiver side of the `broadcast_channel`)
and calls `tx_sender.send(Recipients::All, tx_bytes, true)` over the shared transaction channel.

**tx_gossip task:** calls `tx_receiver.recv()` in a loop on the same shared transaction channel,
decodes the raw bytes as `Transaction`, and calls `shared_mempool.lock().unwrap().add(tx)`.

**Why separate tasks:** The encode/send and decode/receive paths are independent and must not
block each other or the consensus main loop.

**Mistake to avoid:** Do not route received gossip transactions through a per-instance application
mailbox — add them directly to the shared mempool. Do not use the transaction channel id that
overlaps with any consensus instance's channel range.

---

## 4. Block Proposal Timing (`tproposal`)

Each instance waits before sending its proposal so that instances are time-staggered within
each view. The target proposal time for instance `k` (1-based) in view `v` is:

```
tproposal(genesis_ts, K, k, v) = genesis_ms + v * 3000 + (k-1) * 3000 / K   [ms]
```

When consensus calls `Propose`, the application actor:

1. Immediately pulls up to `MAX_BLOCK_TRANSACTIONS` from the mempool (first pickup).
2. Resolves the parent block from marshal.
3. Waits until `tproposal` if it is in the future; skips the wait if `now >= tproposal`
   (catch-up mode).
4. Pulls additional transactions from the mempool after the wait (second pickup, up to cap).
5. Builds the final block with all collected transactions.
6. Sends the block digest back to consensus.

**Why two pickups:** The wait window between building the block and proposing it allows
transactions that arrived during the stagger window to be included. Without the second pickup,
transactions generated during the offset window are always missed.

**Mistake to avoid:** Do not build the block *after* the wait — build it immediately so the
parent digest is resolved early and does not delay the proposal. Do not set `block.view` from
the finalization proof's view; use the `view` argument passed by consensus to `Propose`.

---

## 5. Finalization Path: From Consensus to the Gatling Thread

This section describes the full path a block travels from consensus finalization to appearing in
the global Gatling log.

### 5.1 FinalizationPusher

`FinalizationPusher` implements the `Reporter<Activity>` trait. When consensus reports a
`Finalization`, it:

1. Extracts `view` from the finalization proof.
2. Calls `marshal.subscribe(Some(view), payload_digest).await.await` to fetch the block from
   local storage (blocking until it arrives).
3. Sends `Message::Finalized { view, block }` to the application actor's mailbox.

**Why subscribe with `Some(view)`:** The view hint allows marshal to efficiently locate the block
in the prunable section rather than doing a full scan.

### 5.2 Application Actor: `Message::Finalized`

When the actor receives `Message::Finalized { view, block }`:

1. **Persistence task** (spawned): calls `marshal.verified(view, block)` to ensure the block is
   in marshal storage even if this validator did not verify it locally during the consensus round.
   This is idempotent.

2. **Ancestor finalization task** (spawned): walks the ancestor chain of the block, fetches any
   missing ancestors, then sends all ancestors followed by the triggering block to `buffer_tx`
   (see §6). **The triggering block is sent by this task at the end, not by the main actor body.**

3. Updates `instance_views[instance_idx]` with the new view (used for lag detection).

4. If gatling is **not** enabled, logs individual transaction finalizations here.

**Mistake to avoid:** Do not send the block directly to `gatling_tx` from the actor. The path is
always actor → buffer task → gatling thread, ensuring height ordering before the gatling thread
sees any event.

### 5.3 Per-Instance Buffer Task (`run_buffer`)

One buffer task per instance. Receives `Block` from `buffer_tx` and forwards
`GatlingEvent { instance_id, view: block.view, block }` to the shared `gatling_tx` channel only
when blocks arrive in contiguous height order.

State:
- `next_expected_height: u64 = 1`
- `pending: BTreeMap<u64, Block>` — out-of-order arrivals
- `seen: HashSet<Vec<u8>>` — deduplicate by block digest

Algorithm:
1. Receive block; skip if digest already seen.
2. Insert into `pending` at `block.height` (do not overwrite if key already exists).
3. Loop: if `pending` contains `next_expected_height`, remove it, send `GatlingEvent`, increment
   `next_expected_height`, repeat; else break.

**Why this buffer exists:** The application actor spawns a separate `finalize_ancestors` task per
finalization event. These tasks can complete in any order, delivering ancestor blocks
out-of-height-order to `buffer_tx`. The buffer guarantees the gatling thread always receives
events in ascending height order per instance.

**Mistake to avoid:** Do not send events to `gatling_tx` directly from the actor or from the
ancestor finalization task — always go through `buffer_tx` → `run_buffer`. Do not use
`block.view` as the height — use `block.height`.

---

## 6. Ancestor Fetching

When threshold-Simplex finalizes view `v`, it may have skipped views `v-1`, `v-2`, … Each
skipped view may have had a different block proposed and notarized that is the parent chain of the
finalized block. These blocks are not delivered by consensus but must be included in the output
log.

The `finalize_ancestors` task is spawned for each `Message::Finalized`:

### 6.1 Iterative ancestor chain walk (`'fetch_loop`)

The task uses an outer `'fetch_loop` loop to discover the ancestor chain one level at a time.
This is necessary because each block's parent digest is only known once the block itself is fetched —
without fetching block at height N, the digest of block at height N-1 is unknown.

Constants (defined inside the task):
```
PEER_RESPONSE_TIMEOUT_MS = 2_000   (per peer attempt)
LOCAL_CHECK_TIMEOUT_MS   =   500   (marshal lookup; covers archive restoration ~200ms)
MAX_TOTAL_FETCH_MS       = 60_000  (overall wall-clock budget, ~2× LEADER_TIMEOUT)
INITIAL_RETRY_DELAY_MS   =   200
MAX_RETRY_DELAY_MS       = 5_000
```

Loop body:
1. Walk from `start_parent` through `found_blocks` (following `.parent` links) until hitting an
   unknown digest (the "frontier"). If the walk reaches `genesis_digest` or a digest in
   `finalized_seen` without finding an unknown, exit the loop — done.
2. For the frontier digest, call `marshal.subscribe(None, digest)` with `LOCAL_CHECK_TIMEOUT_MS`.
   - Found locally → insert into `found_blocks`, go to step 1 (walk deeper).
   - Not found → proceed to network fetch.
3. Check total elapsed time against `MAX_TOTAL_FETCH_MS`; if exceeded, warn and exit loop.
4. Fetch the frontier digest from peers:
   - Cycle through all validator participants round-robin.
   - For each attempt: allocate a `request_id`, register `request_id → {digest → oneshot_sender}`
     in `pending_ancestor_requests`, encode and send `AncestorFetcher::Request` to one peer via
     `Recipients::One(peer)`, wait up to `PEER_RESPONSE_TIMEOUT_MS` for the oneshot receiver.
   - On success: verify `block.digest() == frontier_digest`, call `marshal.verified(block.view, block)`,
     insert into `found_blocks`, go to step 1 (walk deeper to discover the next level).
   - On timeout/failure: apply exponential backoff, try next peer.
   - If `MAX_TOTAL_FETCH_MS` is exceeded during fetch: warn "partial reconstruction", exit loop.
5. If the fetch loop exits without finding the frontier block: warn "partial reconstruction".

**Why iterative:** In catch-up mode a validator may be missing a chain of depth D (e.g., received
block at height 50, needs 1–49). Without the iterative approach, only one level is discovered per
invocation, and height 1 is never reached for deep chains. The `'fetch_loop` correctly handles
chains of any depth.

**Mistake to avoid:** Do not break out of the walk after the first unknown digest and then try to
fetch all missing blocks. You cannot know the digest of height N-1 until you have fetched height N.
The walk and fetch must interleave.

### 6.2 Pending request tracking

For each peer fetch attempt:

1. Allocate `request_id` from `request_id_counter` (`AtomicU32`, `Relaxed` ordering).
2. Create a `oneshot::channel::<Block>()` pair `(resp_tx, resp_rx)`.
3. Register `pending_ancestor_requests[request_id] = HashMap::from([(digest, resp_tx)])`.
4. Send `AncestorFetcher::Request(Request { request_id, digests: vec![digest] })` to peer.
5. Select on `resp_rx` with `PEER_RESPONSE_TIMEOUT_MS` timeout.
6. Always remove `request_id` from `pending_ancestor_requests` after the select completes
   (whether via response or timeout) to prevent stale entries.

### 6.3 Ancestor message handler (background task)

Spawned once at actor startup, runs for the lifetime of the actor. Listens on `ancestor_receiver`:

- **Request messages:** look up each requested digest in marshal (`LOCAL_CHECK_TIMEOUT_MS = 500ms`
  timeout each); send back an `AncestorFetcher::Response` to the requesting peer with any found blocks.
- **Response messages:** look up `response.id` in `pending_ancestor_requests`; for each returned
  block, match against the digest map and fire the oneshot sender.

**Why a shared `pending_ancestor_requests` map:** Multiple `finalize_ancestors` tasks may be
active simultaneously. A single message handler demultiplexes responses to the correct task via
the request ID.

**Mistake to avoid:** Do not forget to remove a request from `pending_ancestor_requests` after
it completes or times out — a leaked entry will prevent GC and cause the response handler to send
on a closed oneshot.

### 6.4 Reconstruction and forwarding

After `'fetch_loop` exits, `found_blocks` contains all reachable ancestors. The task then:

1. Walks `start_parent → found_blocks[start_parent].parent → …` until genesis or `finalized_seen`,
   collecting blocks into `final_ancestors` in reverse order (oldest-first after `.reverse()`).
   If a digest is not in `found_blocks` (partial reconstruction), log a warning and stop the walk.
2. For each ancestor block in ascending height order:
   - Send to `buffer_tx`.
   - Insert digest into `finalized_seen`; if new, increment counter and record latency histogram.
3. **After all ancestors**, send the triggering block itself to `buffer_tx`. This ensures
   `run_buffer` always receives heights in strictly ascending order (1, 2, …, N) for that instance.
   The triggering block must be last — never sent before its ancestors.

**Why `finalized_seen`:** The same block digest can arrive via both the direct `Message::Finalized`
path and the ancestor chain of a later block. The set prevents double-counting and double-forwarding.

**Why triggering block is sent last (Fix 1 invariant):** `run_buffer` starts at
`next_expected_height = 1` and cannot advance until height 1 is received. If the triggering block
(height N) were sent before its ancestors (heights 1..N-1), `run_buffer` would buffer it and stall
waiting for height 1. Ancestors must arrive before the block that triggered their fetch.

---

## 7. Global Log Reconstruction (Gatling Thread)

This is the core of Gatling. Read carefully.

### 7.1 Conceptual model

Think of a matrix with K rows (one per instance, 0-indexed) and an infinite column axis of view
numbers (1, 2, 3, …). Each cell `(instance, view)` is either:

- **occupied**: the instance finalized a block at that view,
- **skipped**: the instance advanced past that view without a block (gap),
- **pending**: not yet known.

The globally ordered log is the sequence of occupied cells read in **lexicographic order of
`(view, instance)`**: all instance cells for view 1 before view 2, within each view from instance
0 to instance K-1.

### 7.2 Data structures

```rust
// Per-instance BTreeMap<view, Option<Block>>
// Some(block) = directly finalized view (block exists)
// None        = indirectly finalized view (gap, no block)
instance_queues: Vec<BTreeMap<u64, Option<Block>>>

// Per-instance highest directly finalized view seen so far
finalized_up_to: Vec<u64>  // initialised to 0

// Cursor: next unprocessed cell in lexicographic (view, instance) order
cursor_view: u64     // starts at 1
cursor_instance: usize  // starts at 0
```

### 7.3 Receiving a GatlingEvent

For each `GatlingEvent { instance_id, block }` received on `gatling_rx`:

1. Convert `instance_id` to 0-based: `idx = instance_id - 1`.
2. Read `view = block.view` (**from the block, not from the event's separate `view` field**).
3. Compute `highest_seen_view = max(finalized_up_to[idx], max key in instance_queues[idx])`.
4. If `view > highest_seen_view + 1` and `highest_seen_view > 0`: fill views
   `highest_seen_view+1 .. view` with `None` in `instance_queues[idx]` (indirect finalization).
5. Insert `instance_queues[idx][view] = Some(block)`.
6. Enter the cursor drain loop.

**Mistake to avoid:** Do not read `view` from the `GatlingEvent.view` field alone — the buffer
task copies `block.view` there, but the authoritative source is `block.view`. Importantly, do not
confuse the finalization proof's view with the block's view. Alto's consensus reports a
`Finalization` whose `view` is the view in which the finalization proof was collected; the
associated block was *proposed* in a (possibly earlier) view. **Always use `block.view`.**

### 7.4 Cursor drain loop

After every insertion, execute this loop until it cannot make progress:

```
loop:
  if cursor_view == 0 or cursor_instance >= K: break

  if finalized_up_to[cursor_instance] >= cursor_view:
    // Already processed (possible after state update)
    advance_cursor(); continue

  match instance_queues[cursor_instance].remove(cursor_view):
    Some(Some(block)):
      // Directly finalized view — emit to output log
      log "[gatling] Validator {V} finalized block {block.height} from instance {instance_id}
           (view {cursor_view}) with {N} transactions"
      for each tx: log "[gatling] Transaction {id} (timestamp: {ms}) is now final in
                        block {H} from instance {I} (view {V})"
      finalized_up_to[cursor_instance] = cursor_view
      // Clean up stale entries below cursor_view
      remove all keys < cursor_view from instance_queues[cursor_instance]
      advance_cursor(); continue

    Some(None):
      // Indirectly finalized view — skip silently, do not update finalized_up_to
      advance_cursor(); continue

    None:
      // No entry. Check if instance has any entry with key > cursor_view
      if instance_queues[cursor_instance].has_key_gt(cursor_view):
        // Instance moved past this view without a block
        insert (cursor_view, None) into instance_queues[cursor_instance]
        advance_cursor(); continue
      else:
        // Genuinely waiting — cannot make progress
        break
```

`advance_cursor`:
```rust
cursor_instance += 1;
if cursor_instance >= K {
    cursor_instance = 0;
    cursor_view += 1;
}
```

### 7.5 Why this algorithm is correct

- **Lexicographic order** over `(view, instance)` guarantees a deterministic total order that
  is identical on every validator regardless of the order events arrive.
- **Indirect finalization (`None` entries)** handles the fact that threshold-Simplex can skip
  views. If instance 0 jumps from view 3 to view 7, views 4–6 have no blocks. The algorithm
  fills those with `None` so the cursor can advance past them.
- **The "has higher view" check** (the `None` branch) is how the cursor learns that a view was
  skipped without waiting forever. When a block at view `V+3` arrives, the cursor can infer that
  views `V+1` and `V+2` are gaps and mark them `None` so it can advance.
- **`finalized_up_to` is only updated for `Some(block)` entries**, not for `None` entries. This
  is correct: `finalized_up_to[k]` means "all views up to this value for instance k have been
  processed (possibly as gaps)". Updating it for gaps too would be fine semantically but the
  current implementation only updates it on direct blocks.

### 7.6 Critical mistakes to avoid

1. **Do not use the finalization proof's view as the block view.** The cursor indexes by
   `block.view`. A proof at view `V` may contain a block proposed at view `V-2`. If you store it
   under `V` instead of `V-2` the ordering becomes inconsistent across validators.

2. **Do not update `finalized_up_to` when processing a `None` entry.** `None` means "gap, no
   block". If you advance `finalized_up_to` for it you will silently swallow direct blocks that
   arrive later for the same view.

3. **Do not emit the `None` entry to the log.** Only `Some(block)` entries produce output.

4. **Do not skip the cursor drain loop after inserting a `None`.** A `None` fill triggers
   cursor progress that may unblock many subsequent entries.

5. **Do not make the cursor advance globally uniform without the gap detection.** Without the
   "has higher view" check, if view `V` for instance `k` is forever absent (never got a block,
   instance skipped it) the cursor stalls permanently.

6. **Do not advance the cursor across instances in the wrong order.** The order must be instance
   0, 1, …, K-1 for view V, then instance 0, 1, …, K-1 for view V+1. Swapping view and instance
   axes breaks the global order.

---

## 8. Thread / Task Map

```
main
├── p2p (network task)
├── submit_tx task                [§3.2]  → shared_mempool + broadcast_channel
├── tx_broadcast task             [§3.3]  ← broadcast_channel → p2p tx channel
├── tx_gossip task                [§3.3]  ← p2p tx channel → shared_mempool
├── gatling thread                [§7]    ← gatling_rx (UnboundedReceiver<GatlingEvent>)
└── for each instance i in 0..K:
    └── Engine::run (joined internally)
        ├── application actor     [§5.2]  ← consensus mailbox messages
        │   ├── propose subtasks  [§4]
        │   ├── verify subtasks
        │   ├── persist_finalized subtasks
        │   ├── finalize_ancestors subtasks  [§6]  → buffer_tx
        │   └── ancestor_message_handler    [§6.3] ← ancestor p2p channel
        ├── run_buffer task       [§5.3]  ← buffer_tx → gatling_tx
        ├── marshal actor
        └── consensus actor
```

### Key channel connections

| Channel | Direction | Parties |
|---|---|---|
| `shared_mempool` | shared read/write (Mutex) | submit_tx, tx_gossip, all K application actors (Propose handler) |
| `broadcast_channel` (mpsc) | submit_tx → tx_broadcast | one producer, one consumer |
| `p2p tx channel` | tx_broadcast → network → tx_gossip | P2P transport |
| `application mailbox` (mpsc) | consensus → application actor | per-instance |
| `buffer_tx` (unbounded mpsc) | application actor + finalize_ancestors → run_buffer | per-instance |
| `gatling_tx` (unbounded mpsc) | all K run_buffer tasks → gatling thread | K producers, 1 consumer |
| `ancestor p2p channel` | bidirectional: ancestor requests/responses | per-instance, via ancestor_message_handler |

---

## 9. Configuration Reference

Key fields added to `engine::Config` / `application::Config` beyond base alto:

| Field | Type | Purpose |
|---|---|---|
| `proposal_offset_ms` | `u64` | Stagger offset for `tproposal` (ms); instance `i` gets `i*3000/K` |
| `gatling_tx` | `Option<UnboundedSender<GatlingEvent>>` | If `Some`, enables gatling path |
| `gatling_instance_id` | `usize` | 1-based instance ID, embedded in `GatlingEvent` |
| `instance_views` | `Arc<Vec<AtomicU64>>` | Per-instance latest view, shared across engines |
| `lag_threshold` | `u64` | Views behind before skipping scheduled wait (lag detection) |
| `total_instances` | `usize` | K, used in `tproposal` formula |
| `genesis_timestamp_secs` | `u64` | Absolute genesis time for `tproposal` formula |
| `ancestor_fetch_concurrent` | `usize` | Max digests per ancestor fetch request batch |
| `ancestor_fetch_rate_per_peer` | `Quota` | Rate limit for ancestor fetch channel |
| `shared_mempool` | `Arc<Mutex<Mempool>>` | Shared transaction pool |

---

## 10. `GatlingEvent` Type

```rust
pub struct GatlingEvent {
    pub instance_id: usize,  // 1-based instance number
    pub view: View,          // = block.view (copied from block by run_buffer)
    pub block: Block,        // the finalized block
}
```

The `view` field in `GatlingEvent` is redundant with `block.view` — they must be equal. The
gatling thread should read `event.block.view` as the authoritative view number to insert into
`instance_queues`.

---

## 11. Logging Convention

There are two categories of log lines produced by Gatling: **per-instance logs** from the
application actor and **global log lines** from the gatling thread.

### 11.1 Per-instance application actor logs

Every consensus instance emits these lines unconditionally (whether gatling is enabled or not),
prefixed with the engine ID `[consensus_{N}]` where `N` is the 1-based instance number:

```
[consensus_{N}] Validator {validator_index} finalized block {height} (view {v}) with {tx_count} transactions
```

Emitted once for every block finalized by instance `N`, including ancestor blocks recovered via
`finalize_ancestors`. This is the primary signal that a specific consensus instance has finalized
a block at a particular view.

When gatling is **disabled** (no `--gatling` flag), per-transaction finalization is also logged
per instance:

```
[consensus_{N}] Transaction {digest:?} (timestamp: {ms} ms) is now final in block {height} (view {v})
```

These lines identify the instance by the `[consensus_{N}]` prefix. The view number here is
`block.view` (the view in which the block was proposed), not the finalization proof view.

### 11.2 Gatling thread logs

When gatling is enabled, the gatling thread emits one line per block that enters the global log
and one line per finalized transaction, both prefixed `[gatling]`:

```
[gatling] Validator {idx} finalized block {height} from instance {instance_id} (view {v}) with {N} transactions
[gatling] Transaction {digest:?} (timestamp: {ms} ms) is now final in block {height} from instance {instance_id} (view {v})
```

These lines are consumed by the postprocessing scripts (`extract_gatling_logs.sh`,
`verify_gatling_logs.py`, `generate_csv.py`). Do not change the format without updating those
scripts.

### 11.3 Relationship between the two categories

For every `[gatling]` block line there will be a corresponding `[consensus_{N}]` finalized block
line emitted earlier by the application actor (possibly out of global order). The `[gatling]`
lines impose the deterministic lexicographic `(view, instance)` order; the `[consensus_{N}]`
lines reflect the raw per-instance finalization order as it happened. Both reference the same
`block.view` and `block.height`.
