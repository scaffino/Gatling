# Ancestor Fetching Mechanism

## Overview

When a validator starts after genesis (`overrun_ms > 0`), it may receive a finalized block at height N
without having heights 1..N-1 in local storage. The `finalize_ancestors` task fetches the full ancestor
chain before forwarding blocks to `run_buffer`, which enforces that the gatling thread sees each
instance's blocks in ascending height order.

**Why this matters for correctness**: `run_buffer` per instance buffers blocks and only forwards them
once it has seen every height starting from 1 in order. If height 1 is never delivered, the gatling
thread cursor stalls permanently on that instance — even if all other instances have finalized
thousands of blocks.

## Architecture

### Components

1. **`finalize_ancestors` task** (`actor.rs`)  
   Spawned on every `Message::Finalized` event. Walks the ancestor chain from `block.parent` back to
   genesis (or a previously-finalized ancestor), fetching any missing blocks, then forwards them to
   `run_buffer` in ascending height order followed by the triggering block.

2. **`run_buffer` task** (`actor.rs`)  
   One per consensus instance. Receives blocks from `finalize_ancestors` and buffers them until it
   can forward a contiguous sequence starting from `next_expected_height = 1` to the gatling thread.

3. **Ancestor channel** (per-instance P2P channel)  
   Used for peer-to-peer ancestor request/response messages.
   - Channel number: `base_channel + 6` (where `base = instance_id * 10`)
   - Rate limit: 16 messages/second

4. **Ancestor message handler** (background task per actor)  
   Receives incoming `AncestorFetcher::Request` messages from peers, looks up requested blocks in
   marshal local storage, and responds with `AncestorFetcher::Response`.

5. **`pending_ancestor_requests`** (`Arc<Mutex<HashMap<u32, HashMap<Digest, oneshot::Sender<Block>>>>>`)  
   Shared map from `request_id` → (digest → sender). Allows the message handler to route incoming
   responses to the correct waiting `finalize_ancestors` task.

## Message Protocol

Messages on the ancestor channel are encoded as `AncestorFetcher` enum variants:

```rust
enum AncestorFetcher {
    Request(Request),   // sent by the validator needing a block
    Response(Response), // sent back by the peer that has it
}

struct Request {
    request_id: u32,
    digests: Vec<Digest>,
}
```

Responses carry the actual `Block` and the `request_id` so the receiver can route to the correct
pending entry.

## Iterative Chain Walk Algorithm

The `finalize_ancestors` task uses an outer `'fetch_loop` loop to discover ancestor blocks
level-by-level. This is necessary because each block's parent digest is only known once the block
itself is fetched.

```
'fetch_loop:
  1. Walk from start_parent through already-found blocks (found_blocks map)
     following .parent links until hitting an unknown digest (the "frontier").
  2. If no frontier → walked to genesis or a previously-finalized block → done.
  3. Check marshal local storage for the frontier digest (500ms timeout).
     - Found locally → insert into found_blocks, loop back to step 1.
     - Not found → go to step 4.
  4. Check global timeout (MAX_TOTAL_FETCH_MS = 60s). If exceeded → warn and stop.
  5. Fetch the frontier digest from a peer (with per-peer 2s timeout, exponential backoff).
     - Success → insert into found_blocks, loop back to step 1 to discover the next level.
     - All retries exhausted → warn "partial reconstruction", stop loop.
```

After the loop, `found_blocks` is reconstructed into ascending-height order and forwarded
to `run_buffer`.

### Why iterative (not recursive / pre-walk)?

A validator in catch-up mode may be missing a chain of depth D (e.g., received block at height 50,
needs heights 1–49). Without knowing each block's parent digest in advance, the chain can only be
discovered one level at a time: fetch height 49 → learn its parent = height 48 → fetch height 48 → …
The `'fetch_loop` handles this correctly regardless of chain depth.

## Constants

