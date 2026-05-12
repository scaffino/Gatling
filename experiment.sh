#!/usr/bin/env bash

if [ -z "${BASH_VERSION:-}" ]; then
  exec /usr/bin/env bash "$0" "$@"
fi

set -euo pipefail

# Remote sweeps:
#   - 0 / 2 / 3 crashed: instances 1,2,...,10,20,30,40,50
#   - 1 crashed: same plus 11,12,...,19 (full 1..20 plus 30,40,50)
#
#   - Three runs with no crashed validators → logs_0val_crashed_r1 … r3
#   - Three runs with 1 validator crashed  → logs-1val-crashed-r1 … r3
#   - Three runs with 2 validators crashed → logs-2val-crashed-r1 … r3
#   - Three runs with 3 validators crashed → logs-3val-crashed-r1 … r3
#
# Each run: orchestrator-remote.sh → import-logs.sh → rename ${REPO_ROOT}/logs
# to the snapshot directory. Extra CLI args (e.g. --duration 300) are forwarded
# to orchestrator-remote.sh.

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

ORCHESTRATOR="${REPO_ROOT}/remote-runs/orchestrator-remote.sh"
IMPORT_LOGS="${REPO_ROOT}/postprocessing/import-logs.sh"
DELETE_DROPLETS="${REPO_ROOT}/droplets/delete.sh"

LOGS_DIR="${REPO_ROOT}/logs"
INSTANCES="1,2,3,4,5,6,7,8,9,10,20,30,40,50"
INSTANCES_1VAL_CRASHED="1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,30,40,50"

# Snapshot destinations (must not exist before the script runs)
LOG_SNAPSHOTS=(
  "${REPO_ROOT}/logs_0val_crashed_r1"
  "${REPO_ROOT}/logs_0val_crashed_r2"
  "${REPO_ROOT}/logs_0val_crashed_r3"
  "${REPO_ROOT}/logs-1val-crashed-r1"
  "${REPO_ROOT}/logs-1val-crashed-r2"
  "${REPO_ROOT}/logs-1val-crashed-r3"
  "${REPO_ROOT}/logs-2val-crashed-r1"
  "${REPO_ROOT}/logs-2val-crashed-r2"
  "${REPO_ROOT}/logs-2val-crashed-r3"
  "${REPO_ROOT}/logs-3val-crashed-r1"
  "${REPO_ROOT}/logs-3val-crashed-r2"
  "${REPO_ROOT}/logs-3val-crashed-r3"
)

for d in "${LOG_SNAPSHOTS[@]}"; do
  if [[ -d "$d" ]]; then
    echo "Error: $d already exists. Please move or remove it first." >&2
    exit 1
  fi
done

snapshot_logs() {
  local dest="$1"
  if [[ -d "${LOGS_DIR}" ]]; then
    mv "${LOGS_DIR}" "${dest}"
  else
    echo "Warning: logs directory '${LOGS_DIR}' not found; skipping snapshot to ${dest}." >&2
  fi
}

run_sweep() {
  local label="$1"
  local dest="$2"
  local instances="$3"
  shift 3
  echo "${label}"
  bash "${ORCHESTRATOR}" "$@" --instances "${instances}"
  bash "${IMPORT_LOGS}"
  snapshot_logs "${dest}"
}

# ---------------------------------------------------------------------------
# 0 crashed — three full sweeps
# ---------------------------------------------------------------------------
for run in 1 2 3; do
  run_sweep "No crashed validators — run ${run}/3..." \
    "${REPO_ROOT}/logs_0val_crashed_r${run}" \
    "${INSTANCES}" \
    "$@"
done

# ---------------------------------------------------------------------------
# 1 crashed validator — three full sweeps (indices 1, 0-based)
# ---------------------------------------------------------------------------
for run in 1 2 3; do
  run_sweep "1 validator crashed — run ${run}/3..." \
    "${REPO_ROOT}/logs-1val-crashed-r${run}" \
    "${INSTANCES_1VAL_CRASHED}" \
    "$@" --crash-validator-index 1
done

# ---------------------------------------------------------------------------
# 2 crashed validators — three full sweeps
# ---------------------------------------------------------------------------
for run in 1 2 3; do
  run_sweep "2 validators crashed — run ${run}/3..." \
    "${REPO_ROOT}/logs-2val-crashed-r${run}" \
    "${INSTANCES}" \
    "$@" --crash-validator-indices "1,2"
done

# ---------------------------------------------------------------------------
# 3 crashed validators — three full sweeps
# ---------------------------------------------------------------------------
for run in 1 2 3; do
  run_sweep "3 validators crashed — run ${run}/3..." \
    "${REPO_ROOT}/logs-3val-crashed-r${run}" \
    "${INSTANCES}" \
    "$@" --crash-validator-indices "1,2,3"
done

echo "Deleting DigitalOcean droplets..."
bash "${DELETE_DROPLETS}" --force

echo "All sweeps completed (12 runs) and droplets deleted."
