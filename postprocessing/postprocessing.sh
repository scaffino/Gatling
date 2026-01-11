#!/usr/bin/env bash

# Re-exec under bash if invoked from a different shell (e.g., zsh)
if [ -z "${BASH_VERSION:-}" ]; then
  exec /usr/bin/env bash "$0" "$@"
fi

set -euo pipefail

# Extract gatling lines from per-validator logs into logs/gatling/
# Gatling files match the validator log file pattern with "gatling" prefix:
# - Local format: v{validator_idx}_i{instance}_r{run}.log → gatling_{validator_idx}_i{instance}_r{run}.log
# - Remote format: val_{public_key}_i{instance}_r{run}.log → gatling_{public_key}_i{instance}_r{run}.log
# - Location format: val_{location}_i{instance}_r{run}.log → gatling_{location}_i{instance}_r{run}.log

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# REPO_ROOT is one level up from postprocessing directory
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

# ============================================================================
# CONFIGURATION: Input directory
# ============================================================================
IN_DIR="${REPO_ROOT}/logs/validator-best-copy"
OUT_DIR="${REPO_ROOT}/logs/gatling-best-copy"

# ============================================================================
# HELPER FUNCTIONS
# ============================================================================

# Process a single log file and extract gatling lines
# The output filename matches the input filename pattern but with "gatling" prefix
process_log_file() {
    local file="$1"
    local validator_id="$2"  # Full validator identifier (public key, location, or numeric)
    local instance_idx="$3"
    local run_idx="$4"
    
    local out_file="${OUT_DIR}/gatling_${validator_id}_i${instance_idx}_r${run_idx}.log"
    
    # Extract lines containing '[gatling]', preserving UTC timestamp at the beginning
    # Format: <timestamp> [gatling]...
    if ! grep -Ei "\\[gatling\\]" "$file" | sed -E 's/^([0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]+Z).*(\[gatling\].*)$/\1 \2/' >"$out_file" 2>/dev/null; then
      : >"$out_file"
    fi
    echo "Wrote ${out_file} (from $(basename "$file"))"
}

# ============================================================================
# MAIN PROCESSING
# ============================================================================

mkdir -p "${OUT_DIR}"

# Check if input directory exists
if [[ ! -d "${IN_DIR}" ]]; then
    echo "Error: Input directory not found: ${IN_DIR}" >&2
    exit 1
fi

shopt -s nullglob

echo "Processing directory: ${IN_DIR}"

# Process local format: v{validator_idx}_i{instance_idx}_r{run_idx}.log
# Output: gatling_{validator_idx}_i{instance_idx}_r{run_idx}.log
for f in "${IN_DIR}"/v[0-9]*_i*_r*.log; do
    [[ ! -f "$f" ]] && continue
    base="$(basename "$f")"
    if [[ "$base" =~ ^v([0-9]+)_i([0-9]+)_r([0-9]+)\.log$ ]]; then
        validator_id="${BASH_REMATCH[1]}"
        instance_idx="${BASH_REMATCH[2]}"
        run_idx="${BASH_REMATCH[3]}"
        process_log_file "$f" "$validator_id" "$instance_idx" "$run_idx"
    fi
done

# Process remote format: val_{public_key}_i{instance_idx}_r{run_idx}.log or val_{location}_i{instance_idx}_r{run_idx}.log
# Output: gatling_{public_key}_i{instance_idx}_r{run_idx}.log or gatling_{location}_i{instance_idx}_r{run_idx}.log
# Public key is a hex string (typically 64 chars)
for f in "${IN_DIR}"/val_*_i*_r*.log; do
    [[ ! -f "$f" ]] && continue
    base="$(basename "$f")"
    # Match: val_{location}_i{instance_idx}_r{run_idx}.log (location format)
    # Check location format first to avoid conflicts with hex pattern
    # Location is an alphabetic string like nyc, india, london, etc.
    if [[ "$base" =~ ^val_([a-z]+)_i([0-9]+)_r([0-9]+)\.log$ ]]; then
        location="${BASH_REMATCH[1]}"
        instance_idx="${BASH_REMATCH[2]}"
        run_idx="${BASH_REMATCH[3]}"
        # Use full location name as validator identifier
        process_log_file "$f" "$location" "$instance_idx" "$run_idx"
    # Match: val_{public_key}_i{instance_idx}_r{run_idx}.log
    # Public key is a hex string (typically 64 chars, but we match any hex string)
    elif [[ "$base" =~ ^val_([0-9a-f]+)_i([0-9]+)_r([0-9]+)\.log$ ]]; then
        public_key="${BASH_REMATCH[1]}"
        instance_idx="${BASH_REMATCH[2]}"
        run_idx="${BASH_REMATCH[3]}"
        # Use full public key as validator identifier
        process_log_file "$f" "$public_key" "$instance_idx" "$run_idx"
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
file_count=0
for f in "${OUT_DIR}"/gatling_*_i*_r*.log; do
  [[ ! -f "$f" ]] && continue
  file_count=$((file_count + 1))
  base="$(basename "$f")"
  # Match validator identifier (numeric, hex string of any length, or alphabetic like location names), instance index, and run index
  # Pattern: gatling_{validator_id}_i{instance}_r{run}.log
  # Validator ID can be: numeric (e.g., "1"), hex string (e.g., "3a5f1234..."), or location name (e.g., "nyc")
  if [[ "$base" =~ ^gatling_([0-9a-z]+)_i([0-9]+)_r([0-9]+)\.log$ ]]; then
    iidx="${BASH_REMATCH[2]}"
    ridx="${BASH_REMATCH[3]}"
    groups_list+=" i${iidx}_r${ridx}"
  fi
done

if [[ $file_count -eq 0 ]]; then
  echo "Error: No gatling log files found in ${OUT_DIR}" >&2
  echo "Expected files matching pattern: gatling_*_i*_r*.log" >&2
  exit 1
fi

# Deduplicate groups (compatible with older bash on macOS)
unique_groups=$(printf '%s\n' ${groups_list} | tr ' ' '\n' | grep -E '^i[0-9]+_r[0-9]+$' | sort -u)

if [[ -z "${unique_groups}" ]]; then
  echo "Error: No valid (instance, round) groups found in gatling log files" >&2
  echo "Found ${file_count} file(s) but could not extract instance/round pairs" >&2
  exit 1
fi

echo "Found $(echo "${unique_groups}" | wc -l | tr -d ' ') group(s) to verify"

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
  files=("${OUT_DIR}/gatling_"*_i"${iidx}"_r"${ridx}".log)
  # Normalize names so the verifier picks them up uniformly
  idx=1
  for src in "${files[@]}"; do
    cp "${src}" "${tmpdir}/gatling_${idx}.log"
    idx=$((idx+1))
  done

  echo "Checking group instance=i${iidx}, round=r${ridx} (files=${#files[@]})"
  # verify_gatling_logs.py is in the same directory as this script (postprocessing folder)
  if ! python3 "${SCRIPT_DIR}/verify_gatling_logs.py" --dir "${tmpdir}"; then
    echo "Group i${iidx} r${ridx}: verification FAILED" >&2
    overall_ok=1
  else
    echo "Group i${iidx} r${ridx}: verification OK"
  fi

  rm -rf "${tmpdir}"
done

if [[ $overall_ok -ne 0 ]]; then
  echo "Verification completed with errors" >&2
  exit 1
else
  echo "All groups verified successfully"
fi


