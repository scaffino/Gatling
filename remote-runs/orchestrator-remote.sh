#!/usr/bin/env bash

set -euo pipefail

# =============================================================================
# Remote Orchestrator
# - Iterates consensus instances 1..MAX_INSTANCES
# - For each instances count, runs RUNS_PER_INSTANCE repetitions
# - Before each run: re-run setup (fresh configs)
# - Deploys configs + per-validator peers.yaml to each remote
# - Starts validators on remotes with --submit-tx (built-in transaction generation)
# - Validators automatically generate transactions at configured rate
# - After RUN_DURATION_SECONDS, kills all validators on remotes and locally
# =============================================================================

# -----------------------------
# Configuration (overridable)
# -----------------------------
MIN_INSTANCES="${MIN_INSTANCES:-1}"
MAX_INSTANCES="${MAX_INSTANCES:-10}"
RUNS_PER_INSTANCE="${RUNS_PER_INSTANCE:-10}"

# Number of validators
V="${V:-7}"

# Transaction submission parameters (built into validators via --submit-tx)
SUBMIT_TX_RATE="${SUBMIT_TX_RATE:-0.5}"        # Transactions per second
SUBMIT_TX_START="${SUBMIT_TX_START:-90}"      # Start delay in seconds after genesis
SUBMIT_TX_DURATION="${SUBMIT_TX_DURATION:-180}"  # Duration in seconds

# Per-run wall clock (seconds)
RUN_DURATION_SECONDS="${RUN_DURATION_SECONDS:-450}"
SETTLE_SECONDS="${SETTLE_SECONDS:-2}"

# SSH/SCP options
SSH_OPTS=(
  -o BatchMode=yes
  -o StrictHostKeyChecking=no
  -o UserKnownHostsFile=/dev/null
  -o LogLevel=ERROR
)

# Paths
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
CONFIG_OUTPUT_DIR="${REPO_ROOT}/chain/test-remote"
REMOTE_REPO_DIR="${REMOTE_REPO_DIR:-/root/alto}"
REMOTE_BASE_DIR="${REMOTE_BASE_DIR:-/root/alto/deploy/manual}"
REMOTE_LOG_DIR="${REMOTE_LOG_DIR:-/root/alto/logs/validator}"
REMOTE_STORAGE_DIR="${REMOTE_STORAGE_DIR:-/root/alto/deploy/manual}"

# -----------------------------
# Helpers (needed early)
# -----------------------------
log() { printf '[orchestrator-remote] %s\n' "$*"; }
err() { printf '[orchestrator-remote] ERROR: %s\n' "$*" >&2; }
fatal() { err "$@"; exit 1; }

