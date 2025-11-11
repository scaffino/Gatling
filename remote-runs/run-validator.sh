#!/usr/bin/env bash

set -euo pipefail

# =============================================================================
# Remote validator runner script
# This script runs on each VM to start a validator
# =============================================================================

# =============================================================================
# CONFIGURATION
# =============================================================================

# Parameters (passed from run-remote.sh or environment)
PUBLIC_KEY="${PUBLIC_KEY:-}"
BASE_DIR="${BASE_DIR:-/root/alto/deploy/manual}"
LOG_DIR="${LOG_DIR:-/root/alto/logs/validator}"
REPO_ROOT="${REPO_ROOT:-/root/alto}"

# =============================================================================
# HELPER FUNCTIONS
# =============================================================================

log() {
    printf '[run-validator] %s\n' "$*"
}

err() {
    printf '[run-validator] ERROR: %s\n' "$*" >&2
}

fatal() {
    err "$@"
    exit 1
}

# =============================================================================
# VALIDATION
# =============================================================================

if [[ -z "${PUBLIC_KEY}" ]]; then
    fatal "PUBLIC_KEY not provided. Set it as environment variable or script parameter."
fi

CONFIG_FILE="${BASE_DIR}/${PUBLIC_KEY}.yaml"
PEERS_FILE="${BASE_DIR}/peers.yaml"

if [[ ! -f "${CONFIG_FILE}" ]]; then
    fatal "Config file not found at ${CONFIG_FILE}"
fi

if [[ ! -f "${PEERS_FILE}" ]]; then
    fatal "peers.yaml not found at ${PEERS_FILE}"
fi

# Create log directory
mkdir -p "${LOG_DIR}"

# =============================================================================
# DETECT VALIDATOR BINARY
# =============================================================================

detect_validator_binary() {
    # Try common locations
    local candidates=(
        "/root/alto/target/release/validator"
        # "/root/alto/target/debug/validator"
        "${REPO_ROOT}/target/release/validator"
        # "${REPO_ROOT}/target/debug/validator"
    )
    
    for candidate in "${candidates[@]}"; do
        if [[ -f "${candidate}" ]]; then
            echo "${candidate}"
            return 0
        fi
    done
    
    fatal "Validator binary not found. Please build it with: cd ${REPO_ROOT} && cargo build --release --bin validator"
}

VALIDATOR_BINARY=$(detect_validator_binary)
log "Using validator binary: ${VALIDATOR_BINARY}"

# Validate binary is executable
if [[ ! -x "${VALIDATOR_BINARY}" ]]; then
    log "Making validator binary executable..."
    chmod +x "${VALIDATOR_BINARY}" || fatal "Failed to make binary executable"
fi

# =============================================================================
# CHECK STORAGE DIRECTORY
# =============================================================================

STORAGE_DIR=$(grep "^directory:" "${CONFIG_FILE}" | awk '{print $2}' || echo "")
if [[ -n "${STORAGE_DIR}" ]]; then
    if [[ ! -d "${STORAGE_DIR}" ]]; then
        log "Creating storage directory: ${STORAGE_DIR}"
        mkdir -p "${STORAGE_DIR}"
    fi
else
    log "Warning: Could not find 'directory:' in config file"
fi

# =============================================================================
# CLEANUP HANDLER
# =============================================================================

CURRENT_VALIDATOR_PID=""

cleanup_on_exit() {
    log "Interrupt received. Stopping validator..."
    
    if [[ -n "${CURRENT_VALIDATOR_PID:-}" ]] && kill -0 "${CURRENT_VALIDATOR_PID}" 2>/dev/null; then
        log "Killing validator process ${CURRENT_VALIDATOR_PID}"
        kill -INT "${CURRENT_VALIDATOR_PID}" 2>/dev/null || true
        sleep 2
        if kill -0 "${CURRENT_VALIDATOR_PID}" 2>/dev/null; then
            kill -KILL "${CURRENT_VALIDATOR_PID}" 2>/dev/null || true
        fi
    fi
    
    # Fallback: kill by pattern
    pkill -INT -f "validator --" 2>/dev/null || true
    sleep 1
    pkill -KILL -f "validator --" 2>/dev/null || true
    
    log "Cleanup complete."
    exit 130
}

