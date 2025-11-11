#!/usr/bin/env bash

set -euo pipefail

# =============================================================================
# Main orchestrator script for running validators on remote VMs
# This script runs locally on your laptop and orchestrates everything
#
# Features:
# - Generates validator configs locally
# - Deploys configs to remote VMs
# - Starts validators on all VMs
# - Optionally submits transactions to validators (see ENABLE_TX_SUBMISSION)
# =============================================================================

# =============================================================================
# CONFIGURATION
# =============================================================================

# Transaction submission parameters (optional)
ENABLE_TX_SUBMISSION="${ENABLE_TX_SUBMISSION:-1}"
TX_NUM_TXS="${TX_NUM_TXS:-50}"
TX_SENDER_SEED="${TX_SENDER_SEED:-999}"
TX_RECEIVER_PUBKEY="${TX_RECEIVER_PUBKEY:-3ecf551aeb957616c6c8aa603634ea55845f88712a58745e58a71fe988bb967a}"
TX_SUBMITTER_INDEX="${TX_SUBMITTER_INDEX:-0}"  # Which validator index to submit from (0-based)
TX_STARTUP_WAIT="${TX_STARTUP_WAIT:-120}"      # Seconds to wait for validators to be ready before submitting
TX_SUBMIT_TO_ALL="${TX_SUBMIT_TO_ALL:-1}"     # If 1, submit from every validator; if 0, only TX_SUBMITTER_INDEX
TX_DRIVE_FROM_LOCAL="${TX_DRIVE_FROM_LOCAL:-1}" # If 1, create/send txs from this laptop
TX_BATCH_GAP="${TX_BATCH_GAP:-20}"            # Seconds gap between batches to consecutive validators (local driver)

# Number of consensus instances to run
CONSENSUS_INSTANCES="${CONSENSUS_INSTANCES:-3}"

# Number of validators to generate
V=6

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
REMOTE_STORAGE_DIR="${REMOTE_STORAGE_DIR:-/root/alto/deploy/manual}"

# =============================================================================
# HELPER FUNCTIONS (needed early)
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

# Remote validator machine IPs (order must match validator assignment)
# Load remote hosts from file
load_remote_hosts

# Setup parameters (passed to cargo run --bin setup)
SETUP_PEERS="${V}"
SETUP_BOOTSTRAPPERS="1"
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
LOCAL_VALIDATOR_PID=""
LOCAL_VALIDATOR_INDEX=-1
TX_SUBMITTER_PID=""
TX_SUBMITTER_PIDS=()

# =============================================================================
# HELPER FUNCTIONS
# =============================================================================

command_exists() {
    command -v "$1" >/dev/null 2>&1
}

detect_submit_tx_binary() {
    # Try common locations relative to repo root
    local candidates=(
        "${REPO_ROOT}/target/release/submit_tx"
        "${REPO_ROOT}/target/debug/submit_tx"
    )
    local candidate
    for candidate in "${candidates[@]}"; do
        if [[ -f "${candidate}" ]]; then
            echo "${candidate}"
            return 0
        fi
    done
    # Not found
    echo ""
    return 1
}

remote_cmd() {
    local host="$1"
    shift
    
    if is_localhost "${host}"; then
        # Run command locally
        "$@"
    else
        # Run command via SSH
        ssh "${SSH_OPTS[@]}" "${host}" "$@"
    fi
}

