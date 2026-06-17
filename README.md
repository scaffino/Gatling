# Gatling

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](./LICENSE-MIT)
[![License: Apache 2.0](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](./LICENSE-APACHE)

*This code is for research purposes only and it should not be used in production.*

Gatling is an atomic broadcast protocol that achieves arbitrarily small inter-proposal times  under rotating leader schedules; in particular, smaller than the network delay. Gatling runs multiple parallel instances of a black-box atomic broadcast protocol and staggers their proposal schedules to generate proposals in faster succession than state-of-the-art protocols.
A deterministic interleaving rule merges the outputs of these instances into a single global log.

For more details, check out the paper [Gatling: Rapid-Fire Consensus from Parallel Composition](https://arxiv.org/abs/2606.18220)

## Extending the `alto` chain

Our implementation extends Commonware's `alto` blockchain, a minimal but complete Rust implementation of a blockchain that relies on the [Commonware Monorepo](https://github.com/commonwarexyz/monorepo), a collection of production-grade primitives including consensus, networking, and cryptographic libraries. 
We augment `alto` by enabling each validator to run $K \geq 1$ independent instances of the threshold-Simplex consensus. Each instance maintains its own ledger, follows an independent leader schedule, and communicates over dedicated network channels. Importantly, each instance has  an inter-proposal time of 3s.
We implement the Gatling log reconstruction and confirmation rule, allowing the outputs of these parallel instances to be deterministically merged into a single global log. 

To support transaction processing, we augment $\mathsf{alto}$ with basic transaction handling, including transaction creation, gossiping, and  inclusion in a block. To this end, we equip each  validator with a separate tokio thread that continuously generates transactions at a controlled rate and submits them to the mempools of all threshold-Simplex instances. Each transaction is timestamped at creation time using a Unix timestamp, which we later use to measure the transaction latency.

## Gatling Logic

Gatling adds a multi-instance consensus + deterministic finalization layer on top of the base chain. The main logic lives in the [chain](./chain/README.md) crate (runtime wiring, gossip, and finalization), with postprocessing scripts to reconstruct and verify the global order from logs.

### Where Gatling Is Added to `alto`

- **Multi-instance validator runtime**: `chain/src/bin/validator.rs` starts `K` independent consensus engines and wires a shared gatling thread that consumes finalized blocks from all instances.
- **Consensus engine integration**: `chain/src/engine.rs` defines `GatlingEvent` and includes per-instance configuration (proposal offsets, instance IDs, and the channel to the gatling thread).
- **Application block building**: `chain/src/application/actor.rs` creates blocks by pulling transactions from the mempool during proposal and annotates logs with validator/instance info.
- **Transaction ingestion + gossip**: `chain/src/http_server.rs` (HTTP submission) and `chain/src/bin/validator.rs` (P2P gossip tasks) distribute transactions to every instance’s mempool.
- **Gatling log reconstruction**: `chain/src/bin/validator.rs` runs the gatling thread that reconstructs the global order and emits `[gatling]` log lines; postprocessing scripts only extract and verify those logs.

### What the Gatling Logic Consists Of

- **Transaction creation and block inclusion**: Validators propose blocks by pulling up transactions from a FIFO mempool, then build and broadcast a block for the current view. 
- **Transaction gossip and mempool fan-out**: Locally generated transactions (as well as those received by a client) are pushed into *all* consensus instances’ mempools, then (optionally) gossiped to other validators, which repeat the same process on receipt.
- **Multi-instance protocol formation**: Each validator runs `K` independent consensus instances with staggered proposal offsets; finalized blocks from each instance are forwarded into the Gatling thread via `GatlingEvent`.
- **Gatling log reconstruction**: The Gatling thread maintains per-instance queues keyed by view and a global cursor that processes `(view, instance)` in lexicographic order, logging only directly finalized views and filling gaps with indirect finalizations.
- **Logs for postprocessing**: Each validator’s Gatling thread reconstructs the global order and emits `[gatling]` lines when blocks and transactions become *globally* final; postprocessing extracts these lines into `logs/gatling/` and verifies that all validators observe the same ordering.

## Licensing

This repository is dual-licensed under both the [Apache 2.0](./LICENSE-APACHE) and [MIT](./LICENSE-MIT) licenses. You may choose either license when employing this code.
