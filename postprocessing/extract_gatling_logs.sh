#!/usr/bin/env bash

if [ -z "${BASH_VERSION:-}" ]; then
  exec /usr/bin/env bash "$0" "$@"
fi

set -euo pipefail

# Extract [gatling] lines from per-validator logs into logs/gatling/.
#
# Filename convention (instance index first, no run index):
#   Input  (remote): val_i{N}_{key}.log
#   Input  (local):  v_i{N}_{idx}.log
#   Output:          gatling_i{N}_{id}.log

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

IN_DIR="${REPO_ROOT}/logs/validator"
OUT_DIR="${REPO_ROOT}/logs/gatling"

process_log_file() {
    local file="$1"
    local instance_idx="$2"
    local validator_id="$3"

    local out_file="${OUT_DIR}/gatling_i${instance_idx}_${validator_id}.log"

    if ! grep -Ei "\[gatling\]" "$file" | \
         sed -E 's/^([0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]+Z).*(\[gatling\].*)$/\1 \2/' \
         >"$out_file" 2>/dev/null; then
        : >"$out_file"
    fi
    echo "Wrote ${out_file} (from $(basename "$file"))"
}

mkdir -p "${OUT_DIR}"

if [[ ! -d "${IN_DIR}" ]]; then
    echo "Error: Input directory not found: ${IN_DIR}" >&2
    exit 1
fi

shopt -s nullglob

echo "Processing directory: ${IN_DIR}"

extract_pids=()

# Local format: v_i{instance_idx}_{validator_idx}.log
for f in "${IN_DIR}"/v_i*_*.log; do
    [[ ! -f "$f" ]] && continue
    base="$(basename "$f")"
    if [[ "$base" =~ ^v_i([0-9]+)_([0-9]+)\.log$ ]]; then
        process_log_file "$f" "${BASH_REMATCH[1]}" "${BASH_REMATCH[2]}" &
        extract_pids+=("$!")
    fi
done

# Remote format: val_i{instance_idx}_{key}.log
# Key can be a hex string or alphabetic location name.
for f in "${IN_DIR}"/val_i*_*.log; do
    [[ ! -f "$f" ]] && continue
    base="$(basename "$f")"
    if [[ "$base" =~ ^val_i([0-9]+)_([0-9a-zA-Z]+)\.log$ ]]; then
        process_log_file "$f" "${BASH_REMATCH[1]}" "${BASH_REMATCH[2]}" &
        extract_pids+=("$!")
    fi
done

if [[ ${#extract_pids[@]} -eq 0 ]]; then
    echo "Error: No matching log files found in ${IN_DIR}" >&2
    exit 1
fi

failed=0
for pid in "${extract_pids[@]}"; do
    wait "${pid}" || failed=$((failed + 1))
done
if [[ $failed -gt 0 ]]; then
    echo "Warning: ${failed} extraction job(s) failed" >&2
fi

echo "Verifying gatling logs grouped by instance..."

shopt -s nullglob

instances_list=""
file_count=0
for f in "${OUT_DIR}"/gatling_i*_*.log; do
    [[ ! -f "$f" ]] && continue
    file_count=$((file_count + 1))
    base="$(basename "$f")"
    if [[ "$base" =~ ^gatling_i([0-9]+)_[0-9a-zA-Z]+\.log$ ]]; then
        instances_list+=" i${BASH_REMATCH[1]}"
    fi
done

if [[ $file_count -eq 0 ]]; then
    echo "Error: No gatling log files found in ${OUT_DIR}" >&2
    echo "Expected files matching: gatling_i*_*.log" >&2
    exit 1
fi

unique_instances=$(printf '%s\n' ${instances_list} | tr ' ' '\n' | grep -E '^i[0-9]+$' | sort -u)

if [[ -z "${unique_instances}" ]]; then
    echo "Error: No valid instance indices found in gatling log files" >&2
    exit 1
fi

echo "Found $(echo "${unique_instances}" | wc -l | tr -d ' ') instance group(s) to verify"

verify_pids=()
verify_tmpdirs=()

for key in ${unique_instances}; do
    [[ -z "$key" ]] && continue
    if [[ "$key" =~ ^i([0-9]+)$ ]]; then
        iidx="${BASH_REMATCH[1]}"
    else
        continue
    fi

    tmpdir=$(mktemp -d 2>/dev/null || mktemp -d -t "gatling_verify_i${iidx}")
    files=("${OUT_DIR}/gatling_i${iidx}_"*.log)

    for src in "${files[@]}"; do
        cp "${src}" "${tmpdir}/$(basename "${src}")"
    done

    echo "Checking group instance=i${iidx} (files=${#files[@]})"
    python3 "${SCRIPT_DIR}/verify_gatling_logs.py" --dir "${tmpdir}" &
    verify_pids+=("$!")
    verify_tmpdirs+=("${tmpdir}")
done

overall_ok=0
for i in "${!verify_pids[@]}"; do
    wait "${verify_pids[$i]}" || overall_ok=1
    rm -rf "${verify_tmpdirs[$i]}"
done

if [[ $overall_ok -ne 0 ]]; then
    echo "Verification completed with errors" >&2
    exit 1
else
    echo "All groups verified successfully"
fi