```rust
const PEER_RESPONSE_TIMEOUT_MS: u64 = 2_000;  // Timeout per peer attempt
const LOCAL_CHECK_TIMEOUT_MS:    u64 =   500;  // Marshal local lookup (covers archive restoration)
const MAX_TOTAL_FETCH_MS:        u64 = 60_000; // Overall budget (~2× LEADER_TIMEOUT)
const INITIAL_RETRY_DELAY_MS:    u64 =   200;  // First backoff delay
const MAX_RETRY_DELAY_MS:        u64 = 5_000;  // Backoff cap
```

**Why `LOCAL_CHECK_TIMEOUT_MS = 500ms`?**  
When a block is in marshal's archive (freezer), restoration can take ~200ms per engine. A 500ms
timeout covers this without falsely treating archived blocks as missing.

**Why `MAX_TOTAL_FETCH_MS = 60s` instead of a retry count?**  
Retrying N times against a slow or partitioned peer could stall indefinitely. A wall-clock budget
bounds the worst-case stall regardless of how many peers/retries are attempted.

## Peer Selection

The task cycles through all validator participants round-robin, trying one peer per attempt.
Backoff applies when a peer does not respond within `PEER_RESPONSE_TIMEOUT_MS`. This distributes
load and avoids hammering a single slow peer.

## Forwarding Order to `run_buffer`

After reconstruction, blocks are sent in strictly ascending height order:

```
ancestor[height=1] → ancestor[height=2] → ... → ancestor[height=N-1] → block[height=N]
```

`run_buffer` starts at `next_expected_height = 1` and stalls if any height is missing. Sending
in ascending order, with the triggering block last, ensures `run_buffer` can drain its buffer
completely on the first delivery.

## Startup Behaviour (catch-up mode)

When `overrun_ms > 0` (validator process started after genesis), consensus immediately catches
up to the current view. The first finalized block will likely be at height > 1. The first
`finalize_ancestors` invocation must fetch the entire chain back to height 1.

A peer will only respond if it already has the requested block in its marshal storage. Blocks
finalized before the late validator started are guaranteed to be present on honest peers that
ran from genesis.

## Error Handling

| Situation | Behaviour |
|---|---|
| Peer timeout | Back off, try next peer in rotation |
| Global timeout exceeded | Log warning, partial reconstruction forwarded |
| Digest mismatch on response | Log warning, discard block, retry |
| No peers available | Log warning, stop loop |
| Send failure to peer | Retry immediately with next peer |
| Partial reconstruction | `run_buffer` stalls waiting for the missing height; the gatling cursor for that instance will remain blocked until the block is eventually fetched via a future finalization event |

## Serving Ancestor Requests

The `ancestor_message_handler` task (spawned at actor startup) handles incoming requests:

1. Decode as `AncestorFetcher::Request`.
2. For each requested digest, call `marshal.subscribe(None, digest)` with `LOCAL_CHECK_TIMEOUT_MS`.
3. If found: reply with `AncestorFetcher::Response` containing the block.
4. If not found: do not reply (the requester will timeout and retry another peer).

## Channel Registration

```rust
// validator.rs
const ANCESTOR_CHANNEL: u32 = 6;
let ancestor_channel = base_channel + ANCESTOR_CHANNEL; // base = instance_id * 10
let ancestor = network.register(
    ancestor_channel,
    Quota::per_second(NonZeroU32::new(16).unwrap()),
    config.message_backlog
);
```

## Comparison with Marshal Backfill

| Aspect | Ancestor Channel | Marshal Backfill |
|---|---|---|
| Purpose | Fetch ancestors for finalization ordering | General missing-block recovery |
| Trigger | Explicit: `finalize_ancestors` task | Automatic: `marshal.subscribe()` miss |
| Scope | Ancestor chain of a specific finalized block | Any missing block |
| Peer targeting | One peer at a time (round-robin) | Internal to marshal |
| Local check timeout | 500ms (covers archive restoration) | Full backfill duration |
| On failure | Partial reconstruction, logged | Backfill retries indefinitely |
