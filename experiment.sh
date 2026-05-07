#!/usr/bin/env bash

if [ -z "${BASH_VERSION:-}" ]; then
  exec /usr/bin/env bash "$0" "$@"
fi

set -euo pipefail

# End-to-end pipeline: run a remote experiment sweep, import the resulting
# validator logs, tear down the droplets, then run the local postprocessing
# (gatling extraction + verification, latency CSV, plots).
#
# Any CLI args passed to this script are forwarded to orchestrator-remote.sh
# (e.g. --instances "1,10,20" --duration 120).

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

ORCHESTRATOR="${REPO_ROOT}/remote-runs/orchestrator-remote.sh"
IMPORT_LOGS="${REPO_ROOT}/postprocessing/import-logs.sh"
DELETE_DROPLETS="${REPO_ROOT}/droplets/delete.sh"
EXTRACT_GATLING="${REPO_ROOT}/postprocessing/extract_gatling_logs.sh"
GENERATE_CSV="${REPO_ROOT}/postprocessing/generate_csv.py"
PLOTS="${REPO_ROOT}/postprocessing/plots.py"

echo "Step 1/6: Running remote orchestrator..."
bash "${ORCHESTRATOR}" "$@"

echo "Step 2/6: Importing validator logs..."
bash "${IMPORT_LOGS}"

echo "Step 3/6: Deleting DigitalOcean droplets..."
bash "${DELETE_DROPLETS}" --force

echo "Step 4/6: Extracting and verifying gatling logs..."
bash "${EXTRACT_GATLING}"

echo "Step 5/6: Computing latency and writing stats.csv..."
python3 "${GENERATE_CSV}" 

echo "Step 6/6: Generating PDF plots..."
python3 "${PLOTS}"

echo "Postprocessing completed."
