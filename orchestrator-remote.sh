#!/usr/bin/env bash

set -euo pipefail

# Remote orchestrator script for running validators on remote machines
# This script should be run on each remote validator machine
#
# Usage:
#   ./orchestrator-remote.sh [PUBLIC_KEY] [MAX_INSTANCES] [RUNS_PER_INSTANCE] [SLEEP_SECONDS]
#
# Examples:
#   # Auto-detect public key, use defaults (MAX_INSTANCES=10, RUNS_PER_INSTANCE=2, SLEEP_SECONDS=300)
#   ./orchestrator-remote.sh
#
#   # Specify public key explicitly
#   ./orchestrator-remote.sh 9fabcf5e25d70ebd590a8d25bc8c8a152ed60983fa2a09bb4b36621b1c5d6a52
#
#   # Custom parameters: max instances=5, 3 runs per instance, 600s sleep
#   ./orchestrator-remote.sh 9fabcf5e25d70ebd590a8d25bc8c8a152ed60983fa2a09bb4b36621b1c5d6a52 5 3 600
#
# Environment variables (override defaults):
#   BASE_DIR: Directory containing config files (default: /root/alto/deploy/manual)
#   VALIDATOR_BINARY: Path to validator binary (auto-detected if not set)
#   LOG_DIR: Directory for log files (default: /root/alto/logs/validators)
#   REPO_ROOT: Repository root directory (default: /root/alto)
#
# If PUBLIC_KEY is not provided, the script will try to auto-detect it from available config files

# ============================================================================
# CONFIGURATION
# ============================================================================

# Default values (can be overridden via command line or environment)
PUBLIC_KEY="${1:-}"
MAX_INSTANCES="${MAX_INSTANCES:-${2:-10}}"
RUNS_PER_INSTANCES="${RUNS_PER_INSTANCES:-${3:-2}}"
SLEEP_SECONDS="${SLEEP_SECONDS:-${4:-300}}"
SETTLE_SECONDS="${SETTLE_SECONDS:-4}"

# Base directory where configs are stored (adjust if needed)
BASE_DIR="${BASE_DIR:-/root/alto/deploy/manual}"

# Validator binary path (adjust if needed)
# Script will auto-detect if not set and binary doesn't exist
VALIDATOR_BINARY="${VALIDATOR_BINARY:-}"

# Log directory
LOG_DIR="${LOG_DIR:-/root/alto/logs/validators}"
mkdir -p "${LOG_DIR}"

# Repo root (for cargo run fallback)
REPO_ROOT="${REPO_ROOT:-/root/alto}"

# ============================================================================
# VALIDATE BASE DIR EXISTS
# ============================================================================

if [[ ! -d "${BASE_DIR}" ]]; then
    echo "Error: Base directory does not exist: ${BASE_DIR}" >&2
    echo "Please create it or set BASE_DIR environment variable" >&2
    exit 1
fi

# ============================================================================
# DETECT PUBLIC KEY IF NOT PROVIDED
# ============================================================================

