#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
IPS_FILE="${SCRIPT_DIR}/../remote-runs/ips.txt"
LOG_DIR="${SCRIPT_DIR}/../logs/validator"
REMOTE_LOG_DIR="/root/Gatling/logs/validator"

SSH_OPTS=(
  -o BatchMode=yes
  -o StrictHostKeyChecking=no
  -o UserKnownHostsFile=/dev/null
  -o LogLevel=ERROR
)

mkdir -p "${LOG_DIR}"

load_hosts() {
  local hosts=()

  if [[ ! -f "${IPS_FILE}" ]]; then
    echo "Error: ips.txt not found at ${IPS_FILE}" >&2
    exit 1
  fi

  while IFS= read -r line || [[ -n "${line}" ]]; do
    line="${line%%#*}"
    line="${line#"${line%%[![:space:]]*}"}"
    line="${line%"${line##*[![:space:]]}"}"
    [[ -z "${line}" ]] && continue
    [[ "${line}" =~ ^[[:space:]]*# ]] && continue
    if [[ ! "${line}" =~ ^root@ ]]; then
      line="root@${line}"
    fi
    hosts+=("${line}")
  done < "${IPS_FILE}"

  if [[ ${#hosts[@]} -eq 0 ]]; then
    echo "Error: No valid hosts found in ${IPS_FILE}" >&2
    exit 1
  fi

  echo "${hosts[@]}"
}

get_host_id() {
  local line="$1"
  local host="$2"
  local regex='#[[:space:]]*(.+)'
  if [[ "${line}" =~ ${regex} ]]; then
    local comment="${BASH_REMATCH[1]}"
    comment="${comment#gatling-}"
    comment="${comment%"${comment##*[![:space:]]}"}"
    echo "${comment}"
  else
    if [[ "${host}" == *@* ]]; then
      echo "${host##*@}"
    else
      echo "${host}"
    fi
  fi
}

get_host_id_for_host() {
  local target_host="$1"
  local host_id="${target_host##*@}"

  while IFS= read -r line || [[ -n "${line}" ]]; do
    [[ "${line}" =~ ^[[:space:]]*# ]] && continue
    [[ -z "${line}" ]] && continue
    local host_part="${line%%#*}"
    host_part="${host_part%"${host_part##*[![:space:]]}"}"
    if [[ ! "${host_part}" =~ ^root@ ]]; then
      host_part="root@${host_part}"
    fi
    if [[ "${host_part}" == "${target_host}" ]]; then
      host_id="$(get_host_id "${line}" "${host_part}")"
      break
    fi
  done < "${IPS_FILE}"

  echo "${host_id}"
}

download_from_host() {
  local host="$1"
  local host_id="$2"

  ssh "${SSH_OPTS[@]}" "${host}" "ls ${REMOTE_LOG_DIR}/val_*.log 2>/dev/null" | while read -r file; do
    if [[ -n "${file}" ]]; then
      local filename
      filename=$(basename "${file}")
      echo "  [${host_id}] Downloading ${filename}..."
      scp "${SSH_OPTS[@]}" "${host}:${file}" "${LOG_DIR}/${filename}" || {
        echo "  [${host_id}] Warning: Failed to download ${filename}" >&2
      }
    fi
  done
}

echo "Loading hosts from ${IPS_FILE}..."
hosts=($(load_hosts))
echo "Found ${#hosts[@]} host(s) — downloading in parallel"

pids=()
for host in "${hosts[@]}"; do
  host_id="$(get_host_id_for_host "${host}")"
  download_from_host "${host}" "${host_id}" &
  pids+=("$!")
done

failed=0
for pid in "${pids[@]}"; do
  wait "${pid}" || failed=$((failed + 1))
done

if [[ ${failed} -gt 0 ]]; then
  echo "Warning: ${failed} download job(s) encountered errors" >&2
fi
echo "Log import complete. Logs saved to ${LOG_DIR}"
