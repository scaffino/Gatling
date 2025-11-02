# Ancestor Fetching Mechanism

## Overview

When a block is finalized, the system recursively fetches and finalizes all missing ancestor blocks in the chain. The ancestor fetching mechanism uses two sources in order:
1. **Marshal local storage** (checked first, with short timeout to avoid triggering backfill)
2. **Peer requests via ancestor channel** (if not found locally)

This ensures efficient block retrieval without interfering with marshal's backfill mechanism.

## Architecture

### Components Involved

1. **Application Actor** (`chain/src/application/actor.rs`)
   - Spawns `finalize_ancestors` task when a block is finalized
   - Manages ancestor fetching logic

2. **Ancestor Channel** (Per-instance P2P channel)
   - Channel number: `base_channel + 6` (where base = instance_id * 10)
   - Rate limit: 16 messages/second
   - Used for peer-to-peer ancestor requests/responses

3. **Marshal** (`marshal::Actor`)
   - Provides local block storage
   - Used for quick local lookup (10ms timeout to avoid backfill)

4. **Ancestor Message Handler** (background task)
   - Handles incoming ancestor requests from peers
   - Handles incoming ancestor responses
   - Runs continuously throughout actor lifetime

## Workflow

### High-Level Flow

```
Block Finalized
    │
    ▼
Spawn finalize_ancestors Task
    │
    ├─→ For each missing ancestor:
    │       │
    │       ├─→ 1. Check Marshal Local Storage (10ms timeout)
    │       │       ├─→ Found? → Use block, continue chain
    │       │       └─→ Not found? → Continue to step 2
    │       │
    │       └─→ 2. Request from Peers (via ancestor channel)
    │               ├─→ Send Digest request to all peers
    │               ├─→ Wait for Block response
    │               ├─→ Verify digest matches
    │               ├─→ Store in marshal
    │               └─→ Continue ancestor chain
    │
    └─→ Finalize ancestors in ascending height order
```

## Detailed Mechanism

### 1. Trigger: Block Finalization

When consensus finalizes a block, the `FinalizationPusher` sends a `Message::Finalized` to the Application Actor:

```rust
Message::Finalized { view, block }
```

The Actor processes this message and spawns the `finalize_ancestors` task:

```rust
self.context.with_label("finalize_ancestors").spawn(move |context| async move {
    // Ancestor fetching logic here
});
```

### 2. Ancestor Chain Discovery

The task walks up the parent chain starting from `block.parent`:

```rust
let mut missing_ancestors: Vec<Block> = Vec::new();
let mut cursor_digest = block.parent;

while cursor_digest != genesis_digest {
    // Skip if already finalized
    if finalized_seen.contains(&cursor_digest) {
        break;
    }
    
    // Fetch ancestor block
    // ... (see step 3)
    
    // Continue up chain
    cursor_digest = ancestor.parent;
    missing_ancestors.push(ancestor);
}
```

### 3. Fetching Each Ancestor

For each missing ancestor, the system attempts to fetch it using a two-step process:

#### Step 3a: Check Marshal Local Storage

First, check if the block is immediately available in marshal's local storage:

```rust
// Use very short timeout to check if block is immediately available locally
// If timeout triggers quickly, block is not in local storage, skip backfill
let subscribe_fut = marshal.subscribe(None, cursor_digest).await;
let local_check_timeout = Duration::from_millis(10); // 10ms

let local_check_result = select!(
    result = subscribe_fut => {
        match result {
            Ok(ancestor) => Some(ancestor),
            Err(_) => None,
        }
    },
    _ = context.sleep(local_check_timeout) => {
        // Timeout quickly - block not in local storage, skip backfill
        None
    }
);
```

**Why 10ms timeout?**
- If the block is in local storage, `marshal.subscribe()` returns immediately (< 1ms)
- If not in local storage, marshal would trigger backfill which takes time
- The 10ms timeout prevents marshal backfill from being triggered
- If timeout occurs, we know block is not locally available and proceed to peer request

#### Step 3b: Request from Peers via Ancestor Channel

If marshal doesn't have the block locally, request it from peers:

