# Architecture: Validator, Engine, and Actor Relationships

## Component Hierarchy and Relationships

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                           VALIDATOR (validator.rs)                          │
│                    Main Entry Point - System Coordinator                    │
└─────────────────────────────────────────────────────────────────────────────┘
                                    │
                                    │ Creates & Manages
                                    │
                    ┌───────────────┼───────────────┐
                    │               │               │
                    ▼               ▼               ▼
          ┌─────────────┐  ┌─────────────┐  ┌─────────────┐
          │   Network   │  │   HTTP      │  │  Gatling    │
          │   (P2P)     │  │   Server    │  │  Thread     │
          └─────────────┘  └─────────────┘  └─────────────┘
                    │
                    │ Registers Channels
                    │ ├─ Per Instance (base + 0-6)
                    │ └─ Shared (channel 5 for transactions)
                    │
                    ▼
          ┌──────────────────────────────────────┐
          │     Creates N Engine Instances       │
          │     (one per consensus instance)     │
          └──────────────────────────────────────┘
                    │
                    │ For each instance:
                    │   ├─ Channels: (pending, recovered, resolver, 
                    │   │              broadcaster, backfill, ancestor)
                    │   ├─ Config: namespace, timeouts, quotas
                    │   └─ Passes to Engine::new()
                    │
                    ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                            ENGINE (engine.rs)                               │
│                    Orchestrates Consensus Components                        │
└─────────────────────────────────────────────────────────────────────────────┘
                                    │
                    ┌───────────────┼───────────────┐
                    │               │               │
                    ▼               ▼               ▼
          ┌──────────────┐  ┌──────────────┐  ┌──────────────┐
          │  Consensus   │  │   Marshal    │  │    Buffer    │
          │  (BFT Core)  │  │  (Storage)  │  │  (Broadcast) │
          └──────────────┘  └──────────────┘  └──────────────┘
                    │               │               │
                    │               │               │
                    │               │               │
                    │               ▼               │
                    │      ┌─────────────────┐     │
                    │      │   Supervisor     │     │
                    │      │ (Leader Select)  │     │
                    │      └─────────────────┘     │
                    │               │               │
                    │               │               │
                    └───────┬───────┴───────────────┘
                            │
                            │ All interact with:
                            │
                            ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                         APPLICATION ACTOR (actor.rs)                       │
