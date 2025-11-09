# Remote Run Tooling

This directory contains scripts for orchestrating validator experiments across remote machines. Key entry points:

- `setup-droplet.sh` – generates validator configs locally and (optionally) deploys them to remote hosts.
- `orchestrator-remote.sh` – manages validator lifecycles on each remote machine.
- `drive-remote-runs.sh` – local controller that ties everything together for repeatable multi-run experiments.

## drive-remote-runs.sh

`drive-remote-runs.sh` lives on your laptop. For every `(instances, run)` pair it:

1. Regenerates validator configs via `setup-droplet.sh`.
2. Produces per-validator `peers.yaml` files with the correct remote IP substitutions.
3. Copies the fresh configs to each remote validator host.
4. SSHes into every host to launch `orchestrator-remote.sh` with the right environment overrides so each invocation covers just that pair.

### Prerequisites

- Passwordless SSH access to each validator host configured inside the script.
- The repository cloned on each remote at `/root/alto` (override with `REMOTE_REPO_DIR`).
- Validator binaries already built on the remote machines.

### Quick Start

```bash
# Regenerate configs each time and run full 1..10 instance sweep with 2 runs per instance
cd remote-runs
./drive-remote-runs.sh

# Dry run to preview the commands without executing them
DRY_RUN=1 ./drive-remote-runs.sh

# Use a smaller matrix
MAX_INSTANCES=3 RUNS_PER_INSTANCE=1 ./drive-remote-runs.sh
```

The script uses `setup-droplet.sh` with `SKIP_REMOTE_DEPLOY=1` so the remote distribution happens inside the driver. Update the `REMOTE_HOSTS` array at the top of the script to match your validator machines, keeping it in the same order as the configuration in `setup-droplet.sh`.

Each run receives a unique label (`YYYYMMDD-HHMMSS_iX_rY`) that is used both for the remote config directory (`${REMOTE_BASE_DIR}/${label}`) and the log directory (`${REMOTE_LOG_BASE}/${label}`). You can override these locations via environment variables if your remotes use different paths.

### Environment Overrides

- `MAX_INSTANCES`, `RUNS_PER_INSTANCE`, `SLEEP_SECONDS` – control the experiment grid and timing.
- `ENABLE_TX_SUBMISSION`, `TX_COUNT`, `TX_WAVES`, `TX_INTERVAL`, `TX_START_DELAY` – forwarded to `orchestrator-remote.sh`.
- `REMOTE_BASE_DIR`, `REMOTE_LOG_BASE`, `REMOTE_REPO_DIR` – adjust remote paths.
- `SKIP_SETUP=1` – reuse the configs already in `chain/test-remote`.

### Failure Handling

If any remote orchestrator exits with a non-zero status the driver aborts the remaining schedule for that `(instances, run)` pair and returns a non-zero exit code. Logs remain in the per-run directories on each host for inspection. Use `DRY_RUN=1` to debug connection issues without touching the remotes.