copy_file() {
    local source="$1"
    local host="$2"
    local destination="$3"
    
    if is_localhost "${host}"; then
        # Copy file locally
        mkdir -p "$(dirname "${destination}")"
        cp "${source}" "${destination}"
    else
        # Copy file via SCP
        scp "${SSH_OPTS[@]}" "${source}" "${host}:${destination}"
    fi
}

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
    local ip=$(extract_ip "${host}")
    [[ "${ip}" == "127.0.0.1" ]] || [[ "${ip}" == "localhost" ]] || [[ "${ip}" == "::1" ]]
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
    
    # Extract IPs from REMOTE_HOSTS and detect localhost
    LOCAL_VALIDATOR_INDEX=-1
    for idx in "${!REMOTE_HOSTS[@]}"; do
        local host="${REMOTE_HOSTS[idx]}"
        local ip=$(extract_ip "${host}")
        REMOTE_IPS+=("${ip}")
        
        # Check if this is localhost
        if is_localhost "${host}"; then
            if [[ ${LOCAL_VALIDATOR_INDEX} -ne -1 ]]; then
                fatal "Multiple localhost entries found. Only one localhost validator is supported."
            fi
            LOCAL_VALIDATOR_INDEX=${idx}
            log "Detected localhost validator at index ${idx}"
        fi
    done
    
    if [[ ${#REMOTE_IPS[@]} -ne ${#PUBLIC_KEYS[@]} ]]; then
        fatal "Number of remote hosts (${#REMOTE_IPS[@]}) does not match number of validators (${#PUBLIC_KEYS[@]})"
    fi
    
    log "Found ${#PUBLIC_KEYS[@]} validators:"
    for i in "${!PUBLIC_KEYS[@]}"; do
        local host_type="remote"
        if [[ ${i} -eq ${LOCAL_VALIDATOR_INDEX} ]]; then
            host_type="local"
        fi
        log "  Validator $((i+1)): ${PUBLIC_KEYS[i]} → ${REMOTE_IPS[i]} (${host_type})"
    done
}

# =============================================================================
# HELPER: UPDATE STORAGE PATH IN CONFIG FILE
# =============================================================================

update_storage_path_in_config() {
    local config_file="$1"
    local new_storage_dir="$2"
    local output_file="$3"
    
    # Use Python for robust YAML manipulation (if available), otherwise use sed
    if command_exists python3; then
        # Use Python to update YAML
        python3 <<EOF
import yaml
import sys

# Read the YAML file
with open('${config_file}', 'r') as f:
    config = yaml.safe_load(f)

# Update directory
config['directory'] = '${new_storage_dir}'

# Write to output file
with open('${output_file}', 'w') as f:
    yaml.dump(config, f, default_flow_style=False, sort_keys=False)
EOF
        if [[ $? -eq 0 ]]; then
            return 0
        else
            log "  Python YAML update failed, falling back to sed"
        fi
    fi
    
    # Fallback: Use sed to update directory line
    if sed "s|^directory:.*|directory: ${new_storage_dir}|" "${config_file}" > "${output_file}"; then
        return 0
    else
        err "  Failed to update directory in ${config_file}"
        return 1
    fi
}

# =============================================================================
# PHASE 3: GENERATE MODIFIED PEERS.YAML FILES
# =============================================================================

generate_peers_yaml() {
    local peers_template="$1"
    local current_key="$2"
    local output_file="$3"
    local current_validator_ip="${4:-}"  # Optional: IP for current validator (if empty, keeps original)
    
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
                    
                    # Handle current validator's entry
                    if [[ "${peer_key}" == "${current_key}" ]]; then
                        # If current_validator_ip is provided, use it (for remote VMs)
                        # Otherwise, keep original address (for localhost)
                        if [[ -n "${current_validator_ip}" ]]; then
                            echo "${indent}${peer_key}: ${current_validator_ip}:${peer_port}"
                        else
                            echo "${indent}${peer_key}: ${peer_addr}"
                        fi
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
        local is_local=false
        
        if [[ ${idx} -eq ${LOCAL_VALIDATOR_INDEX} ]]; then
            is_local=true
            log "Deploying to localhost (validator: ${public_key})"
        else
            log "Deploying to ${remote_ip} (validator: ${public_key})"
        fi
        
        # Generate modified peers.yaml for this validator
        local peers_output="${temp_dir}/peers_${public_key}.yaml"
        # For remote VMs, pass the public IP so the validator advertises itself correctly
        # For localhost, don't pass IP (keeps 127.0.0.1)
        if [[ "${is_local}" == "true" ]]; then
            generate_peers_yaml "${peers_template}" "${public_key}" "${peers_output}" ""
        else
            generate_peers_yaml "${peers_template}" "${public_key}" "${peers_output}" "${remote_ip}"
        fi
        
        if [[ "${is_local}" == "true" ]]; then
            # For localhost, use local paths directly (keep original config)
            local local_base_dir="${REPO_ROOT}/chain/test-remote"
            local local_log_dir="${REPO_ROOT}/logs/validator"
            
            # Create local directories
            mkdir -p "${local_base_dir}" "${local_log_dir}"
            
            # Copy config file (it's already in the right place, but ensure peers.yaml is updated)
            cp "${peers_output}" "${local_base_dir}/peers.yaml"
            
            log "  ✓ Deployed to localhost"
        else
            # For remote validators, update storage directory to VM-relative path
            local modified_config="${temp_dir}/config_${public_key}.yaml"
            local new_storage_dir="${REMOTE_STORAGE_DIR}/${public_key}"
            
            # Extract current directory path for logging
            local current_dir
            current_dir=$(grep "^directory:" "${config_file}" | awk '{print $2}' || echo "")
            
            if [[ -n "${current_dir}" ]] && [[ "${current_dir}" != "${new_storage_dir}" ]]; then
                log "  Updating storage directory: ${current_dir} → ${new_storage_dir}"
                update_storage_path_in_config "${config_file}" "${new_storage_dir}" "${modified_config}"
                config_file="${modified_config}"
            else
                # Already has correct path or couldn't extract, use original
                cp "${config_file}" "${modified_config}"
                config_file="${modified_config}"
            fi
            
            # Create remote directories (including per-validator storage directory)
            remote_cmd "${host}" "mkdir -p '${REMOTE_BASE_DIR}' '${REMOTE_LOG_DIR}' '${new_storage_dir}' '${REMOTE_REPO_DIR}/remote-runs'"
            
            # Copy modified config file
            copy_file "${config_file}" "${host}" "${REMOTE_BASE_DIR}/${public_key}.yaml"
            
            # Copy modified peers.yaml
            copy_file "${peers_output}" "${host}" "${REMOTE_BASE_DIR}/peers.yaml"
            
            # Ensure the remote run-validator.sh is up-to-date and executable
            copy_file "${SCRIPT_DIR}/run-validator.sh" "${host}" "${REMOTE_REPO_DIR}/remote-runs/run-validator.sh"
            remote_cmd "${host}" "chmod +x '${REMOTE_REPO_DIR}/remote-runs/run-validator.sh'"
            
            # Copy submitTx-remote.sh and ensure it's executable
            copy_file "${SCRIPT_DIR}/submitTx-remote.sh" "${host}" "${REMOTE_REPO_DIR}/remote-runs/submitTx-remote.sh"
            remote_cmd "${host}" "chmod +x '${REMOTE_REPO_DIR}/remote-runs/submitTx-remote.sh'"
            
            log "  ✓ Deployed to ${remote_ip} (storage: ${new_storage_dir})"
        fi
    done
    
    rm -rf "${temp_dir}"
    trap - EXIT
}

# =============================================================================
# PHASE 5: START VALIDATORS ON VMs
# =============================================================================

start_validators() {
    log "Phase 5: Starting validators"
    
    # Clear array and local PID
    ACTIVE_SSH_PIDS=()
    LOCAL_VALIDATOR_PID=""
    
    # Start localhost validator first if it exists
    if [[ ${LOCAL_VALIDATOR_INDEX} -ge 0 ]]; then
        local public_key="${PUBLIC_KEYS[LOCAL_VALIDATOR_INDEX]}"
        local config_file="${CONFIG_FILES[LOCAL_VALIDATOR_INDEX]}"
        
        log "Starting localhost validator (${public_key})"
        
        # Use local paths
        local local_base_dir="${REPO_ROOT}/chain/test-remote"
        local local_log_dir="${REPO_ROOT}/logs/validator"
        mkdir -p "${local_log_dir}"
        
        # Check if run-validator.sh exists locally, if not, run validator directly
        local validator_script="${SCRIPT_DIR}/run-validator.sh"
        if [[ -f "${validator_script}" ]]; then
            # Use the validator script
            (
                cd "${REPO_ROOT}/chain" && \
                PUBLIC_KEY="${public_key}" \
                BASE_DIR="${local_base_dir}" \
                LOG_DIR="${local_log_dir}" \
                REPO_ROOT="${REPO_ROOT}" \
                "${validator_script}"
            ) &
            LOCAL_VALIDATOR_PID=$!
            log "  Started local validator (PID: ${LOCAL_VALIDATOR_PID})"
        else
            # Fallback: try to find and run validator binary directly
            local validator_binary="${REPO_ROOT}/chain/target/release/validator"
            if [[ ! -f "${validator_binary}" ]]; then
                validator_binary="${REPO_ROOT}/chain/target/debug/validator"
            fi
            
            if [[ -f "${validator_binary}" ]]; then
                log "Running validator binary directly: ${validator_binary}"
                (
                    cd "${REPO_ROOT}/chain" && \
                    "${validator_binary}" \
                        --peers="${local_base_dir}/peers.yaml" \
                        --config="${config_file}" \
                        --gatling \
                        --no-gossip-txs \
                        --consensus-instances "${CONSENSUS_INSTANCES}" \
                        >> "${local_log_dir}/val_${public_key}.log" 2>&1
                ) &
                LOCAL_VALIDATOR_PID=$!
                log "  Started local validator (PID: ${LOCAL_VALIDATOR_PID})"
            else
                fatal "Could not find validator binary or run-validator.sh script"
            fi
        fi
        
        # Wait a bit for local validator to start
        sleep 2
    fi
    
    # Start remote validators
    for idx in "${!REMOTE_HOSTS[@]}"; do
        # Skip localhost validator (already started)
        if [[ ${idx} -eq ${LOCAL_VALIDATOR_INDEX} ]]; then
            continue
        fi
        
        local host="${REMOTE_HOSTS[idx]}"
        local public_key="${PUBLIC_KEYS[idx]}"
        local remote_ip="${REMOTE_IPS[idx]}"
        
        # Add delay between validators (especially if localhost was first)
        if [[ ${LOCAL_VALIDATOR_INDEX} -ge 0 ]] || [[ ${idx} -gt 0 ]]; then
            sleep 2
        fi
        
        log "Starting validator on ${remote_ip} (${public_key})"
        
        # Build command to run on remote VM
        local cmd
        cmd=$(cat <<EOF
cd '${REMOTE_REPO_DIR}' && \
PUBLIC_KEY='${public_key}' \
BASE_DIR='${REMOTE_BASE_DIR}' \
LOG_DIR='${REMOTE_LOG_DIR}' \
CONSENSUS_INSTANCES='${CONSENSUS_INSTANCES}' \
./remote-runs/run-validator.sh
EOF
)
        
        # Run in background and track PID
        ssh "${SSH_OPTS[@]}" "${host}" "${cmd}" &
        local pid=$!
        ACTIVE_SSH_PIDS+=("${pid}")
        log "  Started (SSH PID: ${pid})"
    done
    
    log "All validators started. Waiting for execution..."
    log "Press Ctrl+C to stop all validators"
}

# =============================================================================
# PHASE 6: SUBMIT TRANSACTIONS (OPTIONAL)
# =============================================================================

submit_transactions() {
    # Auto-enable if using local driver and TX_NUM_TXS > 0 (user-friendly default)
    if [[ -z "${ENABLE_TX_SUBMISSION}" ]] && [[ "${TX_DRIVE_FROM_LOCAL}" == "1" ]] && [[ "${TX_NUM_TXS}" -gt 0 ]]; then
        log "Auto-enabling transaction submission (TX_DRIVE_FROM_LOCAL=1 and TX_NUM_TXS=${TX_NUM_TXS})"
        ENABLE_TX_SUBMISSION=1
    fi
    
    # Skip if transaction submission is not enabled
    if [[ -z "${ENABLE_TX_SUBMISSION}" ]]; then
        return 0
    fi
    
    log "Phase 6: Submitting transactions"
    # Wait for validators to be ready
    log "Waiting ${TX_STARTUP_WAIT} seconds for validators to be ready..."
    sleep "${TX_STARTUP_WAIT}"
    
    # Local driver: create and send transactions from this laptop to remote validators
    if [[ "${TX_DRIVE_FROM_LOCAL}" == "1" ]]; then
        log "Using local driver: transactions will be created and sent from this laptop"
        
        # Detect submit_tx binary
        local submit_tx_bin
        submit_tx_bin=$(detect_submit_tx_binary || true)
        if [[ -z "${submit_tx_bin}" ]]; then
            err "submit_tx binary not found."
            err "Build it with: cd ${REPO_ROOT} && cargo build --release --package alto-client --bin submit_tx"
            return 1
        fi
        if [[ ! -x "${submit_tx_bin}" ]]; then
            chmod +x "${submit_tx_bin}" || true
        fi
        log "Using submit_tx: ${submit_tx_bin}"
        
        # Build target index list
        local target_indexes=()
        if [[ "${TX_SUBMIT_TO_ALL}" == "1" ]]; then
            for t_idx in "${!PUBLIC_KEYS[@]}"; do
                target_indexes+=("${t_idx}")
            done
            log "Will submit ${TX_NUM_TXS} transactions per validator for ${#target_indexes[@]} validator(s)"
        else
            if [[ ${TX_SUBMITTER_INDEX} -lt 0 ]] || [[ ${TX_SUBMITTER_INDEX} -ge ${#PUBLIC_KEYS[@]} ]]; then
                err "Invalid TX_SUBMITTER_INDEX ${TX_SUBMITTER_INDEX}. Must be between 0 and $(( ${#PUBLIC_KEYS[@]} - 1 ))"
                return 1
            fi
            target_indexes+=("${TX_SUBMITTER_INDEX}")
            log "Will submit ${TX_NUM_TXS} transactions from validator index ${TX_SUBMITTER_INDEX}"
        fi
        
        # Sequentially submit to each validator with a gap between batches
        local j
        local LOCAL_TX_SUMMARY=()
        for j in "${!target_indexes[@]}"; do
            local idx="${target_indexes[j]}"
            local public_key="${PUBLIC_KEYS[idx]}"
            local ip="${REMOTE_IPS[idx]}"
            local config_file="${CONFIG_FILES[idx]}"
            
            # Extract transaction_port from local config
            local tx_port
            tx_port=$(grep "^transaction_port:" "${config_file}" | awk '{print $2}' || echo "")
            if [[ -z "${tx_port}" ]]; then
                err "transaction_port not found in ${config_file}"
                return 1
            fi
            
            local url="http://${ip}:${tx_port}"
            log "Submitting to validator ${public_key} at ${url} (${TX_NUM_TXS} txs)"
            
            local success=0
            local failed=0
            local i
            for i in $(seq 1 "${TX_NUM_TXS}"); do
                if "${submit_tx_bin}" \
                    --validator "${url}" \
                    --sender-seed "${TX_SENDER_SEED}" \
                    --receiver "${TX_RECEIVER_PUBKEY}" \
                    --amount "${i}" \
                    >/dev/null 2>&1; then
                    success=$((success + 1))
                else
                    failed=$((failed + 1))
                fi
                # Small delay between transactions to avoid spikes
                sleep 0.1
            done
            
            log "  Completed for ${public_key}: success=${success}, failed=${failed}"
            log "  Summary: sent ${success}/${TX_NUM_TXS} txs to ${public_key} at ${url}"
            LOCAL_TX_SUMMARY+=("$(printf '%s %s %s/%s' "${public_key}" "${url}" "${success}" "${TX_NUM_TXS}")")
            
            # Batch gap between validators, except after last one
            if [[ $(( j + 1 )) -lt ${#target_indexes[@]} ]]; then
                log "Sleeping ${TX_BATCH_GAP}s before next validator batch..."
                sleep "${TX_BATCH_GAP}"
            fi
        done
        
        log "Local transaction submission finished."
        if [[ ${#LOCAL_TX_SUMMARY[@]} -gt 0 ]]; then
            echo ""
            echo "[run-remote] ========================================="
            echo "[run-remote] Transaction submission summary (local driver):"
            local entry
            for entry in "${LOCAL_TX_SUMMARY[@]}"; do
                # entry format: "<pubkey> <url> <success>/<total>"
                local pk url counts
                pk="$(echo "${entry}" | awk '{print $1}')"
                url="$(echo "${entry}" | awk '{print $2}')"
                counts="$(echo "${entry}" | awk '{print $3}')"
                echo "[run-remote]   ${pk:0:16}... <- ${counts} at ${url}"
            done
            echo "[run-remote] ========================================="
        fi
        return 0
    fi
    
    # Decide target indexes (all or single) for remote/host-driven submissions
    local target_indexes=()
    if [[ "${TX_SUBMIT_TO_ALL}" == "1" ]]; then
        for idx in "${!REMOTE_HOSTS[@]}"; do
            target_indexes+=("${idx}")
        done
        log "Submitting ${TX_NUM_TXS} transactions from every validator (${#target_indexes[@]} total submitters)"
    else
        # Validate submitter index
        if [[ ${TX_SUBMITTER_INDEX} -lt 0 ]] || [[ ${TX_SUBMITTER_INDEX} -ge ${#REMOTE_HOSTS[@]} ]]; then
            err "Invalid TX_SUBMITTER_INDEX ${TX_SUBMITTER_INDEX}. Must be between 0 and $(( ${#REMOTE_HOSTS[@]} - 1 ))"
            return 1
        fi
        target_indexes+=("${TX_SUBMITTER_INDEX}")
        log "Submitting ${TX_NUM_TXS} transactions from validator index ${TX_SUBMITTER_INDEX}"
    fi
    
    # Launch submissions for each target
    TX_SUBMITTER_PIDS=()
    for idx in "${target_indexes[@]}"; do
        local submitter_host="${REMOTE_HOSTS[idx]}"
        local submitter_public_key="${PUBLIC_KEYS[idx]}"
        local submitter_ip="${REMOTE_IPS[idx]}"
        local is_local=false
        
        if [[ ${idx} -eq ${LOCAL_VALIDATOR_INDEX} ]]; then
            is_local=true
            log "Submitting from localhost (validator: ${submitter_public_key})"
        else
            log "Submitting from ${submitter_ip} (validator: ${submitter_public_key})"
        fi
        
        # Determine base directory based on whether submitter is local or remote
        local base_dir
        if [[ "${is_local}" == "true" ]]; then
            base_dir="${REPO_ROOT}/chain/test-remote"
        else
            base_dir="${REMOTE_BASE_DIR}"
        fi
        
        # Build command to run submitTx-remote.sh
        local cmd
        if [[ "${is_local}" == "true" ]]; then
            cmd=$(cat <<EOF
cd '${REPO_ROOT}' && \
NUM_TXS='${TX_NUM_TXS}' \
SENDER_SEED='${TX_SENDER_SEED}' \
RECEIVER_PUBKEY='${TX_RECEIVER_PUBKEY}' \
BASE_DIR='${base_dir}' \
PUBLIC_KEY='${submitter_public_key}' \
REPO_ROOT='${REPO_ROOT}' \
'${SCRIPT_DIR}/submitTx-remote.sh' \
    '${TX_NUM_TXS}' \
    '${TX_SENDER_SEED}' \
    '${TX_RECEIVER_PUBKEY}' \
    '${base_dir}' \
    '${submitter_public_key}'
EOF
)
            # Run in background
            bash -c "${cmd}" &
            local pid=$!
            TX_SUBMITTER_PIDS+=("${pid}")
            log "  Started local transaction submission (PID: ${pid})"
        else
            cmd=$(cat <<EOF
cd '${REMOTE_REPO_DIR}' && \
NUM_TXS='${TX_NUM_TXS}' \
SENDER_SEED='${TX_SENDER_SEED}' \
RECEIVER_PUBKEY='${TX_RECEIVER_PUBKEY}' \
BASE_DIR='${base_dir}' \
PUBLIC_KEY='${submitter_public_key}' \
REPO_ROOT='${REMOTE_REPO_DIR}' \
./remote-runs/submitTx-remote.sh \
    '${TX_NUM_TXS}' \
    '${TX_SENDER_SEED}' \
    '${TX_RECEIVER_PUBKEY}' \
    '${base_dir}' \
    '${submitter_public_key}'
EOF
)
            # Run in background via SSH
            ssh "${SSH_OPTS[@]}" "${submitter_host}" "${cmd}" &
            local pid=$!
            TX_SUBMITTER_PIDS+=("${pid}")
            log "  Started remote transaction submission on ${submitter_ip} (SSH PID: ${pid})"
        fi
        
        # Optional tiny stagger to avoid thundering herd
        sleep 0.2
    done
    
    log "Transaction submissions started for ${#target_indexes[@]} validator(s)."
}

# =============================================================================
# PHASE 7: CLEANUP ON INTERRUPT
# =============================================================================

cleanup_on_interrupt() {
    log ""
    log "Interrupt received. Stopping all validators and transaction submission..."
    
    # Stop transaction submissions if running
    if [[ -n "${TX_SUBMITTER_PIDS[*]:-}" ]]; then
        for pid in "${TX_SUBMITTER_PIDS[@]}"; do
            if [[ -n "${pid}" ]] && kill -0 "${pid}" 2>/dev/null; then
                log "Stopping transaction submission (PID: ${pid})..."
                kill -INT "${pid}" 2>/dev/null || true
            fi
        done
        sleep 1
        for pid in "${TX_SUBMITTER_PIDS[@]}"; do
            if [[ -n "${pid}" ]] && kill -0 "${pid}" 2>/dev/null; then
                kill -KILL "${pid}" 2>/dev/null || true
            fi
        done
    fi
    
    # Stop localhost validator if running
    if [[ -n "${LOCAL_VALIDATOR_PID:-}" ]] && kill -0 "${LOCAL_VALIDATOR_PID}" 2>/dev/null; then
        log "Stopping localhost validator (PID: ${LOCAL_VALIDATOR_PID})..."
        kill -INT "${LOCAL_VALIDATOR_PID}" 2>/dev/null || true
        sleep 1
        if kill -0 "${LOCAL_VALIDATOR_PID}" 2>/dev/null; then
            kill -KILL "${LOCAL_VALIDATOR_PID}" 2>/dev/null || true
        fi
    fi
    
    # Also try to kill any validator processes locally (fallback)
    pkill -INT -f "validator --" 2>/dev/null || true
    sleep 1
    pkill -KILL -f "validator --" 2>/dev/null || true
    
    # Stop validators on remote VMs
    for idx in "${!REMOTE_HOSTS[@]}"; do
        # Skip localhost (already handled)
        if [[ ${idx} -eq ${LOCAL_VALIDATOR_INDEX} ]]; then
            continue
        fi
        
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
    command_exists cargo || fatal "cargo not found"
    
    # Check for SSH/SCP only if we have remote hosts (non-localhost)
    local has_remote=false
    for host in "${REMOTE_HOSTS[@]}"; do
        if ! is_localhost "${host}"; then
            has_remote=true
            break
        fi
    done
    
    if [[ "${has_remote}" == "true" ]]; then
        command_exists ssh || fatal "ssh not found (required for remote hosts)"
        command_exists scp || fatal "scp not found (required for remote hosts)"
    fi
    
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
    submit_transactions
    
    # Wait for all SSH processes (validators)
    local status=0
    for pid in "${ACTIVE_SSH_PIDS[@]}"; do
        if ! wait "${pid}"; then
            status=1
        fi
    done
    
    # Wait for transaction submissions if enabled
    if [[ -n "${ENABLE_TX_SUBMISSION}" ]] && [[ -n "${TX_SUBMITTER_PIDS[*]:-}" ]]; then
        log "Waiting for ${#TX_SUBMITTER_PIDS[@]} transaction submission job(s) to complete..."
        for pid in "${TX_SUBMITTER_PIDS[@]}"; do
            if ! wait "${pid}"; then
                local tx_status=$?
                if [[ ${tx_status} -ne 130 ]]; then  # 130 is SIGINT, which is expected on Ctrl+C
                    err "A transaction submission job failed with exit code ${tx_status}"
                    status=1
                fi
            fi
        done
        if [[ "${status}" -eq 0 ]]; then
            log "All transaction submissions completed successfully"
        fi
    fi
    
    # Clear trap on normal completion
    trap - INT TERM
    
    if [[ "${status}" -ne 0 ]]; then
        fatal "One or more validators failed"
    fi
    
    log "All validators completed successfully."
}

main "$@"

