#!/usr/bin/env bash

set -euo pipefail

# Extract gatling lines from per-validator logs into gatling_logs/gatling_X.log

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
  if [[ "$base" =~ ^validator_([0-9]+)\.log$ ]]; then
    idx="${BASH_REMATCH[1]}"
    out_file="${OUT_DIR}/gatling_${idx}.log"
    # grep case-insensitive for '[gatling]' and everything after it; if none, create empty file
    if ! grep -Eio "\\[gatling\\].*" "$f" >"$out_file" 2>/dev/null; then
      : >"$out_file"
    fi
    echo "Wrote ${out_file}"
  fi
done

echo "Verifying gatling logs..."
python3 "${REPO_ROOT}/verify_gatling_logs.py" --dir "${OUT_DIR}"


