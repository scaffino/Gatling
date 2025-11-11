# Remote Run Tooling

This directory contains scripts for orchestrating validator runs across remote machines. There are two primary workflows:

- Quick single-run orchestration with `run-remote.sh` (recommended)
- Experiment grid driver with `drive-remote-runs.sh`

## Quick Start: run-remote.sh

Run everything from your laptop; the script generates configs, deploys, and starts validators on all remotes.

Prerequisites:
- SSH access to each remote host configured in `run-remote.sh` (`REMOTE_HOSTS`).
- Repo cloned on remotes at `/root/alto` (override with `REMOTE_REPO_DIR`).
- Firewall/security groups allow p2p and http/metrics ports between validators.

Usage:

```bash
# Optional knobs (defaults shown)
export CONSENSUS_INSTANCES=3                 # consensus instances per validator
export REMOTE_REPO_DIR=/root/alto            # repo path on remotes
export REMOTE_BASE_DIR=/root/alto/deploy/manual
export REMOTE_LOG_DIR=/root/alto/logs/validator
export STARTUP_STAGGER_NON_BOOTSTRAPPER=5    # seconds: non-bootstrappers wait so bootstrapper starts first

# Optional: Enable transaction submission
export ENABLE_TX_SUBMISSION=1                # enable transaction submission
export TX_NUM_TXS=50                         # number of transactions each submitter sends
export TX_SENDER_SEED=999                    # sender seed for transactions
export TX_RECEIVER_PUBKEY="..."              # receiver public key (hex)
export TX_DRIVE_FROM_LOCAL=1                 # if 1, create/send txs from this laptop (default)
export TX_SUBMIT_TO_ALL=1                    # if 1, submit to every validator (default)
export TX_BATCH_GAP=20                       # seconds between validator batches (local driver)
# If you want only one submitter instead:
# export TX_SUBMIT_TO_ALL=0
# export TX_SUBMITTER_INDEX=0               # which validator submits (0-based)
export TX_STARTUP_WAIT=10                    # seconds to wait before submitting

./remote-runs/run-remote.sh
```

What it does:
- Generates fresh validator configs locally under `chain/test-remote/`.
- Picks one bootstrapper and writes it into all configs.
- Rewrites `peers.yaml` per validator so each advertises its real remote IP.
- Rewrites `directory:` in each remote config to `${REMOTE_BASE_DIR}/${PUBLIC_KEY}` and creates it.
- Copies per-validator config, per-validator `peers.yaml`, and `run-validator.sh` to each VM.
- Starts validators on all VMs (and locally if a localhost entry exists).
- Optionally submits transactions to validators (if `ENABLE_TX_SUBMISSION` is set).

Notes:
- `run-validator.sh` delays non-bootstrapper start by `STARTUP_STAGGER_NON_BOOTSTRAPPER` seconds (default 5).
- `CONSENSUS_INSTANCES` is passed through to `run-validator.sh` and applied at the validator binary.
- Logs are written on each remote under `${REMOTE_LOG_DIR}/val_<PUBLIC_KEY>.log`.
- Transaction submission: When `ENABLE_TX_SUBMISSION=1`, the script will automatically submit transactions after validators start.
  - Default local driver: `TX_DRIVE_FROM_LOCAL=1` creates and sends transactions from your laptop.
    - `TX_SUBMIT_TO_ALL=1`: submit `TX_NUM_TXS` to every validator, sequentially, with `TX_BATCH_GAP` seconds between validators.
    - Single target: set `TX_SUBMIT_TO_ALL=0` and choose `TX_SUBMITTER_INDEX`.
  - Remote driver (fallback): set `TX_DRIVE_FROM_LOCAL=0` to run submitters on each VM instead.

Ports:
- First validator: p2p 3000, metrics 3001, transaction 8081.
- Additional validators increment p2p by 2 (3002, 3004, 3006, …).

Troubleshooting:
- Connectivity: open firewalls on all p2p ports between validators.
- Stuck at startup (remote-only): increase `STARTUP_STAGGER_NON_BOOTSTRAPPER` (e.g., 8–12).
- Only 1 consensus instance: ensure `CONSENSUS_INSTANCES` is exported before running; confirm `run-validator.sh` was synced (the orchestrator copies it).

Manual single-node start on a remote (advanced):

```bash
CONSENSUS_INSTANCES=3 \
PUBLIC_KEY="<PUBKEY>" \
BASE_DIR="/root/alto/deploy/manual" \
LOG_DIR="/root/alto/logs/validator" \
REPO_ROOT="/root/alto" \
./remote-runs/run-validator.sh
```

## Experiment Driver: drive-remote-runs.sh

`drive-remote-runs.sh` automates sweeps across instance counts and repeated runs. For each `(instances, run)` pair it:
1. Regenerates validator configs.
2. Produces per-validator `peers.yaml` with correct remote IPs.
3. Copies configs to each remote host.
4. SSHes into every host to launch the remote orchestrator with the right overrides.

Prerequisites:
- Passwordless SSH to each validator host configured in the script.
- Repo cloned on remotes at `/root/alto` (override with `REMOTE_REPO_DIR`).
- Validator binary available on remotes.

Quick start:

```bash
cd remote-runs
./drive-remote-runs.sh

# Dry run to preview without executing
DRY_RUN=1 ./drive-remote-runs.sh

# Smaller matrix
MAX_INSTANCES=3 RUNS_PER_INSTANCE=1 ./drive-remote-runs.sh
```

Environment overrides:
- `MAX_INSTANCES`, `RUNS_PER_INSTANCE`, `SLEEP_SECONDS`
- `ENABLE_TX_SUBMISSION`, `TX_COUNT`, `TX_WAVES`, `TX_INTERVAL`, `TX_START_DELAY`
- `REMOTE_BASE_DIR`, `REMOTE_LOG_BASE`, `REMOTE_REPO_DIR`
- `SKIP_SETUP=1` – reuse configs already in `chain/test-remote`

Failure handling:
- If any remote orchestrator exits non-zero, the driver stops that `(instances, run)` and returns non-zero.
- Logs remain on each host under the per-run directories for inspection.