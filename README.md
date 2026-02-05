# alto

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](./LICENSE-MIT)
[![License: Apache 2.0](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](./LICENSE-APACHE)
[![Codecov](https://codecov.io/gh/commonwarexyz/alto/graph/badge.svg?token=Y2A6Q5G25W)](https://codecov.io/gh/commonwarexyz/alto)
[![Ask DeepWiki](https://deepwiki.com/badge.svg)](https://deepwiki.com/commonwarexyz/alto)

## Components

_Components are designed for deployment in adversarial environments. If you find an exploit, please refer to our [security policy](./SECURITY.md) before disclosing it publicly (an exploit may equip a malicious party to attack users of a primitive)._

* [chain](./chain/README.md): A minimal (and wicked fast) blockchain built with the [Commonware Library](https://github.com/commonwarexyz/monorepo).
* [client](./client/README.md): Client for interacting with `alto`.
* [explorer](./explorer/README.md): Visualize `alto` activity.
* [inspector](./inspector/README.md): Inspect `alto` activity.
* [types](./types/README.md): Common types used throughout `alto`.

## Gatling Logic (High Level)

Gatling adds a multi-instance consensus + deterministic finalization layer on top of the base chain. The main logic lives in the `chain` crate (runtime wiring, gossip, and finalization), with postprocessing scripts to reconstruct and verify the global order from logs.

### Where Gatling Is Added

- **Multi-instance validator runtime**: `chain/src/bin/validator.rs` starts `K` independent consensus engines and wires a shared gatling thread that consumes finalized blocks from all instances.
- **Consensus engine integration**: `chain/src/engine.rs` defines `GatlingEvent` and includes per-instance configuration (proposal offsets, instance IDs, and the channel to the gatling thread).
- **Application block building**: `chain/src/application/actor.rs` creates blocks by pulling transactions from the mempool during proposal and annotates logs with validator/instance info.
- **Transaction ingestion + gossip**: `chain/src/http_server.rs` (HTTP submission) and `chain/src/bin/validator.rs` (P2P gossip tasks) distribute transactions to every instance’s mempool.
- **Gatling log reconstruction**: `chain/src/bin/validator.rs` runs the gatling thread that reconstructs the global order and emits `[gatling]` log lines; postprocessing scripts only extract and verify those logs.

### What the Gatling Logic Consists Of

- **Transaction creation and block inclusion**: validators propose blocks by pulling up to `MAX_BLOCK_TRANSACTIONS` from a FIFO mempool, then build and broadcast a block for the current view. The proposal path includes a “second pickup” to include transactions that arrived during the proposal wait window.
- **Transaction gossip and mempool fan-out**: submitted or locally generated transactions are verified and pushed into *all* consensus instances’ mempools, then optionally gossiped to other validators, which repeat the same fan-out on receipt.
- **Multi-instance protocol formation**: each validator runs `K` independent consensus instances with staggered proposal offsets; finalized blocks from each instance are forwarded into the gatling thread via `GatlingEvent`.
- **Gatling log reconstruction (gatling thread)**: the gatling thread maintains per-instance queues keyed by view and a global cursor that processes `(view, instance)` in lexicographic order, logging only directly finalized views and filling gaps with indirect finalizations.
- **Logs for postprocessing**: each validator’s gatling thread reconstructs the global order and emits `[gatling]` lines when blocks and transactions become *globally* final; postprocessing extracts these lines into `logs/gatling/` and verifies that all validators observe the same ordering.

## Licensing

This repository is dual-licensed under both the [Apache 2.0](./LICENSE-APACHE) and [MIT](./LICENSE-MIT) licenses. You may choose either license when employing this code.

## Support

If you have any questions about `alto`, we encourage you to post in [GitHub Discussions](https://github.com/commonwarexyz/monorepo/discussions). We're happy to help!
