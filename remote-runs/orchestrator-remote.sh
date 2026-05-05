#!/usr/bin/env bash

set -euo pipefail

# =============================================================================
# Remote Orchestrator
# - Iterates over INSTANCES (explicit list of consensus instance counts)
# - For each instance count, runs one experiment
# - Before each run: re-run setup (fresh configs)
# - Deploys configs + per-validator peers.yaml to each remote
# - Starts validators on remotes with --submit-tx (built-in transaction generation)
# - Validators automatically generate transactions at configured rate
# - After RUN_DURATION_SECONDS, kills all validators on remotes and locally
# =============================================================================
#
# =============================================================================
# Usage
#
#   ./remote-runs/orchestrator-remote.sh [options]
#
# Options (CLI flags)
#   -t, --duration SECONDS
#       Wall-clock run duration per experiment (genesis+SECONDS), then kill validators.
#
#   --instances LIST
#       Explicit consensus instances sweep (comma/space separated), e.g. "1,10,20" or "1 10 20".
#
#   --runs-per-instance N
#       Number of repetitions per instance value.
#
#   --crash-validator-index N
#       Fault injection: after readiness succeeds, crash exactly one validator by index (0-based).
#       Index is the line order in remote-runs/ips.txt (and therefore the validator config order).
#
#   --crash-delay SECONDS
#       Optional delay (after readiness) before crashing the validator (default: 0).
#
#   -h, --help
#       Print usage and exit.
#
# Inputs / conventions
#   - remote-runs/ips.txt: one host per line (IP or root@IP). Lines starting with # are ignored.
#   - The script expects exactly V hosts in ips.txt (default V=10).
#
# Environment variables (override defaults)
#   - V: number of validators (must match ips.txt line count)
#   - RUN_DURATION_SECONDS, INSTANCES
#   - SUBMIT_TX_RATE, SUBMIT_TX_START, SUBMIT_TX_DURATION (computed if not set)
#   - REMOTE_REPO_DIR, REMOTE_BASE_DIR, REMOTE_LOG_DIR, REMOTE_STORAGE_DIR
#   - CRASH_VALIDATOR_INDEX, CRASH_DELAY_SECONDS (alternatives to flags)
# =============================================================================

# -----------------------------
# Configuration (overridable)
# -----------------------------
# Explicit consensus instances sweep (space or comma separated).
INSTANCES="${INSTANCES:-1 10 20 30 40 50}"

# Number of validators
V="${V:-10}"

# Fault injection: optionally crash one validator after readiness
CRASH_VALIDATOR_INDEX="${CRASH_VALIDATOR_INDEX:-}"   # 0-based index into REMOTE_HOSTS/PUBLIC_KEYS
CRASH_DELAY_SECONDS="${CRASH_DELAY_SECONDS:-0}"      # delay after readiness before crashing

# Per-run wall clock (seconds)
RUN_DURATION_SECONDS="${RUN_DURATION_SECONDS:-1200}"
SETTLE_SECONDS="${SETTLE_SECONDS:-4}"
READINESS_TIMEOUT="${READINESS_TIMEOUT:-300}"    # seconds to wait for first finalized block per validator
MAX_RUN_RETRIES="${MAX_RUN_RETRIES:-2}"          # total attempts per instance value before giving up