```rust
// Create oneshot channel for response
let (response_tx, response_rx) = oneshot::channel();

// Register pending request
{
    let mut pending = pending_ancestor_requests.lock().unwrap();
    pending.insert(cursor_digest, response_tx);
}

// Send request to all peers
let request_bytes = Bytes::from(cursor_digest.encode().to_vec());
ancestor_sender.send(Recipients::All, request_bytes, true).await;

// Wait for response (no timeout - waits indefinitely)
let response_result = response_rx.await.ok();

// Remove from pending
pending_ancestor_requests.remove(&cursor_digest);
```

**Message Format**:
- **Request**: Encoded `Digest` (block digest being requested)
- **Response**: Encoded `Block` (the actual block data)

### 4. Handling Incoming Requests (Serving Peers)

A background task continuously handles incoming ancestor messages:

```rust
// Spawn task to handle incoming ancestor messages (both requests and responses)
self.context.with_label("ancestor_message_handler").spawn(move |_| async move {
    loop {
        match ancestor_receiver.recv().await {
            Ok((_peer, message_bytes)) => {
                // Try to decode as a block first (response)
                match Block::decode(message_bytes.as_ref()) {
                    Ok(block) => {
                        // This is a block response - fulfill pending request
                        let block_digest = block.digest();
                        if let Some(response_tx) = pending_requests.remove(&block_digest) {
                            response_tx.send(block);  // Send to waiting task
                        }
                    }
                    Err(_) => {
                        // Not a block, try decoding as a digest (request)
                        match Digest::decode(message_bytes.as_ref()) {
                            Ok(requested_digest) => {
                                // Check marshal local storage (10ms timeout)
                                match marshal.subscribe(None, requested_digest).await {
                                    Ok(block) => {
                                        // We have it - send block response
                                        let block_bytes = Bytes::from(block.encode().to_vec());
                                        ancestor_sender.send(Recipients::All, block_bytes, true).await;
                                    }
                                    Err(_) => {
                                        // Don't have it - ignore request
                                    }
                                }
                            }
                            Err(_) => {
                                // Invalid message - ignore
                            }
                        }
                    }
                }
            }
            Err(_) => break,  // Receiver closed
        }
    }
});
```

### 5. Processing Responses

When a block response is received:

```rust
match response_result {
    Some(block) => {
        // Verify digest matches
        if block.digest() == cursor_digest {
            // Store in marshal
            marshal.verified(block.view, block.clone()).await;
            ancestor_opt = Some(block);
            break;  // Success - continue to next ancestor
        } else {
            warn!("Received ancestor block with mismatched digest");
        }
    }
    None => {
        // No response received (channel closed or cancelled)
        // Will retry with exponential backoff
    }
}
```

### 6. Retry Logic

If fetching from peers fails, the system retries with exponential backoff:

```rust
const MAX_RETRIES: usize = 5;
const INITIAL_RETRY_DELAY_MS: u64 = 100;
const MAX_RETRY_DELAY_MS: u64 = 1000;

// Retry loop
while ancestor_opt.is_none() && retry_count <= MAX_RETRIES {
    // Try marshal local storage (step 3a)
    // If not found, try peer request (step 3b)
    
    if ancestor_opt.is_none() {
        // Exponential backoff before retry
        let delay_ms = (INITIAL_RETRY_DELAY_MS * (1 << retry_count))
            .min(MAX_RETRY_DELAY_MS);
        context.sleep(Duration::from_millis(delay_ms)).await;
        retry_count += 1;
    }
}
```

**Retry delays**: 100ms, 200ms, 400ms, 800ms, 1000ms (max)

## Message Flow Diagrams

### Request Flow (When Block Missing Locally)

```
Validator A (Needs Ancestor)
    │
    │ 1. Check Marshal (10ms timeout)
    │    └─→ Not in local storage
    │
    │ 2. Create oneshot channel
    │ 3. Register pending request (digest → oneshot sender)
    │ 4. Send Digest request on ancestor channel (Recipients::All)
    │
    ▼
[P2P Network - Ancestor Channel]
    │
    │ 5. Broadcast request to all peers
    │
    ├──────────────────────────┼──────────────────────────┐
    │                          │                          │
    ▼                          ▼                          ▼
Validator B              Validator C              Validator D
    │                          │                          │
    │ 6. Receive request       │ 6. Receive request       │ 6. Receive request
    │ 7. Check Marshal         │ 7. Check Marshal         │ 7. Check Marshal
    │    (10ms timeout)        │    (10ms timeout)        │    (10ms timeout)
    │                          │                          │
    ├─→ Has block?             ├─→ Has block?             ├─→ Has block?
    │   │                          │                          │
    │   │ 8. Send Block            │                          │
    │   │    response               │                          │
    │   │                          │                          │
    │   └─→ No block?           └─→ No block?           └─→ No block?
    │       (ignore)                   (ignore)                   (ignore)
    │
    ▼
[P2P Network - Ancestor Channel]
    │
    │ 9. Deliver Block response
    │
    ▼
Validator A
    │
    │ 10. Match response to pending request (by digest)
    │ 11. Send block via oneshot channel
    │ 12. Verify digest matches
    │ 13. Store in marshal
    │ 14. Continue ancestor chain
```