if [[ -z "${PUBLIC_KEY}" ]]; then
    echo "[orchestrator-remote] Auto-detecting public key from config files..."
    
    # Find all .yaml files in BASE_DIR (excluding peers.yaml)
    CONFIG_FILES=()
    while IFS= read -r line; do
        [[ -n "$line" ]] && CONFIG_FILES+=("$line")
    done < <(find "${BASE_DIR}" -maxdepth 1 -type f -name "*.yaml" \
        ! -name "peers.yaml" \
        -print | LC_ALL=C sort)
    
    if [[ ${#CONFIG_FILES[@]} -eq 0 ]]; then
        echo "Error: No config files found in ${BASE_DIR}" >&2
        echo "Please provide PUBLIC_KEY as first argument or place config files in ${BASE_DIR}" >&2
        exit 1
    fi
    
    # If multiple configs found, use the first one (user should specify if ambiguous)
    if [[ ${#CONFIG_FILES[@]} -gt 1 ]]; then
        echo "Warning: Multiple config files found. Using first one: $(basename "${CONFIG_FILES[0]}")" >&2
        echo "Available configs:" >&2
        for cfg in "${CONFIG_FILES[@]}"; do
            echo "  - $(basename "${cfg}")" >&2
        done
        echo "To use a specific config, provide PUBLIC_KEY as first argument" >&2
    fi
    
    CONFIG_FILE="${CONFIG_FILES[0]}"
    PUBLIC_KEY="$(basename "${CONFIG_FILE}" .yaml)"
    echo "[orchestrator-remote] Detected public key: ${PUBLIC_KEY}"
else
    CONFIG_FILE="${BASE_DIR}/${PUBLIC_KEY}.yaml"
fi

# ============================================================================
# VALIDATION
# ============================================================================

PEERS_FILE="${BASE_DIR}/peers.yaml"

if [[ ! -f "${PEERS_FILE}" ]]; then
    echo "Error: peers.yaml not found at ${PEERS_FILE}" >&2
    exit 1
fi

if [[ ! -f "${CONFIG_FILE}" ]]; then
    echo "Error: Config file not found at ${CONFIG_FILE}" >&2
    exit 1
fi

# Auto-detect validator binary if not set or doesn't exist
if [[ -z "${VALIDATOR_BINARY}" ]] || [[ ! -f "${VALIDATOR_BINARY}" ]]; then
    # Try common locations
    CANDIDATES=(
        "/root/alto/target/release/validator"
        "/root/alto/target/debug/validator"
        "${REPO_ROOT}/target/release/validator"
        "${REPO_ROOT}/target/debug/validator"
    )
    
    VALIDATOR_BINARY=""
    for candidate in "${CANDIDATES[@]}"; do
        if [[ -f "${candidate}" ]]; then
            VALIDATOR_BINARY="${candidate}"
            echo "[orchestrator-remote] Found validator binary at: ${VALIDATOR_BINARY}"
            break
        fi
    done
    
    if [[ -z "${VALIDATOR_BINARY}" ]]; then
        echo "Error: Validator binary not found in any standard location" >&2
        echo "Please build it with: cd ${REPO_ROOT} && cargo build --release --bin validator" >&2
        echo "Or set VALIDATOR_BINARY environment variable" >&2
        exit 1
    fi
fi

# Validate binary is executable
if [[ ! -x "${VALIDATOR_BINARY}" ]]; then
    echo "Warning: Validator binary exists but is not executable. Making it executable..." >&2
    chmod +x "${VALIDATOR_BINARY}" || {
        echo "Error: Failed to make binary executable" >&2
        exit 1
    }
fi

# Check storage directory
STORAGE_DIR=$(grep "^directory:" "${CONFIG_FILE}" | awk '{print $2}' || echo "")
if [[ -z "${STORAGE_DIR}" ]]; then
    echo "Warning: Could not find 'directory:' in config file. Continuing anyway..." >&2
else
    if [[ ! -d "${STORAGE_DIR}" ]]; then
        echo "Storage directory does not exist: ${STORAGE_DIR}"
        echo "Creating it..."
        mkdir -p "${STORAGE_DIR}"
        echo "Created: ${STORAGE_DIR}"
    fi
fi


# ============================================================================
# HELPER FUNCTIONS
# ============================================================================

kill_validator() {
    local instances="$1"
    echo "[orchestrator-remote] Stopping validator processes for instances=${instances}..."
    
    # Patterns to match validator processes
    local patterns=(
        "validator --"
        "cargo run --bin validator"
    )
    
    # Graceful stop first (SIGINT)
    for pat in "${patterns[@]}"; do
        pids=$(pgrep -fl "$pat" 2>/dev/null | awk '{print $1}' || true)
        if [[ -n "$pids" ]]; then
            echo "$pids" | xargs kill -INT 2>/dev/null || true
        fi
    done
    
    # Short grace period
    sleep 2
    
    # Force kill any leftovers (SIGKILL)
    for pat in "${patterns[@]}"; do
        pids=$(pgrep -fl "$pat" 2>/dev/null | awk '{print $1}' || true)
        if [[ -n "$pids" ]]; then
            echo "$pids" | xargs kill -KILL 2>/dev/null || true
        fi
    done
    
    # Additional cleanup: kill by instances pattern
    pkill -f "consensus-instances ${instances}" 2>/dev/null || true
}

# ============================================================================
# MAIN ORCHESTRATION LOOP
# ============================================================================

echo "[orchestrator-remote] Starting orchestration"
echo "  Public Key: ${PUBLIC_KEY}"
echo "  Config: ${CONFIG_FILE}"
echo "  Peers: ${PEERS_FILE}"
echo "  Binary: ${VALIDATOR_BINARY}"
echo "  Max Instances: ${MAX_INSTANCES}"
echo "  Runs per Instance: ${RUNS_PER_INSTANCES}"
echo "  Sleep between runs: ${SLEEP_SECONDS}s"
echo "  Log directory: ${LOG_DIR}"
echo ""

for instances in $(seq 1 ${MAX_INSTANCES}); do
    for runIndex in $(seq 1 ${RUNS_PER_INSTANCES}); do
        echo "[orchestrator-remote] ========================================="
        echo "[orchestrator-remote] Starting run: instances=${instances}, run=${runIndex}"
        
        # Pre-run cleanup
        echo "[orchestrator-remote] Pre-clean: stopping any leftover validators..."
        kill_validator "${instances}" || true
        sleep 3
        
        # Set up log file
        LOG_FILE="${LOG_DIR}/val_${PUBLIC_KEY}_i${instances}_r${runIndex}.log"
        echo "[orchestrator-remote] Log file: ${LOG_FILE}"
        
        # Start validator
        echo "[orchestrator-remote] Starting validator..."
        ulimit -n 65536 || true
        
        # Build command (run from repo root to ensure any relative paths work)
        VAL_CMD="cd '${REPO_ROOT}' && ulimit -n 65536 && '${VALIDATOR_BINARY}' \
            --peers='${PEERS_FILE}' \
            --config='${CONFIG_FILE}' \
            --gatling \
            --no-gossip-txs \
            --consensus-instances ${instances} \
            2>&1 | sed 's/\\x1b\[[0-9;]*m//g' | tee -a '${LOG_FILE}'"
        
        # Run validator in background and capture PID
        bash -c "${VAL_CMD}" &
        VALIDATOR_PID=$!
        echo "[orchestrator-remote] Validator started with PID: ${VALIDATOR_PID}"
        
        # Wait for the specified duration
        echo "[orchestrator-remote] Sleeping ${SLEEP_SECONDS}s..."
        sleep "${SLEEP_SECONDS}"
        
        # Stop validator
        echo "[orchestrator-remote] Stopping validator..."
        kill_validator "${instances}"
        
        # Wait a bit more for graceful shutdown
        if kill -0 "${VALIDATOR_PID}" 2>/dev/null; then
            echo "[orchestrator-remote] Waiting for validator to stop gracefully..."
            sleep 5
            # Force kill if still running
            if kill -0 "${VALIDATOR_PID}" 2>/dev/null; then
                kill -KILL "${VALIDATOR_PID}" 2>/dev/null || true
            fi
        fi
        
        echo "[orchestrator-remote] Settle ${SETTLE_SECONDS}s"
        sleep "${SETTLE_SECONDS}"
        
        echo "[orchestrator-remote] Completed: instances=${instances}, run=${runIndex}"
        echo ""
    done
done

echo "[orchestrator-remote] ========================================="
echo "[orchestrator-remote] Completed all runs."
echo "[orchestrator-remote] Logs are available in: ${LOG_DIR}"