# Transaction submission parameters (built into validators via --submit-tx)
SUBMIT_TX_RATE="${SUBMIT_TX_RATE:-100}"          # Transactions per second
SUBMIT_TX_START="${SUBMIT_TX_START:-600}"        # Start delay in seconds after genesis
# SUBMIT_TX_DURATION is computed in main() after parse_args so --duration is respected

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
REMOTE_REPO_DIR="${REMOTE_REPO_DIR:-/root/Gatling}"
REMOTE_BASE_DIR="${REMOTE_BASE_DIR:-/root/Gatling/deploy/manual}"
REMOTE_LOG_DIR="${REMOTE_LOG_DIR:-/root/Gatling/logs/validator}"
REMOTE_STORAGE_DIR="${REMOTE_STORAGE_DIR:-/root/Gatling/deploy/manual}"

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
    line="${line%%#*}"                              # Strip inline comments
    line="${line#"${line%%[![:space:]]*}"}"         # Trim leading whitespace
    line="${line%"${line##*[![:space:]]}"}"         # Trim trailing whitespace
    [[ -z "${line}" ]] && continue

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
SETUP_WORKER_THREADS=8
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
GENESIS_TS=""

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

# True if idx is the local validator index
is_local_idx() {
  [[ ${LOCAL_VALIDATOR_INDEX} -ge 0 && $1 -eq ${LOCAL_VALIDATOR_INDEX} ]]
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
      --instances LIST      Explicit instances list (comma/space separated, default: ${INSTANCES})

      --crash-validator-index N  After readiness, crash validator at 0-based index N (default: disabled)
      --crash-delay SECONDS      Delay after readiness before crashing (default: ${CRASH_DELAY_SECONDS})

You can also set environment variables:
  RUN_DURATION_SECONDS, INSTANCES,
  SUBMIT_TX_RATE, SUBMIT_TX_START, SUBMIT_TX_DURATION,
  CRASH_VALIDATOR_INDEX, CRASH_DELAY_SECONDS, etc.
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
      --instances)
        shift
        [[ $# -gt 0 ]] || { err "Missing value for --instances"; exit 2; }
        INSTANCES="$1"
        ;;
      --crash-validator-index)
        shift
        [[ $# -gt 0 ]] || { err "Missing value for --crash-validator-index"; exit 2; }
        CRASH_VALIDATOR_INDEX="$1"
        ;;
      --crash-delay)
        shift
        [[ $# -gt 0 ]] || { err "Missing value for --crash-delay"; exit 2; }
        CRASH_DELAY_SECONDS="$1"
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

crash_one_validator() {
  local instances="$1"
  [[ -n "${CRASH_VALIDATOR_INDEX}" ]] || return 0

  if [[ ! "${CRASH_VALIDATOR_INDEX}" =~ ^[0-9]+$ ]]; then
    err "Invalid --crash-validator-index '${CRASH_VALIDATOR_INDEX}' (must be integer >= 0); skipping crash injection"
    return 0
  fi

  if [[ "${CRASH_VALIDATOR_INDEX}" -ge ${#REMOTE_HOSTS[@]} ]]; then
    err "Crash index ${CRASH_VALIDATOR_INDEX} out of range (hosts=${#REMOTE_HOSTS[@]}); skipping crash injection"
    return 0
  fi

  if [[ ! "${CRASH_DELAY_SECONDS}" =~ ^[0-9]+$ ]]; then
    err "Invalid --crash-delay '${CRASH_DELAY_SECONDS}' (must be integer >= 0); defaulting to 0"
    CRASH_DELAY_SECONDS=0
  fi

  if [[ "${CRASH_DELAY_SECONDS}" -gt 0 ]]; then
    log "Crash injection armed: will kill validator idx=${CRASH_VALIDATOR_INDEX} after ${CRASH_DELAY_SECONDS}s (instances=${instances})..."
    sleep "${CRASH_DELAY_SECONDS}"
  else
    log "Crash injection armed: killing validator idx=${CRASH_VALIDATOR_INDEX} now (instances=${instances})..."
  fi

  local host="${REMOTE_HOSTS[CRASH_VALIDATOR_INDEX]}"
  local public_key="${PUBLIC_KEYS[CRASH_VALIDATOR_INDEX]}"
  local match="validator --.*--config=${REMOTE_BASE_DIR}/${public_key}.yaml"

  if is_local_idx "${CRASH_VALIDATOR_INDEX}"; then
    pkill -KILL -f "${match}" 2>/dev/null || true
  else
    remote_cmd "${host}" "pkill -KILL -f '${match}' || true" || true
  fi

  log "Crash injection complete (killed idx=${CRASH_VALIDATOR_INDEX} ident=${public_key} host=${host})"
}

# ---------------------------------
# Phase: Generate fresh configs
# ---------------------------------
generate_configs() {
  log "Generating ${V} validator configs (setup)..."
  rm -rf "${CONFIG_OUTPUT_DIR}" || true
  local setup_out
  setup_out=$(
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
  printf '%s\n' "${setup_out}"
  GENESIS_TS=$(printf '%s\n' "${setup_out}" | awk '/genesis timestamp:/{print $NF}')
  [[ -n "${GENESIS_TS}" ]] || fatal "Could not parse genesis timestamp from setup output"
  log "Genesis timestamp: ${GENESIS_TS}"
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

  local idx
  for idx in "${!REMOTE_HOSTS[@]}"; do
    local host="${REMOTE_HOSTS[idx]}"
    REMOTE_IPS+=("$(extract_ip "${host}")")
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
# Phase: Wait for all validators to produce their first finalized block
# ---------------------------------
wait_for_readiness() {
  local instances="$1"
  local deadline=$(( $(date +%s) + READINESS_TIMEOUT ))
  log "Waiting for all validators to finalize their first block (timeout: ${READINESS_TIMEOUT}s)..."

  while true; do
    local all_ready=true
    local not_ready=()
    local idx
    for idx in "${!REMOTE_HOSTS[@]}"; do
      local host="${REMOTE_HOSTS[idx]}"
      local log_file="${REMOTE_LOG_DIR}/val_i${instances}_${PUBLIC_KEYS[idx]}.log"
      if ! remote_cmd "${host}" "grep -qF '[gatling] Validator' '${log_file}'" 2>/dev/null; then
        all_ready=false
        not_ready+=("${idx}")
      fi
    done

    if ${all_ready}; then
      log "All validators ready"
      return 0
    fi

    if [[ $(date +%s) -ge ${deadline} ]]; then
      err "Readiness timeout after ${READINESS_TIMEOUT}s — one or more validators produced no finalized block"
      err "Validators that did NOT finalize the first block:"
      for idx in "${not_ready[@]}"; do
        local ip="${REMOTE_IPS[idx]:-unknown}"
        local ident="${PUBLIC_KEYS[idx]:-unknown}"
        local log_file="${REMOTE_LOG_DIR}/val_i${instances}_${ident}.log"
        err "  - ip=${ip} ident=${ident} log=${log_file}"
      done
      return 1
    fi

    sleep 5
  done
}

# ---------------------------------
# Kill and wait for SSH background PIDs
# ---------------------------------
_kill_ssh_pids() {
  local pid
  for pid in "${ACTIVE_VALIDATOR_SSH_PIDS[@]:-}"; do
    if [[ -n "${pid}" ]] && kill -0 "${pid}" 2>/dev/null; then
      kill "${pid}" 2>/dev/null || true
    fi
  done
  for pid in "${ACTIVE_VALIDATOR_SSH_PIDS[@]:-}"; do
    [[ -n "${pid}" ]] && wait "${pid}" 2>/dev/null || true
  done
  ACTIVE_VALIDATOR_SSH_PIDS=()
}

# ---------------------------------
# Kill validators everywhere (graceful: INT then KILL)
# ---------------------------------
kill_everything() {
  log "Stopping validators on all hosts..."

  local idx
  for idx in "${!REMOTE_HOSTS[@]}"; do
    is_local_idx "${idx}" && continue
    remote_cmd "${REMOTE_HOSTS[idx]}" "pkill -INT -f 'validator --' || true" || true &
  done
  pkill -INT -f "validator --" 2>/dev/null || true
  wait 2>/dev/null || true

  sleep 1

  for idx in "${!REMOTE_HOSTS[@]}"; do
    is_local_idx "${idx}" && continue
    remote_cmd "${REMOTE_HOSTS[idx]}" "pkill -KILL -f 'validator --' || true" || true &
  done
  pkill -KILL -f "validator --" 2>/dev/null || true
  wait 2>/dev/null || true

  _kill_ssh_pids
  log "Validator cleanup completed"
}

# ---------------------------------
# Cleanup on interrupt (immediate KILL, no graceful shutdown)
# ---------------------------------
cleanup_on_interrupt() {
  log ""
  log "===== INTERRUPT RECEIVED ====="
  log "Cleaning up all validators and exiting..."

  local idx
  for idx in "${!REMOTE_HOSTS[@]}"; do
    is_local_idx "${idx}" && continue
    remote_cmd "${REMOTE_HOSTS[idx]}" "pkill -KILL -f 'validator --' || true" || true &
  done
  pkill -KILL -f "validator --" 2>/dev/null || true
  wait 2>/dev/null || true

  _kill_ssh_pids

  log "Cleanup complete. Exiting."
  log "===== EXIT ====="
  exit 130
}

# ---------------------------------
# Phase: Deploy to VMs
# ---------------------------------
deploy_to_vms() {
  local peers_template="${CONFIG_OUTPUT_DIR}/peers.yaml"
  local temp_dir
  temp_dir="$(mktemp -d)"
  trap 'rm -rf "${temp_dir}"' RETURN

  local idx
  for idx in "${!REMOTE_HOSTS[@]}"; do
    local host="${REMOTE_HOSTS[idx]}"
    local public_key="${PUBLIC_KEYS[idx]}"
    local config_file="${CONFIG_FILES[idx]}"
    local remote_ip="${REMOTE_IPS[idx]}"
    local peers_output="${temp_dir}/peers_${public_key}.yaml"

    if is_local_idx "${idx}"; then
      generate_peers_yaml "${peers_template}" "${public_key}" "${peers_output}" ""
      local local_base_dir="${REPO_ROOT}/chain/test-remote"
      mkdir -p "${local_base_dir}"
      cp "${peers_output}" "${local_base_dir}/peers.yaml"
      log "Deployed peers.yaml for localhost (${public_key})"
    else
      generate_peers_yaml "${peers_template}" "${public_key}" "${peers_output}" "${remote_ip}"
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
# Clear storage directories
# ---------------------------------
clear_storage_directories() {
  log "Clearing storage directories for clean state..."
  local idx
  for idx in "${!REMOTE_HOSTS[@]}"; do
    local host="${REMOTE_HOSTS[idx]}"
    local storage_dir="${REMOTE_STORAGE_DIR}/${PUBLIC_KEYS[idx]}"
    if is_localhost "${host}"; then
      rm -rf "${storage_dir}" && mkdir -p "${storage_dir}" || true
      log "Cleared local storage: ${storage_dir}"
    else
      remote_cmd "${host}" "rm -rf '${storage_dir}' && mkdir -p '${storage_dir}'" 2>/dev/null || true
      log "Cleared remote storage on ${host}: ${storage_dir}"
    fi
  done
}

# ---------------------------------
# Phase: Start validators (remote)
# ---------------------------------
start_validators() {
  local instances="$1"
  ACTIVE_VALIDATOR_SSH_PIDS=()

  # Pre-run cleanup: kill any existing validators
  log "Pre-run cleanup: stopping any existing validator processes..."
  local idx
  for idx in "${!REMOTE_HOSTS[@]}"; do
    local host="${REMOTE_HOSTS[idx]}"
    log "Stopping validator processes on ${host} (instances=${instances})..."
    remote_cmd "${host}" "pkill -INT -f 'validator --' || true" || true
    sleep 1
    remote_cmd "${host}" "pkill -KILL -f 'validator --' || true" || true
  done

  log "Waiting for ports to be released..."
  sleep 3

  for idx in "${!REMOTE_HOSTS[@]}"; do
    local host="${REMOTE_HOSTS[idx]}"
    local public_key="${PUBLIC_KEYS[idx]}"
    local log_name="val_i${instances}_${public_key}.log"
    local cmd="
export PATH=\"/root/.cargo/bin:\$PATH\" && \
[ -f /root/.cargo/env ] && source /root/.cargo/env || true && \
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
  log "Started validators for instance=${instances}"
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
# Main
# ---------------------------------
main() {
  validate_prerequisites
  trap cleanup_on_interrupt INT TERM

  parse_args "$@"

  # Compute SUBMIT_TX_DURATION after parse_args so --duration is respected
  SUBMIT_TX_DURATION="${SUBMIT_TX_DURATION:-$(( RUN_DURATION_SECONDS - SUBMIT_TX_START ))}"

  # Build instances list from INSTANCES (comma/space separated)
  local instances_list=()
  local inst
  while IFS= read -r inst; do
    [[ -z "${inst}" ]] && continue
    instances_list+=("${inst}")
  done < <(printf '%s\n' "${INSTANCES}" | tr ',[:space:]' '\n' | sed '/^$/d')

  [[ ${#instances_list[@]} -gt 0 ]] || fatal "No instances specified (set INSTANCES env var or use --instances)."
  local i
  for i in "${instances_list[@]}"; do
    [[ "${i}" =~ ^[0-9]+$ ]] || fatal "Invalid instance value '${i}' (must be integer)."
    [[ "${i}" -ge 1 ]] || fatal "Invalid instance value '${i}' (must be >= 1)."
  done

  log "Run duration per experiment: ${RUN_DURATION_SECONDS}s"
  log "Instances sweep: ${instances_list[*]}"
  if [[ -n "${CRASH_VALIDATOR_INDEX}" ]]; then
    log "Fault injection: crash validator idx=${CRASH_VALIDATOR_INDEX} after readiness (delay=${CRASH_DELAY_SECONDS}s)"
  fi

  # Clear log directory on all VMs once at the beginning (parallel)
  log "Clearing log directory on all VMs at start..."
  for idx in "${!REMOTE_HOSTS[@]}"; do
    remote_cmd "${REMOTE_HOSTS[idx]}" "rm -rf '${REMOTE_LOG_DIR}'/* 2>/dev/null || true" || true &
  done
  wait 2>/dev/null || true

  local instances
  for instances in "${instances_list[@]}"; do
    log "===== RUN START: instances=${instances} ====="
    export CONSENSUS_INSTANCES="${instances}"

    local attempt run_ok
    run_ok=0
    for attempt in $(seq 1 $((MAX_RUN_RETRIES + 1))); do
      [[ ${attempt} -gt 1 ]] && log "Retrying run (attempt ${attempt}/$(( MAX_RUN_RETRIES + 1 )))..."
      if (
        set +e
        generate_configs
        collect_validator_info
        clear_storage_directories
        deploy_to_vms
        start_validators "${instances}"
        log "Validators configured with --submit-tx ${SUBMIT_TX_RATE} ${SUBMIT_TX_START} ${SUBMIT_TX_DURATION}"

        if ! wait_for_readiness "${instances}"; then
          kill_everything || true
          exit 1
        fi

        crash_one_validator "${instances}"

        local kill_at sleep_for
        kill_at=$(( GENESIS_TS + RUN_DURATION_SECONDS ))
        sleep_for=$(( kill_at - $(date +%s) ))
        [[ ${sleep_for} -lt 0 ]] && sleep_for=0
        log "Sleeping ${sleep_for}s (kill at genesis+${RUN_DURATION_SECONDS}s)..."
        sleep "${sleep_for}"
        kill_everything || true
      ); then
        run_ok=1
        break
      else
        err "Attempt ${attempt} failed for instances=${instances}"
      fi
    done
    [[ ${run_ok} -eq 1 ]] || err "All attempts exhausted for instances=${instances}, skipping"

    log "Settle ${SETTLE_SECONDS}s..."
    sleep "${SETTLE_SECONDS}"
    log "===== RUN END: instances=${instances} ====="
  done

  trap - INT TERM
  log "All runs completed."
}

main "$@"
