# Port Configuration in Alto Validators

## Overview

Yes, **each validator receives ALL messages from other validators via a single port**. The port is specified in each validator's config file, and messages are multiplexed using "channels" over a single TCP connection.

## Where Ports Are Specified

### 1. Validator Config File

Each validator has a YAML config file (e.g., `<public_key>.yaml`) that specifies:

```yaml
port: 3000              # Main P2P communication port
metrics_port: 3001      # Metrics endpoint port
transaction_port: 8081  # HTTP transaction submission port
```

**Location**: `chain/test-remote/<public_key>.yaml`

### 2. Setup Script

Ports are assigned during config generation in `chain/src/bin/setup.rs`:

```rust
// For local setup, ports are assigned sequentially
let mut port = start_port;  // Default: 3000
for each validator {
    port: port,              // 3000, 3002, 3004, ...
    metrics_port: port + 1,   // 3001, 3003, 3005, ...
    port += 2;               // Increment by 2
}
```

**For remote deployment**: All validators can use the same port (e.g., 3000) since they're on different machines.

### 3. Remote Deployment Script

The `run-remote.sh` script sets the starting port:

```bash
SETUP_START_PORT=3000
```

This is passed to the setup binary when generating configs.

## How Ports Work

### Single Port for All Messages

Each validator listens on **ONE port** (`config.port`) for all P2P communication:

```rust
// In chain/src/bin/validator.rs line 464
SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), config.port)
```

This means:
- ✅ All consensus messages arrive on this port
- ✅ All transaction gossip arrives on this port  
- ✅ All backfill/ancestor fetching arrives on this port
- ✅ All messages from all validators arrive on this port

### Message Multiplexing via Channels

Messages are **multiplexed** using channel IDs. Each message type and consensus instance gets a unique channel:

```rust
// Each consensus instance gets channels spaced 10 apart
let base_channel = i as u32 * 10;
let pending_channel = base_channel + 0;      // Channel 0, 10, 20, ...
let recovered_channel = base_channel + 1;    // Channel 1, 11, 21, ...
let resolver_channel = base_channel + 2;      // Channel 2, 12, 22, ...
let broadcaster_channel = base_channel + 3;   // Channel 3, 13, 23, ...
let backfill_channel = base_channel + 4;      // Channel 4, 14, 24, ...
let ancestor_channel = base_channel + 6;      // Channel 6, 16, 26, ...

// Shared transaction channel
let transaction_channel = 5;                   // Channel 5 (shared)
```

**Example with 2 consensus instances:**
- Instance 1: channels 0, 1, 2, 3, 4, 6
- Instance 2: channels 10, 11, 12, 13, 14, 16
- Transaction gossip: channel 5

The network layer automatically demultiplexes messages based on channel ID.

## Port Configuration Examples

### Local Setup (Same Machine)

When running validators locally, each needs a different port:

```yaml
# Validator 1 config
port: 3000
metrics_port: 3001
transaction_port: 8081

# Validator 2 config  
port: 3002
metrics_port: 3003
transaction_port: 8082

# Validator 3 config
port: 3004
metrics_port: 3005
transaction_port: 8083
```

**peers.yaml**:
```yaml
addresses:
  validator1_key: 127.0.0.1:3000
  validator2_key: 127.0.0.1:3002
  validator3_key: 127.0.0.1:3004
```

### Remote Setup (Different Machines)

When running validators on different VMs, they can all use the same port:

```yaml
# All validators use port 3000
# Validator 1 (VM 1)
port: 3000
metrics_port: 3001
transaction_port: 8081

# Validator 2 (VM 2)  
port: 3000
metrics_port: 3001
transaction_port: 8081

# Validator 3 (VM 3)
port: 3000
metrics_port: 3001
transaction_port: 8081
```

**peers.yaml** (on each VM, with remote IPs):
```yaml
addresses:
  validator1_key: 164.90.133.225:3000  # VM 1 IP
  validator2_key: 170.64.129.99:3000   # VM 2 IP
  validator3_key: 134.122.73.49:3000   # VM 3 IP
```

The `run-remote.sh` script automatically:
1. Generates configs with sequential ports (3000, 3002, 3004, ...)
2. Creates per-validator `peers.yaml` files
3. Replaces IPs with remote VM IPs
4. **Preserves the port numbers** from the original config

## Network Architecture

```
┌─────────────────────────────────────────────────────────┐
│                    Validator A (VM 1)                    │
│  ┌──────────────────────────────────────────────────┐   │
│  │  Network Layer (listening on port 3000)          │   │
│  │  ┌──────────┐  ┌──────────┐  ┌──────────┐      │   │
│  │  │ Channel 0│  │ Channel 1│  │ Channel 5│ ...  │   │
│  │  │(Pending) │  │(Recovered)│  │(Tx Gossip)│      │   │
│  │  └──────────┘  └──────────┘  └──────────┘      │   │
│  └──────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────┘
         │                    │                    │
         │ Port 3000          │ Port 3000          │ Port 3000
         ▼                    ▼                    ▼
┌─────────────────┐  ┌─────────────────┐  ┌─────────────────┐
│  Validator B    │  │  Validator C    │  │  Validator D    │
│  (VM 2)         │  │  (VM 3)         │  │  (VM 4)         │
│  Port 3000      │  │  Port 3000      │  │  Port 3000      │
└─────────────────┘  └─────────────────┘  └─────────────────┘
```

## Key Points

1. **Single Port**: Each validator uses ONE port for all P2P communication
2. **Channel Multiplexing**: Different message types use different channel IDs
3. **Same Port Across VMs**: In remote deployments, all validators can use the same port (e.g., 3000) since they're on different machines
4. **Port Preservation**: The `run-remote.sh` script preserves port numbers when generating remote `peers.yaml` files
5. **Network Layer**: The `authenticated::Network` handles all connection management, message routing, and channel demultiplexing

## Additional Ports

Besides the main P2P port, validators also use:

- **metrics_port**: For Prometheus metrics (typically `port + 1`)
- **transaction_port**: For HTTP transaction submission (typically 8080+)

These are separate ports and not used for validator-to-validator communication.

