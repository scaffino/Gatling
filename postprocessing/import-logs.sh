#!/bin/bash
set -euo pipefail

# Get script directory
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
IPS_FILE="${SCRIPT_DIR}/../remote-runs/ips.txt"
LOG_DIR="${SCRIPT_DIR}/../logs/validator"
REMOTE_LOG_DIR="/root/Gatling/logs/validator"

# SSH options
SSH_OPTS=(
  -o BatchMode=yes
  -o StrictHostKeyChecking=no
  -o UserKnownHostsFile=/dev/null
  -o LogLevel=ERROR
)

# Create destination folder
mkdir -p "${LOG_DIR}"

# Load hosts from ips.txt
load_hosts() {
  local hosts=()
  
  if [[ ! -f "${IPS_FILE}" ]]; then
    echo "Error: ips.txt not found at ${IPS_FILE}" >&2
    exit 1
  fi
  
  while IFS= read -r line || [[ -n "${line}" ]]; do
    # Strip leading/trailing whitespace
    line="${line%%#*}"  # Remove inline comments
    line="${line#"${line%%[![:space:]]*}"}"  # Trim leading whitespace
    line="${line%"${line##*[![:space:]]}"}"  # Trim trailing whitespace
    
    # Skip empty lines and comment lines
    [[ -z "${line}" ]] && continue
    [[ "${line}" =~ ^[[:space:]]*# ]] && continue
    
    # If line doesn't start with root@, assume it's just an IP and prepend root@
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

# Extract host identifier from comment or use IP
get_host_id() {
  local line="$1"
  local host="$2"
  
  # Try to extract comment (everything after #)
  local regex='#[[:space:]]*(.+)'
  if [[ "${line}" =~ ${regex} ]]; then
    local comment="${BASH_REMATCH[1]}"
    # Remove "gatling-" prefix if present, and any trailing whitespace
    comment="${comment#gatling-}"
    comment="${comment%"${comment##*[![:space:]]}"}"
    echo "${comment}"
  else
    # Use IP as identifier
    if [[ "${host}" == *@* ]]; then
      echo "${host##*@}"
    else
      echo "${host}"
    fi
  fi
}

# Get host ID for a given host by looking it up in ips.txt
get_host_id_for_host() {
  local target_host="$1"
  local host_id="${target_host##*@}"  # Default to IP
  
  # Read ips.txt to find matching host and extract comment
  while IFS= read -r line || [[ -n "${line}" ]]; do
    # Skip pure comment lines
    [[ "${line}" =~ ^[[:space:]]*# ]] && continue
    [[ -z "${line}" ]] && continue
    
    # Extract the host part (before #)
    local host_part="${line%%#*}"
    host_part="${host_part%"${host_part##*[![:space:]]}"}"  # Trim trailing whitespace
    
    # Normalize host format
    if [[ ! "${host_part}" =~ ^root@ ]]; then
      host_part="root@${host_part}"
    fi
    
    # If this line matches our target host, extract the ID
    if [[ "${host_part}" == "${target_host}" ]]; then
      host_id="$(get_host_id "${line}" "${host_part}")"
      break
    fi
  done < "${IPS_FILE}"
  
  echo "${host_id}"
}

# Main
echo "Loading hosts from ${IPS_FILE}..."
hosts=($(load_hosts))
echo "Found ${#hosts[@]} host(s)"

# Download logs from each host
for host in "${hosts[@]}"; do
  host_id="$(get_host_id_for_host "${host}")"
  echo "Downloading logs from ${host} (${host_id})..."
  
  # List and download each log file
  ssh "${SSH_OPTS[@]}" "${host}" "ls ${REMOTE_LOG_DIR}/val_*.log 2>/dev/null" | while read -r file; do
    if [[ -n "${file}" ]]; then
      filename=$(basename "${file}")
      # Optionally prefix with host identifier to avoid conflicts
      # Uncomment the next line if you want host prefixes:
      # filename="val_${host_id}_${filename#val_}"
      
      echo "  Downloading ${filename}..."
      scp "${SSH_OPTS[@]}" "${host}:${file}" "${LOG_DIR}/${filename}" || {
        echo "  Warning: Failed to download ${filename} from ${host}" >&2
      }
    fi
  done
done

echo "Log import complete. Logs saved to ${LOG_DIR}"