### Response Flow (When Serving Peers)

```
Validator X (Has Ancestor)
    │
    │ 1. Receive Digest request from peer
    │ 2. Decode as Digest
    │ 3. Check Marshal local storage (10ms timeout)
    │
    ├─→ Found in local storage?
    │   │
    │   ├─→ Yes: Encode Block → Send to Recipients::All
    │   └─→ No: Ignore request (don't have block)
    │
    ▼
[P2P Network - Ancestor Channel]
    │
    │ 4. Broadcast Block response (all peers receive it)
    │
    ▼
Requesting Validators
    │
    │ 5. Receive Block response
    │ 6. Decode as Block
    │ 7. Check pending requests (by digest)
    │ 8. If matches: Fulfill oneshot channel
```

## Key Design Decisions

### 1. Marshal Local Storage Check First

**Why?**
- Local storage is fastest (usually < 1ms)
- Avoids network overhead
- Reduces load on ancestor channel

**Implementation**:
- Uses `marshal.subscribe()` with 10ms timeout
- If timeout triggers, block is not in local storage
- Proceeds to peer request without waiting for marshal backfill

### 2. No Timeout for Peer Requests

**Why?**
- Retry logic already handles failures
- Prevents premature timeout when peers are slow to respond
- Exponential backoff provides natural timeout behavior

**Trade-off**:
- If no peer responds, request waits indefinitely for that retry attempt
- But retry loop (up to 5 retries) ensures eventual timeout

### 3. Broadcast Responses (Recipients::All)

**Why?**
- Simpler implementation (no need for peer-specific sending)
- Multiple validators might need the same block
- Recipients filter by digest matching in pending requests

**Trade-off**:
- Slightly less efficient (all peers receive response)
- But ancestor requests are infrequent, so overhead is minimal

### 4. Per-Instance Ancestor Channel

**Why?**
- Each consensus instance is independent
- Prevents cross-instance interference
- Allows per-instance rate limiting

**Channel Assignment**:
- Instance 0: channels 0-9 (ancestor = 6)
- Instance 1: channels 10-19 (ancestor = 16)
- Instance 2: channels 20-29 (ancestor = 26)
- etc.

### 5. Separate Message Handler Task

**Why?**
- Ancestor requests can arrive at any time
- Not tied to a specific `finalize_ancestors` task
- Can serve multiple concurrent ancestor fetches

**Lifecycle**:
- Spawned once at Actor startup
- Runs continuously throughout Actor lifetime
- Handles requests from all peers for all consensus instances

## Error Handling

### Missing Blocks

If an ancestor block cannot be fetched after all retries:

```rust
// Max retries reached - give up on this ancestor
info!("Could not fetch ancestor {} after {} retries - stopping ancestor chain", 
      cursor_digest, MAX_RETRIES);
break;  // Stop walking up the chain
```

