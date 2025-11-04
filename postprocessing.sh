#!/usr/bin/env bash

set -euo pipefail

# Extract gatling lines from per-validator logs into gatling_logs/gatling_X_X.log
# Input: validator_X_X.log (X = validator number, X = instance count)
# Output: gatling_X_X.log (matching validator and instance numbers)

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="${SCRIPT_DIR}"
IN_DIR="${REPO_ROOT}/logs/validators"
OUT_DIR="${REPO_ROOT}/logs/gatling"

if [[ ! -d "${IN_DIR}" ]]; then
  echo "Logs directory not found: ${IN_DIR}" >&2
  exit 1
fi

mkdir -p "${OUT_DIR}"

shopt -s nullglob
for f in "${IN_DIR}"/validator_*.log; do
  base="$(basename "$f")"
  if [[ "$base" =~ ^validator_([0-9]+)_([0-9]+)\.log$ ]]; then
    validator_idx="${BASH_REMATCH[1]}"
    instance_count="${BASH_REMATCH[2]}"
    out_file="${OUT_DIR}/gatling_${validator_idx}_${instance_count}.log"
    # Extract lines containing '[gatling]', preserving UTC timestamp at the beginning
    # Format: <timestamp> [gatling]...
    if ! grep -Ei "\\[gatling\\]" "$f" | sed -E 's/^([0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]+Z).*(\[gatling\].*)$/\1 \2/' >"$out_file" 2>/dev/null; then
      : >"$out_file"
    fi
    echo "Wrote ${out_file}"
  fi
done

echo "Verifying gatling logs..."
python3 "${REPO_ROOT}/verify_gatling_logs.py" --dir "${OUT_DIR}"


