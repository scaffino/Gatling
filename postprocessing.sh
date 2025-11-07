#!/usr/bin/env bash

# Re-exec under bash if invoked from a different shell (e.g., zsh)
if [ -z "${BASH_VERSION:-}" ]; then
  exec /usr/bin/env bash "$0" "$@"
fi

set -euo pipefail

# Extract gatling lines from per-validator logs into logs/gatling/gatling_vV_iI_rR.log
# Input: vV_iI_rR.log (V = validator index, I = instance index, R = run index)
# Output: gatling_vV_iI_rR.log (matching validator, instance, and run indices)

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
for f in "${IN_DIR}"/v*_i*_r*.log; do
  base="$(basename "$f")"
  if [[ "$base" =~ ^v([0-9]+)_i([0-9]+)_r([0-9]+)\.log$ ]]; then
    validator_idx="${BASH_REMATCH[1]}"
    instance_idx="${BASH_REMATCH[2]}"
    run_idx="${BASH_REMATCH[3]}"
    out_file="${OUT_DIR}/gatling_v${validator_idx}_i${instance_idx}_r${run_idx}.log"
    # Extract lines containing '[gatling]', preserving UTC timestamp at the beginning
    # Format: <timestamp> [gatling]...
    if ! grep -Ei "\\[gatling\\]" "$f" | sed -E 's/^([0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]+Z).*(\[gatling\].*)$/\1 \2/' >"$out_file" 2>/dev/null; then
      : >"$out_file"
    fi
    echo "Wrote ${out_file}"
  fi
done

echo "Verifying gatling logs grouped by instance (iX) and round (rX)..."

# We verify consistency ACROSS validators for the same instance and round.
# For each (i, r) pair present in ${OUT_DIR}, we copy matching files into a
# temporary directory with normalized names (gatling_*.log) and run the
# verifier on that directory.

shopt -s nullglob

# Build unique list of (instance, round) groups without bash associative arrays
groups_list=""
for f in "${OUT_DIR}"/gatling_v*_i*_r*.log; do
  base="$(basename "$f")"
  if [[ "$base" =~ ^gatling_v([0-9]+)_i([0-9]+)_r([0-9]+)\.log$ ]]; then
    iidx="${BASH_REMATCH[2]}"
    ridx="${BASH_REMATCH[3]}"
    groups_list+=" i${iidx}_r${ridx}"
  fi
done

# Deduplicate groups (compatible with older bash on macOS)
unique_groups=$(printf '%s\n' ${groups_list} | tr ' ' '\n' | grep -E '^i[0-9]+_r[0-9]+$' | sort -u)

overall_ok=0
for key in ${unique_groups}; do
  [[ -z "$key" ]] && continue
  if [[ "$key" =~ ^i([0-9]+)_r([0-9]+)$ ]]; then
    iidx="${BASH_REMATCH[1]}"
    ridx="${BASH_REMATCH[2]}"
  else
    continue
  fi

  tmpdir=$(mktemp -d 2>/dev/null || mktemp -d -t "gatling_verify_${key}")
  # Collect files for this (i, r)
  files=("${OUT_DIR}/gatling_v"*_i"${iidx}"_r"${ridx}".log)
  # Normalize names so the verifier picks them up uniformly
  idx=1
  for src in "${files[@]}"; do
    cp "${src}" "${tmpdir}/gatling_${idx}.log"
    idx=$((idx+1))
  done

  #echo "- Checking group instance=i${iidx}, round=r${ridx} (files=${#files[@]})"
  if ! python3 "${REPO_ROOT}/verify_gatling_logs.py" --dir "${tmpdir}"; then
    echo "Group i${iidx} r${ridx}: verification FAILED" >&2
    overall_ok=1
  else
    echo "Group i${iidx} r${ridx}: verification OK"
  fi

  rm -rf "${tmpdir}"
done

if [[ -z "${unique_groups}" ]]; then
  echo "No gatling files found to verify in ${OUT_DIR}" >&2
  exit 1
fi

if [[ $overall_ok -ne 0 ]]; then
  exit 1
fi


