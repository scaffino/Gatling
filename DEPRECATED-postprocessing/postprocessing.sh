#!/usr/bin/env bash

# Re-exec under bash if invoked from a different shell (e.g., zsh)
if [ -z "${BASH_VERSION:-}" ]; then
  exec /usr/bin/env bash "$0" "$@"
fi

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

echo "Step 1/4: Importing validator logs..."
bash "${SCRIPT_DIR}/import-logs.sh"

echo "Step 2/4: Extracting gatling logs..."
bash "${SCRIPT_DIR}/extract_gatling_logs.sh"

echo "Step 3/4: Computing latency..."
python3 "${SCRIPT_DIR}/generate_csv.py"

echo "Step 4/4: Generating plots..."
python3 "${SCRIPT_DIR}/plots.py"

echo "Postprocessing completed."