│                    Business Logic - Block Processing                       │
└─────────────────────────────────────────────────────────────────────────────┘
                                    │
                    ┌───────────────┼───────────────┐
                    │               │               │
                    ▼               ▼               ▼
          ┌──────────────┐  ┌──────────────┐  ┌──────────────┐
          │   Mempool    │  │  Ancestor    │  │  Finalized  │
          │ (Transactions│  │  Handler     │  │  Blocks     │
          └──────────────┘  └──────────────┘  └──────────────┘
```

## Additional Components

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                      CONSENSUS TO ACTOR BRIDGE                               │
└─────────────────────────────────────────────────────────────────────────────┘

Consensus (Reporter)
    │
    │ Activity::Finalization
    ▼
┌──────────────────────────┐
│  FinalizationPusher      │  ← Bridge Component (ingress.rs)
│  - Extracts view/payload │
│  - Fetches block from    │
│    marshal               │
│  - Sends to Actor        │
└──────────────────────────┘
    │
    │ Message::Finalized { view, block }
    ▼
Application Actor (Mailbox)
    │
    │ (If indexer enabled)
    ▼
┌──────────────────────────┐
│  Indexer::Pusher         │  ← Optional Component (indexer.rs)
│  - Uploads seeds         │
│  - Uploads notarizations│
│  - Uploads finalizations│
└──────────────────────────┘


┌─────────────────────────────────────────────────────────────────────────────┐
│                      MESSAGE PASSING INTERFACE                               │
└─────────────────────────────────────────────────────────────────────────────┘

Consensus ──Mailbox──> Actor
    │                    │
    │                    │
    ├─ Automaton         ├─ Handles: Genesis, Propose, Verify
    ├─ Relay             ├─ Handles: Broadcast
    └─ Reporter          └─ Handles: Finalized (via FinalizationPusher)

Mailbox (ingress.rs)
    ├─ Implements Automaton trait (genesis, propose, verify)
    ├─ Implements Relay trait (broadcast)
    └─ Message channel: mpsc::Sender<Message> → mpsc::Receiver<Message>
```

## Detailed Component Relationships

### 1. VALIDATOR (validator.rs)
**Role**: System bootstrap and coordination

**Responsibilities**:
- Initializes P2P network infrastructure
- Registers all channels (per-instance + shared)
- Creates multiple Engine instances (one per consensus instance)
- Coordinates HTTP server for transaction submission
- Manages shared state (included_transactions, instance_views)
- Starts gossip mechanisms

**Key Operations**:
```rust
// Creates network
let (mut network, mut oracle) = authenticated::Network::new(...);

// Registers channels per consensus instance
for i in 0..consensus_instances {
    let base = i * 10;
    let channels = (
        network.register(base + 0, ...),  // pending
        network.register(base + 1, ...),  // recovered
        network.register(base + 2, ...),  // resolver
        network.register(base + 3, ...),  // broadcaster
        network.register(base + 4, ...),  // backfill
        network.register(base + 6, ...),  // ancestor
    );
    consensus_channels.push(channels);
}

// Creates engines
for i in 0..consensus_instances {
    let engine = Engine::new(context, config).await;
    consensus_engines.push(engine);
}

// Starts engines with channels
for engine in consensus_engines {
    let channels = consensus_channels.remove(0);
    engine.start(...channels...);
}
```

### 2. ENGINE (engine.rs)
**Role**: Component orchestrator and lifecycle manager

**Contains**:
- `application: Actor<E>` - Application business logic
- `consensus: Consensus<...>` - BFT consensus algorithm
- `marshal: marshal::Actor<...>` - Block storage and retrieval
- `buffer: buffered::Engine<...>` - Broadcast buffering
- `supervisor: Supervisor` - Leader selection and participant management
- `finalization_pusher: FinalizationPusher<E>` - Bridge from Consensus to Actor
- `indexer_pusher: Option<indexer::Pusher<E, I>>` - Optional external indexer

**Responsibilities**:
- Creates all sub-components during `Engine::new()`
- Distributes channels to appropriate components:
  - `ancestor_network` → Application Actor
  - `backfill_network` → Marshal
  - `broadcast_network` → Buffer
  - `pending/recovered/resolver_network` → Consensus
- Coordinates component startup via `Engine::start()`
- Manages component lifecycle (waits for completion)

**Key Operations**:
```rust
// Creates components
let (application, supervisor, application_mailbox) = Actor::new(context, config);
let (marshal, marshal_mailbox) = marshal::Actor::init(..., coordinator: supervisor.clone(), ...);
let (buffer, buffer_mailbox) = buffered::Engine::new(...);
let finalization_pusher = FinalizationPusher::new(..., application_mailbox, marshal_mailbox);
let indexer_pusher = cfg.indexer.map(|i| indexer::Pusher::new(..., i, marshal_mailbox));
let reporter = (finalization_pusher, indexer_pusher).into();
let consensus = Consensus::new(..., automaton: application_mailbox, relay: application_mailbox, reporter, supervisor);

// Distributes channels during start()
pub fn start(
    self,
    pending_network,    // → Consensus
    recovered_network,  // → Consensus
    resolver_network,   // → Consensus
    broadcast_network,  // → Buffer
    backfill_network,   // → Marshal
    ancestor_network,   // → Application Actor
) {
    application.start(marshal_mailbox, ancestor_network);
    buffer.start(broadcast_network);
    marshal.start(application_mailbox, buffer_mailbox, backfill_network);
    consensus.start(pending_network, recovered_network, resolver_network);
}
```

### 3. APPLICATION ACTOR (actor.rs)
**Role**: Business logic and block processing

**Responsibilities**:
- Handles block proposals (collects transactions from mempool)
- Verifies incoming blocks
- Finalizes blocks (triggers ancestor chain finalization)
- Manages mempool (adds/removes transactions)
- Handles ancestor requests/responses via ancestor channel
- Records metrics (finalized blocks, latency)

**Key Message Types** (received via mailbox):
- `Genesis` - Initialize genesis block
- `Propose` - Create new block proposal
- `Verify` - Verify incoming block
- `Broadcast` - Broadcast block to network
- `Finalized` - Process finalized block (triggers ancestor finalization)
- `SubmitTransaction` - Add transaction to mempool

**Ancestor Channel Usage**:
```rust
// Receives ancestor channel in start()
pub fn start(mut self, marshal, ancestor_network) {
    let (ancestor_sender, ancestor_receiver) = ancestor_network;
    // ...
    
    // In finalize_ancestors task:
    // 1. Check marshal local storage (10ms timeout)
    // 2. If not found, send request via ancestor_sender
    // 3. Wait for response via oneshot channel
    // 4. Handle incoming requests via ancestor_receiver
}
```

## Data Flow Diagram

```
┌─────────────────────────────────────────────────────────────────┐
│ TRANSACTION FLOW                                                 │
└─────────────────────────────────────────────────────────────────┘

Client → HTTP Server → Validator → Engine → Actor → Mempool
                                          ↓
                                    (distributed to all engines)


┌─────────────────────────────────────────────────────────────────┐
│ CONSENSUS FLOW                                                   │
└─────────────────────────────────────────────────────────────────┘

Consensus → Mailbox (Automaton::propose) → Actor (Propose) → Creates Block → Returns Digest
         → Mailbox (Automaton::verify)   → Actor (Verify) → Verifies Block → Returns bool
         → Mailbox (Relay::broadcast)     → Actor (Broadcast) → Broadcasts Block
         → Reporter → FinalizationPusher → Fetches Block from Marshal
                    → Actor (Finalized) → Processes Block → Triggers Ancestor Chain
                    → Indexer::Pusher (optional) → Uploads to External Indexer


┌─────────────────────────────────────────────────────────────────┐
│ ANCESTOR FETCHING FLOW                                           │
└─────────────────────────────────────────────────────────────────┘

Actor (finalize_ancestors)
    │
    ├─→ Check Marshal (local storage only, 10ms timeout)
    │   ├─ Found? → Use block
    │   └─ Not found? → Continue
    │
    └─→ Request from Peers (via ancestor channel)
        │
        ├─→ Send Digest request to all peers
        ├─→ Wait for Block response
        ├─→ Verify digest matches
        ├─→ Store in marshal
        └─→ Continue ancestor chain

Other Validators (receive request)
    │
    ├─→ Check Marshal (local storage only)
    ├─→ If found: Send Block response
    └─→ If not found: Ignore request


┌─────────────────────────────────────────────────────────────────┐
│ BLOCK STORAGE FLOW                                               │
└─────────────────────────────────────────────────────────────────┘

Actor → Marshal (verified blocks)
     ← Marshal (subscribe to blocks)
     ← Marshal (local storage check)

Marshal ↔ Backfill Channel (for general block fetching)
Marshal ↔ Buffer (for broadcasting blocks)
```

## Relationship Summary

### Validator → Engine
- **Relationship**: Composition (1:N)
- **Creates**: Multiple Engine instances (one per consensus instance)
- **Provides**: Channel pairs, configuration, shared state
- **Lifecycle**: Validator creates engines → engines run independently

### Engine → Actor
- **Relationship**: Composition (1:1)
- **Creates**: Single Actor instance per Engine
- **Provides**: Marshal mailbox, ancestor channel network
- **Lifecycle**: Engine creates actor → actor runs in background task
- **Communication**: Via Mailbox (message passing)

### Actor Dependencies
- **Marshal**: For block storage/retrieval
- **Consensus**: Sends messages to actor (via Mailbox)
- **Ancestor Channel**: For peer-to-peer ancestor requests
- **Mempool**: For transaction management (internal to actor)

### Supervisor Component
**Role**: Leader selection and participant management

**Responsibilities**:
- Determines leader for each view based on seed (deterministic round-robin)
- Manages participant list (sorted for consistency)
- Provides cryptographic identity and polynomial for threshold signatures
- Used by both Consensus (for leader selection) and Marshal (for coordinator)

**Key Operations**:
```rust
// Created by Actor, shared with Consensus and Marshal
let supervisor = Supervisor::new(polynomial, participants, share);

// Used by Consensus
let leader = supervisor.leader(view, seed);  // Returns PublicKey

// Used by Marshal
marshal::Actor::init(..., coordinator: supervisor.clone(), ...);
```

### FinalizationPusher Component
**Role**: Bridge between Consensus Reporter and Application Actor

**Responsibilities**:
- Receives `Activity::Finalization` from Consensus
- Extracts view and payload (block digest)
- Fetches block from Marshal using `marshal.subscribe()`
- Sends `Message::Finalized` to Application Actor via Mailbox

**Key Operations**:
```rust
// Created by Engine
let finalization_pusher = FinalizationPusher::new(
    context,
    application_mailbox.sender_clone(),
    marshal_mailbox.clone(),
);

// Used in Reporter chain
let reporter = (
    finalization_pusher,  // First in chain
    indexer_pusher,       // Second (optional)
).into();
```

### Indexer Component (Optional)
**Role**: External indexing service for monitoring/exploration

**Responsibilities**:
- Uploads seeds to indexer service
- Uploads notarizations (with blocks) to indexer
- Uploads finalizations (with blocks) to indexer
- Part of Reporter chain (receives Activity from Consensus)

**Key Operations**:
```rust
// Created by Engine (if indexer URI provided)
let indexer = Client::new(indexer_uri, identity);
let indexer_pusher = indexer::Pusher::new(
    context,
    indexer,
    marshal_mailbox.clone(),
);

// Used in Reporter chain after FinalizationPusher
// Consensus → FinalizationPusher → Indexer::Pusher → (external indexer)
```

### Mailbox Component (ingress.rs)
**Role**: Message passing interface between Consensus and Actor

**Responsibilities**:
- Implements `Automaton` trait (genesis, propose, verify)
- Implements `Relay` trait (broadcast)
- Implements `Reporter` trait (for marshal reporting)
- Provides `submit_transaction()` for external transaction submission

**Message Types**:
- `Genesis` - Get genesis block digest
- `Propose` - Create new block proposal
- `Verify` - Verify incoming block
- `Broadcast` - Broadcast block to network
- `Finalized` - Process finalized block
- `SubmitTransaction` - Add transaction to mempool

### Buffer Component (buffered::Engine)
**Role**: Broadcast message buffering and prioritization

**Responsibilities**:
- Buffers block broadcasts before sending to network
- Provides priority queuing for broadcasts
- Manages broadcast channel rate limiting
- Used by Marshal to broadcast blocks

**Key Operations**:
```rust
// Created by Engine
let (buffer, buffer_mailbox) = buffered::Engine::new(...);

// Used by Marshal
marshal.start(
    application_mailbox,
    buffer_mailbox,      // ← For broadcasting blocks
    backfill_network,
);
```

## Key Patterns

1. **Channel Distribution**:
   - Validator registers channels with P2P network
   - Validator passes channels to Engine
   - Engine distributes channels to components

2. **Message Passing**:
   - Consensus → Actor: Via `application::Mailbox`
   - Actor internal: Via `mpsc::channel`
   - Actor ↔ Peers: Via P2P channels (ancestor, broadcast, etc.)

3. **Component Lifecycle**:
   - Validator creates everything at startup
   - Components run as independent async tasks
   - Validator coordinates graceful shutdown

4. **Multi-Instance Architecture**:
   - Each consensus instance is independent
   - Shared channels (transaction gossip)
   - Per-instance channels (consensus, ancestor)
   - Shared state (included_transactions, instance_views)

5. **Reporter Chain Pattern**:
   - Consensus uses Reporter trait to report activities
   - FinalizationPusher is first in chain (always present)
   - Indexer::Pusher is second (optional)
   - Each reporter in chain receives Activity and can process it

6. **Supervisor Sharing**:
   - Created by Actor during initialization
   - Shared with Consensus (for leader selection)
   - Shared with Marshal (as coordinator)
   - Ensures consistent participant list across components