# Load REMOTE_HOSTS from ips.txt file (one host per line, format: root@IP or IP)
# Lines starting with # are treated as comments and ignored
# Empty lines are ignored
load_remote_hosts() {
  local ips_file="${SCRIPT_DIR}/ips.txt"
  REMOTE_HOSTS=()
  
  if [[ ! -f "${ips_file}" ]]; then
    fatal "ips.txt not found at ${ips_file}. Please create it with one host per line (format: root@IP or IP)"
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
    
    REMOTE_HOSTS+=("${line}")
  done < "${ips_file}"
  
  if [[ ${#REMOTE_HOSTS[@]} -eq 0 ]]; then
    fatal "No valid hosts found in ${ips_file}"
  fi
  
  log "Loaded ${#REMOTE_HOSTS[@]} remote host(s) from ${ips_file}"
}

# Load remote hosts from file
load_remote_hosts

# Setup parameters (mirrors run-remote.sh)
SETUP_PEERS="${V}"
SETUP_BOOTSTRAPPERS="1"
SETUP_WORKER_THREADS=3
SETUP_LOG_LEVEL="info"
SETUP_MESSAGE_BACKLOG=16384
SETUP_MAILBOX_SIZE=16384
SETUP_DEQUE_SIZE=10
SETUP_START_PORT=3000

# -----------------------------
# Globals
# -----------------------------
CONFIG_FILES=()
PUBLIC_KEYS=()
REMOTE_IPS=()
LOCAL_VALIDATOR_INDEX=-1
ACTIVE_VALIDATOR_SSH_PIDS=()

# -----------------------------
# Helpers
# -----------------------------
command_exists() { command -v "$1" >/dev/null 2>&1; }

extract_ip() {
  local host="$1"
  if [[ "${host}" == *@* ]]; then
    echo "${host##*@}"
  else
    echo "${host}"
  fi
}

is_localhost() {
  local host="$1"
  local ip
  ip="$(extract_ip "${host}")"
  [[ "${ip}" == "127.0.0.1" || "${ip}" == "localhost" || "${ip}" == "::1" ]]
}

remote_cmd() {
  local host="$1"; shift
  if is_localhost "${host}"; then
    "$@"
  else
    ssh "${SSH_OPTS[@]}" "${host}" "$@"
  fi
}

copy_file() {
  local source="$1" host="$2" destination="$3"
  if is_localhost "${host}"; then
    mkdir -p "$(dirname "${destination}")"
    cp "${source}" "${destination}"
  else
    scp "${SSH_OPTS[@]}" "${source}" "${host}:${destination}"
  fi
}


# ---------------------------------
# CLI parsing
# ---------------------------------
print_usage() {
  cat <<EOF
Usage: $(basename "$0") [options]

Options:
  -t, --duration SECONDS    Max run time per experiment before kill (default: ${RUN_DURATION_SECONDS})
      --min-instances N     Min consensus instances to start from (default: ${MIN_INSTANCES})
      --max-instances N     Max consensus instances to sweep (default: ${MAX_INSTANCES})
      --runs-per-instance N Number of runs per instance (default: ${RUNS_PER_INSTANCE})

You can also set environment variables:
  RUN_DURATION_SECONDS, MIN_INSTANCES, MAX_INSTANCES, RUNS_PER_INSTANCE, 
  SUBMIT_TX_RATE, SUBMIT_TX_START, SUBMIT_TX_DURATION, etc.
EOF
}

parse_args() {
  while [[ $# -gt 0 ]]; do
    case "$1" in
      -t|--duration)
        shift
        [[ $# -gt 0 ]] || { err "Missing value for --duration"; exit 2; }
        RUN_DURATION_SECONDS="$1"
        ;;
      --min-instances)
        shift
        [[ $# -gt 0 ]] || { err "Missing value for --min-instances"; exit 2; }
        MIN_INSTANCES="$1"
        ;;
      --max-instances)
        shift
        [[ $# -gt 0 ]] || { err "Missing value for --max-instances"; exit 2; }
        MAX_INSTANCES="$1"
        ;;
      --runs-per-instance)
        shift
        [[ $# -gt 0 ]] || { err "Missing value for --runs-per-instance"; exit 2; }
        RUNS_PER_INSTANCE="$1"
        ;;
      -h|--help)
        print_usage
        exit 0
        ;;
      *)
        err "Unknown option: $1"
        print_usage
        exit 2
        ;;
    esac
    shift
  done
}

# ---------------------------------
# Phase: Generate fresh configs
# ---------------------------------
generate_configs() {
  log "Generating ${V} validator configs (setup)..."
  rm -rf "${CONFIG_OUTPUT_DIR}" || true
  (
    cd "${REPO_ROOT}/chain"
    cargo run --bin setup -- generate \
      --peers "${SETUP_PEERS}" \
      --bootstrappers "${SETUP_BOOTSTRAPPERS}" \
      --worker-threads "${SETUP_WORKER_THREADS}" \
      --log-level "${SETUP_LOG_LEVEL}" \
      --message-backlog "${SETUP_MESSAGE_BACKLOG}" \
      --mailbox-size "${SETUP_MAILBOX_SIZE}" \
      --deque-size "${SETUP_DEQUE_SIZE}" \
      --output "test-remote" \
      local \
      --start-port "${SETUP_START_PORT}"
  )
  [[ -f "${CONFIG_OUTPUT_DIR}/peers.yaml" ]] || fatal "peers.yaml not found after setup"
  log "Configs generated at ${CONFIG_OUTPUT_DIR}"
}

# ---------------------------------
# Phase: Collect validator info
# ---------------------------------
collect_validator_info() {
  CONFIG_FILES=()
  PUBLIC_KEYS=()
  REMOTE_IPS=()
  LOCAL_VALIDATOR_INDEX=-1

  while IFS= read -r line; do
    [[ -n "${line}" ]] && CONFIG_FILES+=("${line}")
  done < <(find "${CONFIG_OUTPUT_DIR}" -maxdepth 1 -type f -name "*.yaml" \
      ! -name "peers.yaml" ! -name "config.yaml" -print | LC_ALL=C sort)

  [[ ${#CONFIG_FILES[@]} -gt 0 ]] || fatal "No validator config files found in ${CONFIG_OUTPUT_DIR}"
  [[ ${#CONFIG_FILES[@]} -eq ${V} ]] || fatal "Expected ${V} validator configs, found ${#CONFIG_FILES[@]}"

  local cfg
  for cfg in "${CONFIG_FILES[@]}"; do
    PUBLIC_KEYS+=("$(basename "${cfg}" .yaml)")
  done

  for idx in "${!REMOTE_HOSTS[@]}"; do
    local host="${REMOTE_HOSTS[idx]}"
    local ip
    ip="$(extract_ip "${host}")"
    REMOTE_IPS+=("${ip}")
    if is_localhost "${host}"; then
      [[ ${LOCAL_VALIDATOR_INDEX} -eq -1 ]] || fatal "Multiple localhost entries not supported"
      LOCAL_VALIDATOR_INDEX=${idx}
    fi
  done

  [[ ${#REMOTE_IPS[@]} -eq ${#PUBLIC_KEYS[@]} ]] || fatal "REMOTE_HOSTS count != validators count"
}

# ---------------------------------
# Helper: peers.yaml generator
# ---------------------------------
generate_peers_yaml() {
  local peers_template="$1"
  local current_key="$2"
  local output_file="$3"
  local current_validator_ip="${4:-}"
  {
    while IFS= read -r line || [[ -n "$line" ]]; do
      if [[ "$line" =~ ^addresses:[[:space:]]*$ ]]; then
        echo "$line"
        continue
      fi
      if [[ -z "$line" ]] || [[ "$line" =~ ^[[:space:]]*$ ]]; then
        echo "$line"
        continue
      fi
      if [[ "$line" =~ ^([[:space:]]+)([^:[:space:]]+):[[:space:]]*(.+)$ ]]; then
        local indent="${BASH_REMATCH[1]}"
        local peer_key="${BASH_REMATCH[2]}"
        local peer_addr="${BASH_REMATCH[3]}"
        if [[ "$peer_addr" =~ ^([^:]+):(.+)$ ]]; then
          local peer_port="${BASH_REMATCH[2]}"
          if [[ "${peer_key}" == "${current_key}" ]]; then
            if [[ -n "${current_validator_ip}" ]]; then
              echo "${indent}${peer_key}: ${current_validator_ip}:${peer_port}"
            else
              echo "${indent}${peer_key}: ${peer_addr}"
            fi
          else
            local peer_ip=""
            local i
            for i in "${!PUBLIC_KEYS[@]}"; do
              if [[ "${PUBLIC_KEYS[i]}" == "${peer_key}" ]]; then
                peer_ip="${REMOTE_IPS[i]}"
                break
              fi
            done
            if [[ -n "${peer_ip}" ]]; then
              echo "${indent}${peer_key}: ${peer_ip}:${peer_port}"
            else
              echo "${indent}${peer_key}: ${peer_addr}"
            fi
          fi
        else
          echo "${indent}${peer_key}: ${peer_addr}"
        fi
      else
        echo "$line"
      fi
    done < "${peers_template}"
  } > "${output_file}"
}

# ---------------------------------
# Fast validator shutdown across all hosts (sequential, like run-remote.sh)
# ---------------------------------
stop_validators_parallel() {
  local instances="${1:-}"
  local idx

  # Stop validators on remote VMs (sequential, fast)
  for idx in "${!REMOTE_HOSTS[@]}"; do
    local host="${REMOTE_HOSTS[idx]}"
    # Skip localhost (handled separately)
    if [[ ${idx} -eq ${LOCAL_VALIDATOR_INDEX} ]] && [[ ${LOCAL_VALIDATOR_INDEX} -ge 0 ]]; then
      continue
    fi
    
    remote_cmd "${host}" "pkill -INT -f 'validator --' || true" || true
    sleep 1
    remote_cmd "${host}" "pkill -KILL -f 'validator --' || true" || true
  done
}

# ---------------------------------
# Phase: Deploy to VMs
# ---------------------------------
deploy_to_vms() {
  local peers_template="${CONFIG_OUTPUT_DIR}/peers.yaml"
  local temp_dir
  temp_dir="$(mktemp -d)"
  trap 'rm -rf "${temp_dir}"' RETURN

  for idx in "${!REMOTE_HOSTS[@]}"; do
    local host="${REMOTE_HOSTS[idx]}"
    local public_key="${PUBLIC_KEYS[idx]}"
    local config_file="${CONFIG_FILES[idx]}"
    local remote_ip="${REMOTE_IPS[idx]}"
    local is_local="false"
    [[ ${idx} -eq ${LOCAL_VALIDATOR_INDEX} ]] && is_local="true"

    local peers_output="${temp_dir}/peers_${public_key}.yaml"
    if [[ "${is_local}" == "true" ]]; then
      generate_peers_yaml "${peers_template}" "${public_key}" "${peers_output}" ""
    else
      generate_peers_yaml "${peers_template}" "${public_key}" "${peers_output}" "${remote_ip}"
    fi

    if [[ "${is_local}" == "true" ]]; then
      local local_base_dir="${REPO_ROOT}/chain/test-remote"
      mkdir -p "${local_base_dir}"
      cp "${peers_output}" "${local_base_dir}/peers.yaml"
      log "Deployed peers.yaml for localhost (${public_key})"
    else
      local modified_config="${temp_dir}/config_${public_key}.yaml"
      local new_storage_dir="${REMOTE_STORAGE_DIR}/${public_key}"
      if ! grep -q "^directory:" "${config_file}"; then
        cp "${config_file}" "${modified_config}"
      else
        sed "s|^directory:.*|directory: ${new_storage_dir}|" "${config_file}" > "${modified_config}"
      fi
      remote_cmd "${host}" "mkdir -p '${REMOTE_BASE_DIR}' '${REMOTE_LOG_DIR}' '${new_storage_dir}'"
      copy_file "${modified_config}" "${host}" "${REMOTE_BASE_DIR}/${public_key}.yaml"
      copy_file "${peers_output}" "${host}" "${REMOTE_BASE_DIR}/peers.yaml"
      log "Deployed config and peers to ${remote_ip} (${public_key})"
    fi
  done
}

# ---------------------------------
# Kill validators on remote host (pre-run cleanup)
# ---------------------------------
kill_validator_remote() {
  local host="$1"
  local instances="${2:-}"
  log "Stopping validator processes on ${host} (instances=${instances:-any})..."
  
  # Fast sequential kill (matching run-remote.sh approach)
  remote_cmd "${host}" "pkill -INT -f 'validator --' || true" || true
  sleep 1
  remote_cmd "${host}" "pkill -KILL -f 'validator --' || true" || true
}

# ---------------------------------
# Clear storage directories
# ---------------------------------
clear_storage_directories() {
  log "Clearing storage directories for clean state..."
  local idx
  for idx in "${!REMOTE_HOSTS[@]}"; do
    local host="${REMOTE_HOSTS[idx]}"
    local public_key="${PUBLIC_KEYS[idx]}"
    local storage_dir="${REMOTE_STORAGE_DIR}/${public_key}"
    
    if is_localhost "${host}"; then
      if [[ -d "${storage_dir}" ]]; then
        find "${storage_dir}" -mindepth 1 -delete 2>/dev/null || true
        log "Cleared local storage: ${storage_dir}"
      fi
    else
      # Check if storage directory exists and clear it
      if remote_cmd "${host}" "test -d '${storage_dir}'" 2>/dev/null; then
        remote_cmd "${host}" "find '${storage_dir}' -mindepth 1 -delete 2>/dev/null || true" 2>/dev/null || true
        log "Cleared remote storage on ${host}: ${storage_dir}"
      fi
    fi
  done
}

# ---------------------------------
# Phase: Start validators (remote)
# ---------------------------------
start_validators() {
  local instances="$1"
  local run_idx="$2"
  ACTIVE_VALIDATOR_SSH_PIDS=()

  # Pre-run cleanup: kill any existing validators
  log "Pre-run cleanup: stopping any existing validator processes..."
  local idx
  for idx in "${!REMOTE_HOSTS[@]}"; do
    local host="${REMOTE_HOSTS[idx]}"
    kill_validator_remote "${host}" "${instances}" || true
  done
  
  # Wait longer for ports to be released (TIME_WAIT state)
  log "Waiting for ports to be released..."
  sleep 3

  for idx in "${!REMOTE_HOSTS[@]}"; do
    local host="${REMOTE_HOSTS[idx]}"
    local public_key="${PUBLIC_KEYS[idx]}"
    local log_name="val_${public_key}_i${instances}_r${run_idx}.log"
    local cmd="
cd '${REMOTE_REPO_DIR}' && \
  NO_COLOR=1 TERM=dumb \
  cargo run --release --bin validator -- \
    --peers='${REMOTE_BASE_DIR}/peers.yaml' \
    --config='${REMOTE_BASE_DIR}/${public_key}.yaml' \
    --gatling \
    --no-gossip-txs \
    --consensus-instances '${instances}' \
    --submit-tx ${SUBMIT_TX_RATE} ${SUBMIT_TX_START} ${SUBMIT_TX_DURATION} \
    2>&1 | sed 's/\x1b\[[0-9;]*m//g' > '${REMOTE_LOG_DIR}/${log_name}'
"
    ssh "${SSH_OPTS[@]}" "${host}" "${cmd}" &
    ACTIVE_VALIDATOR_SSH_PIDS+=("$!")
  done
  log "Started validators for instance=${instances}, run=${run_idx}"
}

# ---------------------------------
# Phase: Submit transactions (built into validators)
# ---------------------------------
# Transactions are now automatically generated by validators using --submit-tx
# No separate submission phase needed

# ---------------------------------
# Kill validators everywhere
# ---------------------------------
kill_everything() {
  log "Stopping validators on all hosts..."
  
  # Fast parallel kill: send INT to all hosts simultaneously
  for idx in "${!REMOTE_HOSTS[@]}"; do
    local host="${REMOTE_HOSTS[idx]}"
    # Skip localhost (handled separately)
    if [[ ${idx} -eq ${LOCAL_VALIDATOR_INDEX} ]] && [[ ${LOCAL_VALIDATOR_INDEX} -ge 0 ]]; then
      continue
    fi
    remote_cmd "${host}" "pkill -INT -f 'validator --' || true" || true &
  done
  
  # Local INT
  pkill -INT -f "validator --" 2>/dev/null || true
  
  # Wait for all INT signals to be sent
  wait 2>/dev/null || true
  
  # Short grace period (once for all hosts)
  sleep 1
  
  # Fast parallel kill: send KILL to all hosts simultaneously
  for idx in "${!REMOTE_HOSTS[@]}"; do
    local host="${REMOTE_HOSTS[idx]}"
    # Skip localhost (handled separately)
    if [[ ${idx} -eq ${LOCAL_VALIDATOR_INDEX} ]] && [[ ${LOCAL_VALIDATOR_INDEX} -ge 0 ]]; then
      continue
    fi
    remote_cmd "${host}" "pkill -KILL -f 'validator --' || true" || true &
  done
  
  # Local KILL
  pkill -KILL -f "validator --" 2>/dev/null || true
  
  # Wait for all KILL signals to be sent
  wait 2>/dev/null || true
  
  # Kill local SSH processes (non-blocking, validators already stopped)
  for pid in "${ACTIVE_VALIDATOR_SSH_PIDS[@]:-}"; do
    if [[ -n "${pid}" ]] && kill -0 "${pid}" 2>/dev/null; then
      kill "${pid}" 2>/dev/null || true
    fi
  done
  
  # Wait for SSH processes (non-blocking)
  for pid in "${ACTIVE_VALIDATOR_SSH_PIDS[@]:-}"; do
    if [[ -n "${pid}" ]]; then
      wait "${pid}" 2>/dev/null || true
    fi
  done
  
  # Clear the array for next iteration
  ACTIVE_VALIDATOR_SSH_PIDS=()
  
  log "Validator cleanup completed"
}

# ---------------------------------
# Prereqs
# ---------------------------------
validate_prerequisites() {
  command_exists cargo || fatal "cargo not found"
  [[ ${#REMOTE_HOSTS[@]} -gt 0 ]] || fatal "REMOTE_HOSTS is empty"
  [[ ${#REMOTE_HOSTS[@]} -eq ${V} ]] || fatal "REMOTE_HOSTS (${#REMOTE_HOSTS[@]}) != V (${V})"
  [[ -d "${REPO_ROOT}/chain" ]] || fatal "missing chain/ dir at ${REPO_ROOT}/chain"
  mkdir -p "${CONFIG_OUTPUT_DIR}"
}

# ---------------------------------
# Cleanup on interrupt
# ---------------------------------
cleanup_on_interrupt() {
  log ""
  log "===== INTERRUPT RECEIVED ====="
  log "Cleaning up all validators and exiting..."
  
  # Fast parallel kill on all hosts (immediate KILL, no graceful shutdown on interrupt)
  log "Stopping all validator processes on remote hosts..."
  for idx in "${!REMOTE_HOSTS[@]}"; do
    local host="${REMOTE_HOSTS[idx]}"
    # Skip localhost (handled separately)
    if [[ ${idx} -eq ${LOCAL_VALIDATOR_INDEX} ]] && [[ ${LOCAL_VALIDATOR_INDEX} -ge 0 ]]; then
      continue
    fi
    # Kill in parallel (background)
    remote_cmd "${host}" "pkill -KILL -f 'validator --' || true" || true &
  done
  
  # Local cleanup (immediate KILL)
  pkill -KILL -f "validator --" 2>/dev/null || true
  
  # Wait for all parallel kills to complete
  wait 2>/dev/null || true
  
  # Kill SSH background processes (non-blocking, validators already stopped)
  for pid in "${ACTIVE_VALIDATOR_SSH_PIDS[@]:-}"; do
    if [[ -n "${pid}" ]] && kill -0 "${pid}" 2>/dev/null; then
      kill "${pid}" 2>/dev/null || true
    fi
  done
  
  # Wait for SSH processes (non-blocking)
  for pid in "${ACTIVE_VALIDATOR_SSH_PIDS[@]:-}"; do
    if [[ -n "${pid}" ]]; then
      wait "${pid}" 2>/dev/null || true
    fi
  done
 
  log "Cleanup complete. Exiting."
  log "===== EXIT ====="
  exit 130  # Exit code 130 is standard for SIGINT
}

# ---------------------------------
# Main
# ---------------------------------
main() {
  validate_prerequisites
  trap cleanup_on_interrupt INT TERM

  # Parse CLI overrides
  parse_args "$@"
  
  # Validate instance range
  if [[ ${MIN_INSTANCES} -lt 1 ]]; then
    fatal "MIN_INSTANCES must be >= 1, got ${MIN_INSTANCES}"
  fi
  if [[ ${MAX_INSTANCES} -lt ${MIN_INSTANCES} ]]; then
    fatal "MAX_INSTANCES (${MAX_INSTANCES}) must be >= MIN_INSTANCES (${MIN_INSTANCES})"
  fi
  
  log "Run duration per experiment: ${RUN_DURATION_SECONDS}s"
  log "Instance range: ${MIN_INSTANCES} to ${MAX_INSTANCES}"

  # Clear log directory on all VMs once at the beginning (parallel)
  log "Clearing log directory on all VMs at start..."
  for idx in "${!REMOTE_HOSTS[@]}"; do
    local host="${REMOTE_HOSTS[idx]}"
    remote_cmd "${host}" "rm -rf '${REMOTE_LOG_DIR}'/* 2>/dev/null || true" || true &
  done
  wait 2>/dev/null || true

  local instances run_idx
  for instances in $(seq "${MIN_INSTANCES}" "${MAX_INSTANCES}"); do
    for run_idx in $(seq 1 "${RUNS_PER_INSTANCE}"); do
      log "===== RUN START: instances=${instances}, run=${run_idx} ====="
      export CONSENSUS_INSTANCES="${instances}"

      # Wrap the entire run in error handling to ensure we continue to next iteration
      if ! (
        set +e  # Temporarily disable exit on error for this subshell
        generate_configs
        collect_validator_info
        
        # Clear storage directories for clean state
        clear_storage_directories
        
        deploy_to_vms
        start_validators "${instances}" "${run_idx}"
        # Start run timer from validator startup
        local run_start_ts
        run_start_ts="$(date +%s)"
        # Transactions are automatically generated by validators via --submit-tx
        log "Validators configured with --submit-tx ${SUBMIT_TX_RATE} ${SUBMIT_TX_START} ${SUBMIT_TX_DURATION}"

        # Sleep only the remaining time from validator startup
        local now_ts elapsed remaining
        now_ts="$(date +%s)"
        elapsed="$(( now_ts - run_start_ts ))"
        remaining="$(( RUN_DURATION_SECONDS - elapsed ))"
        if [[ ${remaining} -lt 0 ]]; then
          remaining=0
        fi
        log "Elapsed since validator startup: ${elapsed}s; sleeping remaining ${remaining}s of ${RUN_DURATION_SECONDS}s window..."
        sleep "${remaining}"

        # Kill everything - suppress all errors to ensure loop continues
        kill_everything || true
      ); then
        err "Warning: Errors during run instances=${instances}, run=${run_idx}, but continuing..."
      fi
      
      log "Settle ${SETTLE_SECONDS}s..."
      sleep "${SETTLE_SECONDS}"
      log "===== RUN END: instances=${instances}, run=${run_idx} ====="
    done
  done

  trap - INT TERM
  log "All runs completed."
}

main "$@"


