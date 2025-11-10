#!/usr/bin/env bash

set -euo pipefail

# =============================================================================
# Main orchestrator script for running validators on remote VMs
# This script runs locally on your laptop and orchestrates everything
# =============================================================================

# =============================================================================
# CONFIGURATION
# =============================================================================

# Number of validators to generate
V=3

# Remote validator machine IPs (order must match validator assignment)
REMOTE_HOSTS=(
    "root@167.71.84.48"    # gatling-nyc
    "root@188.166.175.132" # gatling-london
    "root@167.71.226.93"   # gatling-india
)

# SSH/SCP options
SSH_OPTS=(
    -o BatchMode=yes
    -o StrictHostKeyChecking=no
    -o UserKnownHostsFile=/dev/null
)

# Local paths
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
CONFIG_OUTPUT_DIR="${REPO_ROOT}/chain/test-remote"

# Remote paths
REMOTE_REPO_DIR="${REMOTE_REPO_DIR:-/root/alto}"
REMOTE_BASE_DIR="${REMOTE_BASE_DIR:-/root/alto/deploy/manual}"
REMOTE_LOG_DIR="${REMOTE_LOG_DIR:-/root/alto/logs/validator}"

# Setup parameters (passed to cargo run --bin setup)
SETUP_PEERS="${V}"
SETUP_BOOTSTRAPPERS=1
SETUP_WORKER_THREADS=3
SETUP_LOG_LEVEL="info"
SETUP_MESSAGE_BACKLOG=16384
SETUP_MAILBOX_SIZE=16384
SETUP_DEQUE_SIZE=10
SETUP_START_PORT=3000

# =============================================================================
# GLOBAL STATE
# =============================================================================

CONFIG_FILES=()
PUBLIC_KEYS=()
REMOTE_IPS=()
ACTIVE_SSH_PIDS=()

# =============================================================================
# HELPER FUNCTIONS
# =============================================================================

log() {
    printf '[run-remote] %s\n' "$*"
}

err() {
    printf '[run-remote] ERROR: %s\n' "$*" >&2
}

fatal() {
    err "$@"
    exit 1
}

command_exists() {
    command -v "$1" >/dev/null 2>&1
}

remote_cmd() {
    local host="$1"
    shift
    ssh "${SSH_OPTS[@]}" "${host}" "$@"
}

copy_file() {
    local source="$1"
    local host="$2"
    local destination="$3"
    scp "${SSH_OPTS[@]}" "${source}" "${host}:${destination}"
}

extract_ip() {
    local host="$1"
    if [[ "${host}" == *@* ]]; then
        echo "${host##*@}"
    else
        echo "${host}"
    fi
}

# =============================================================================
# PHASE 1: GENERATE CONFIGS LOCALLY
# =============================================================================