**Impact**:
- Ancestor chain finalization stops at this point
- Blocks above this ancestor remain unfinalized
- System continues normally (doesn't crash)

### Invalid Responses

If a block response doesn't match the requested digest:

```rust
if block.digest() != cursor_digest {
    warn!("Received ancestor block with mismatched digest");
    // Continue retry loop
}
```

### Network Failures

If ancestor channel send fails:

```rust
if send_result.is_err() {
    warn!("Failed to send ancestor request to peers: {:?}", send_result.err());
    // Remove from pending and continue retry
    pending_ancestor_requests.remove(&cursor_digest);
}
```

## Metrics and Logging

### Log Messages

- `"Starting ancestor finalization task for block X with parent: Y"`
- `"Attempting to fetch ancestor X"`
- `"Successfully fetched ancestor X (view Y) from marshal local storage"`
- `"Ancestor X not in marshal, requesting from peers"`
- `"Received ancestor X (view Y) from peer, verifying digest"`
- `"Retry N for ancestor X (waiting Yms)"`
- `"Could not fetch ancestor X after N retries - stopping ancestor chain"`
- `"Completed ancestor finalization chain: N ancestors finalized"`

### Debug Messages

- `"Received ancestor request from peer for digest: X"`
- `"Sent ancestor block X to peers"`
- `"Don't have requested ancestor X in local storage, ignoring request"`
- `"Received ancestor response for block X"`

## Configuration

### Channel Registration

```rust
// In validator.rs
const ANCESTOR_CHANNEL: u32 = 6;

// Per-instance channel
let ancestor_channel = base_channel + ANCESTOR_CHANNEL;
let ancestor = network.register(
    ancestor_channel,
    Quota::per_second(NonZeroU32::new(16).unwrap()),  // 16 msg/sec
    config.message_backlog
);
```

### Timeouts and Constants

```rust
// In actor.rs finalize_ancestors task
const MAX_RETRIES: usize = 5;
const INITIAL_RETRY_DELAY_MS: u64 = 100;
const MAX_RETRY_DELAY_MS: u64 = 1000;
const LOCAL_CHECK_TIMEOUT_MS: u64 = 10;  // Marshal local storage check
```

## Comparison with Marshal Backfill

| Aspect | Ancestor Channel | Marshal Backfill |
|--------|------------------|------------------|
| **Purpose** | Fetch ancestors for finalization | General block fetching |
| **Trigger** | Explicit request in `finalize_ancestors` | Automatic when `marshal.subscribe()` fails |
| **Scope** | Only ancestor blocks | Any missing blocks |
| **Channel** | Dedicated ancestor channel (per-instance) | Backfill channel (shared) |
| **Local Check** | 10ms timeout (avoid backfill) | Full backfill mechanism |
| **Rate Limit** | 16 msg/sec per instance | 8 msg/sec shared |
| **Message Format** | Request: Digest, Response: Block | Internal to marshal |

## Example Scenarios

### Scenario 1: All Ancestors in Local Storage

```
1. Block 10 finalized → parent = block_9_digest
2. Check marshal for block_9 → Found immediately (< 1ms)
3. Use block 9, continue to block 8
4. Check marshal for block_8 → Found immediately
5. Continue until genesis
6. All ancestors finalized from local storage
```

### Scenario 2: Missing Ancestor, Peer Has It

```
1. Block 10 finalized → parent = block_9_digest
2. Check marshal for block_9 → Not found (10ms timeout)
3. Send ancestor request to peers
4. Validator B receives request, has block_9 in local storage
5. Validator B sends block_9 response
6. Validator A receives response, verifies digest
7. Store block_9 in marshal
8. Continue ancestor chain
```

### Scenario 3: Missing Ancestor, No Peer Has It

```
1. Block 10 finalized → parent = block_9_digest
2. Check marshal → Not found
3. Send ancestor request → No response (after waiting)
4. Retry 1: Wait 100ms, check marshal, send request → No response
5. Retry 2: Wait 200ms, check marshal, send request → No response
6. Retry 3: Wait 400ms, check marshal, send request → No response
7. Retry 4: Wait 800ms, check marshal, send request → No response
8. Retry 5: Wait 1000ms, check marshal, send request → No response
9. Give up: "Could not fetch ancestor X after 5 retries"
10. Stop ancestor chain finalization
```

## Performance Characteristics

### Latency

- **Local storage hit**: < 1ms
- **Peer request round-trip**: ~50-200ms (depending on network)
- **Retry delay**: 100ms, 200ms, 400ms, 800ms, 1000ms

### Throughput

- **Rate limit**: 16 ancestor requests/second per consensus instance
- **Concurrent requests**: Multiple ancestors can be fetched concurrently
- **Message handler**: Single handler serves all requests/responses

### Resource Usage

- **Memory**: Pending requests map (HashMap<Digest, oneshot::Sender>)
- **Network**: Ancestor channel messages (Digest + Block encodings)
- **CPU**: Minimal (mostly message encoding/decoding)

## Future Improvements

Potential enhancements:
1. **Request deduplication**: If multiple tasks request same ancestor, share response
2. **Response routing**: Send responses directly to requesting peer (more efficient)
3. **Adaptive timeout**: Adjust timeout based on network conditions
4. **Batch requests**: Request multiple ancestors in single message
5. **Priority queuing**: Prioritize more recent ancestors