trap cleanup_on_exit SIGINT SIGTERM

# =============================================================================
# START VALIDATOR
# =============================================================================

log "Starting validator"
log "  Public key: ${PUBLIC_KEY}"
log "  Config: ${CONFIG_FILE}"
log "  Peers: ${PEERS_FILE}"
log "  Log file: ${LOG_DIR}/val_${PUBLIC_KEY}.log"

# Set up log file
LOG_FILE="${LOG_DIR}/val_${PUBLIC_KEY}.log"

# Set ulimit for file descriptors
ulimit -n 65536 || true

# Optional: stagger non-bootstrapper startup so bootstrapper starts sooner
# Set delay (seconds) for non-bootstrapper nodes; override with env var if desired
STARTUP_STAGGER_NON_BOOTSTRAPPER="${STARTUP_STAGGER_NON_BOOTSTRAPPER:-5}"
if [[ "${STARTUP_STAGGER_NON_BOOTSTRAPPER}" -gt 0 ]]; then
    # Detect if this validator is listed as a bootstrapper in its config
    IS_BOOTSTRAPPER=false
    if command -v awk >/dev/null 2>&1; then
        # Extract bootstrappers block robustly and check for PUBLIC_KEY
        if awk '
            BEGIN { in_boot=0 }
            /^bootstrappers:/ { in_boot=1; next }
            in_boot==1 && /^- / { gsub(/^- /, "", $0); print; next }
            in_boot==1 && /^[a-z_]+:/ { in_boot=0 }
        ' "${CONFIG_FILE}" | grep -qx "${PUBLIC_KEY}"; then
            IS_BOOTSTRAPPER=true
        fi
    else
        # Fallback: simple grep (less robust, but fine as a fallback)
        if grep -q "^- ${PUBLIC_KEY}\$" "${CONFIG_FILE}"; then
            IS_BOOTSTRAPPER=true
        fi
    fi
    if [[ "${IS_BOOTSTRAPPER}" == "false" ]]; then
        log "Non-bootstrapper detected. Sleeping ${STARTUP_STAGGER_NON_BOOTSTRAPPER}s to let bootstrapper start..."
        sleep "${STARTUP_STAGGER_NON_BOOTSTRAPPER}"
    else
        log "Bootstrapper detected. Starting immediately."
    fi
fi

# Build validator command
# Run from repo root to ensure any relative paths work
CONSENSUS_INSTANCES="${CONSENSUS_INSTANCES:-3}"
VALIDATOR_CMD="cd '${REPO_ROOT}' && ulimit -n 65536 && '${VALIDATOR_BINARY}' \
    --peers='${PEERS_FILE}' \
    --config='${CONFIG_FILE}' \
    --gatling \
    --no-gossip-txs \
    --consensus-instances ${CONSENSUS_INSTANCES} \
    2>&1 | sed 's/\\x1b\[[0-9;]*m//g' > '${LOG_FILE}'"

# Start validator in background
bash -c "${VALIDATOR_CMD}" &
CURRENT_VALIDATOR_PID=$!

log "Validator started (PID: ${CURRENT_VALIDATOR_PID})"
log "Logging to: ${LOG_FILE}"
log "Running until interrupted (Ctrl+C)..."

# Wait for validator process
wait "${CURRENT_VALIDATOR_PID}" || {
    local exit_code=$?
    if [[ ${exit_code} -ne 130 ]]; then
        err "Validator exited with code ${exit_code}"
        exit ${exit_code}
    fi
}

# Clear trap on normal completion
trap - SIGINT SIGTERM

log "Validator stopped."