generate_configs() {
    log "Phase 1: Generating validator configs locally"
    
    # Clean previous output directory
    if [[ -d "${CONFIG_OUTPUT_DIR}" ]]; then
        log "Cleaning previous output directory: ${CONFIG_OUTPUT_DIR}"
        rm -rf "${CONFIG_OUTPUT_DIR}"
    fi
    
    # Run setup binary locally
    log "Running setup binary to generate ${V} validator configs..."
    (
        cd "${REPO_ROOT}/chain" && \
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
    
    # Verify peers.yaml was created
    local peers_file="${CONFIG_OUTPUT_DIR}/peers.yaml"
    if [[ ! -f "${peers_file}" ]]; then
        fatal "peers.yaml not found at ${peers_file}"
    fi
    
    log "Config generation completed"
}

# =============================================================================
# PHASE 2: COLLECT VALIDATOR INFO
# =============================================================================

collect_validator_info() {
    log "Phase 2: Collecting validator information"
    
    # Clear arrays
    CONFIG_FILES=()
    PUBLIC_KEYS=()
    REMOTE_IPS=()
    
    # Collect config files
    while IFS= read -r line; do
        [[ -n "${line}" ]] && CONFIG_FILES+=("${line}")
    done < <(find "${CONFIG_OUTPUT_DIR}" -maxdepth 1 -type f -name "*.yaml" \
        ! -name "peers.yaml" \
        ! -name "config.yaml" \
        -print | LC_ALL=C sort)
    
    if [[ ${#CONFIG_FILES[@]} -eq 0 ]]; then
        fatal "No validator config files found in ${CONFIG_OUTPUT_DIR}"
    fi
    
    if [[ ${#CONFIG_FILES[@]} -ne ${V} ]]; then
        fatal "Expected ${V} validator configs, found ${#CONFIG_FILES[@]}"
    fi
    
    # Extract public keys from filenames
    for config_file in "${CONFIG_FILES[@]}"; do
        PUBLIC_KEYS+=("$(basename "${config_file}" .yaml)")
    done
    
    # Extract IPs from REMOTE_HOSTS
    for host in "${REMOTE_HOSTS[@]}"; do
        REMOTE_IPS+=("$(extract_ip "${host}")")
    done
    
    if [[ ${#REMOTE_IPS[@]} -ne ${#PUBLIC_KEYS[@]} ]]; then
        fatal "Number of remote hosts (${#REMOTE_IPS[@]}) does not match number of validators (${#PUBLIC_KEYS[@]})"
    fi
    
    log "Found ${#PUBLIC_KEYS[@]} validators:"
    for i in "${!PUBLIC_KEYS[@]}"; do
        log "  Validator $((i+1)): ${PUBLIC_KEYS[i]} → ${REMOTE_IPS[i]}"
    done
}

# =============================================================================
# PHASE 3: GENERATE MODIFIED PEERS.YAML FILES
# =============================================================================

generate_peers_yaml() {
    local peers_template="$1"
    local current_key="$2"
    local output_file="$3"
    
    {
        while IFS= read -r line || [[ -n "$line" ]]; do
            # Handle "addresses:" header
            if [[ "$line" =~ ^addresses:[[:space:]]*$ ]]; then
                echo "$line"
                continue
            fi
            
            # Skip empty lines
            if [[ -z "$line" ]] || [[ "$line" =~ ^[[:space:]]*$ ]]; then
                echo "$line"
                continue
            fi
            
            # Parse peer entry: "  <public_key>: <ip>:<port>"
            if [[ "$line" =~ ^([[:space:]]+)([^:[:space:]]+):[[:space:]]*(.+)$ ]]; then
                local indent="${BASH_REMATCH[1]}"
                local peer_key="${BASH_REMATCH[2]}"
                local peer_addr="${BASH_REMATCH[3]}"
                
                # Extract port from address
                if [[ "$peer_addr" =~ ^([^:]+):(.+)$ ]]; then
                    local peer_port="${BASH_REMATCH[2]}"
                    
                    # Keep current validator's entry as 127.0.0.1
                    if [[ "${peer_key}" == "${current_key}" ]]; then
                        echo "${indent}${peer_key}: ${peer_addr}"
                    else
                        # Find IP for this peer
                        local peer_ip=""
                        local idx
                        for idx in "${!PUBLIC_KEYS[@]}"; do
                            if [[ "${PUBLIC_KEYS[idx]}" == "${peer_key}" ]]; then
                                peer_ip="${REMOTE_IPS[idx]}"
                                break
                            fi
                        done
                        
                        if [[ -n "${peer_ip}" ]]; then
                            echo "${indent}${peer_key}: ${peer_ip}:${peer_port}"
                        else
                            err "Could not resolve IP for peer ${peer_key}; keeping original entry"
                            echo "${indent}${peer_key}: ${peer_addr}"
                        fi
                    fi
                else
                    # Address format not recognized, keep original
                    echo "${indent}${peer_key}: ${peer_addr}"
                fi
            else
                # Line doesn't match expected format, keep as-is
                echo "$line"
            fi
        done < "${peers_template}"
    } > "${output_file}"
}

# =============================================================================
# PHASE 4: DEPLOY CONFIGS TO VMs
# =============================================================================

deploy_to_vms() {
    log "Phase 4: Deploying configs to VMs"
    
    local peers_template="${CONFIG_OUTPUT_DIR}/peers.yaml"
    local temp_dir
    temp_dir=$(mktemp -d)
    trap 'rm -rf "${temp_dir}"' EXIT
    
    for idx in "${!REMOTE_HOSTS[@]}"; do
        local host="${REMOTE_HOSTS[idx]}"
        local public_key="${PUBLIC_KEYS[idx]}"
        local config_file="${CONFIG_FILES[idx]}"
        local remote_ip="${REMOTE_IPS[idx]}"
        
        log "Deploying to ${remote_ip} (validator: ${public_key})"
        
        # Generate modified peers.yaml for this validator
        local peers_output="${temp_dir}/peers_${public_key}.yaml"
        generate_peers_yaml "${peers_template}" "${public_key}" "${peers_output}"
        
        # Create remote directories
        remote_cmd "${host}" "mkdir -p '${REMOTE_BASE_DIR}' '${REMOTE_LOG_DIR}'"
        
        # Copy config file
        copy_file "${config_file}" "${host}" "${REMOTE_BASE_DIR}/${public_key}.yaml"
        
        # Copy modified peers.yaml
        copy_file "${peers_output}" "${host}" "${REMOTE_BASE_DIR}/peers.yaml"
        
        log "  ✓ Deployed to ${remote_ip}"
    done
    
    rm -rf "${temp_dir}"
    trap - EXIT
}

# =============================================================================
# PHASE 5: START VALIDATORS ON VMs
# =============================================================================

start_validators() {
    log "Phase 5: Starting validators on VMs"
    
    # Clear array
    ACTIVE_SSH_PIDS=()
    
    for idx in "${!REMOTE_HOSTS[@]}"; do
        local host="${REMOTE_HOSTS[idx]}"
        local public_key="${PUBLIC_KEYS[idx]}"
        local remote_ip="${REMOTE_IPS[idx]}"
        
        log "Starting validator on ${remote_ip} (${public_key})"
        
        # Build command to run on remote VM
        local cmd
        cmd=$(cat <<EOF
cd '${REMOTE_REPO_DIR}' && \
PUBLIC_KEY='${public_key}' \
BASE_DIR='${REMOTE_BASE_DIR}' \
LOG_DIR='${REMOTE_LOG_DIR}' \
./remote-runs/run-validator.sh
EOF
)
        
        # Run in background and track PID
        remote_cmd "${host}" "${cmd}" &
        local pid=$!
        ACTIVE_SSH_PIDS+=("${pid}")
        log "  Started (SSH PID: ${pid})"
    done
    
    log "All validators started. Waiting for execution..."
    log "Press Ctrl+C to stop all validators"
}

# =============================================================================
# PHASE 6: CLEANUP ON INTERRUPT
# =============================================================================

cleanup_on_interrupt() {
    log ""
    log "Interrupt received. Stopping all validators..."
    
    # Stop validators on all VMs
    for idx in "${!REMOTE_HOSTS[@]}"; do
        local host="${REMOTE_HOSTS[idx]}"
        local remote_ip="${REMOTE_IPS[idx]}"
        
        log "Stopping validator on ${remote_ip}..."
        remote_cmd "${host}" "pkill -INT -f 'validator --' || true" || true
        sleep 1
        remote_cmd "${host}" "pkill -KILL -f 'validator --' || true" || true
    done
    
    # Kill local SSH processes
    for pid in "${ACTIVE_SSH_PIDS[@]}"; do
        if [[ -n "${pid}" ]] && kill -0 "${pid}" 2>/dev/null; then
            kill "${pid}" 2>/dev/null || true
        fi
    done
    
    wait 2>/dev/null || true
    
    log "Cleanup complete."
    exit 130
}

# =============================================================================
# VALIDATION
# =============================================================================

validate_prerequisites() {
    command_exists ssh || fatal "ssh not found"
    command_exists scp || fatal "scp not found"
    command_exists cargo || fatal "cargo not found"
    
    [[ ${#REMOTE_HOSTS[@]} -gt 0 ]] || fatal "REMOTE_HOSTS array is empty"
    [[ ${#REMOTE_HOSTS[@]} -eq ${V} ]] || fatal "Number of REMOTE_HOSTS (${#REMOTE_HOSTS[@]}) does not match V (${V})"
    
    if [[ ! -d "${REPO_ROOT}/chain" ]]; then
        fatal "Expected to find 'chain/' directory at ${REPO_ROOT}/chain"
    fi
    
    mkdir -p "${CONFIG_OUTPUT_DIR}"
}

# =============================================================================
# MAIN EXECUTION
# =============================================================================

main() {
    validate_prerequisites
    
    # Set up signal handlers
    trap cleanup_on_interrupt INT TERM
    
    # Execute phases
    generate_configs
    collect_validator_info
    deploy_to_vms
    start_validators
    
    # Wait for all SSH processes
    local status=0
    for pid in "${ACTIVE_SSH_PIDS[@]}"; do
        if ! wait "${pid}"; then
            status=1
        fi
    done
    
    # Clear trap on normal completion
    trap - INT TERM
    
    if [[ "${status}" -ne 0 ]]; then
        fatal "One or more validators failed"
    fi
    
    log "All validators completed successfully."
}

main "$@"

