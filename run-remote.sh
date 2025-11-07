#!/usr/bin/env bash

set -euo pipefail

# Remote transaction submission script
# Submits transactions to the local validator only
#
# Usage:
#   ./run-remote.sh [NUM_TXS] [SENDER_SEED] [RECEIVER_PUBKEY] [BASE_DIR] [PUBLIC_KEY]
#
# Examples:
#   # Submit 100 transactions (default, auto-detect local validator)
#   ./run-remote.sh
#
#   # Submit 50 transactions
#   ./run-remote.sh 50
#
#   # Submit 100 transactions with custom sender seed and receiver
#   ./run-remote.sh 100 999 3ecf551aeb957616c6c8aa603634ea55845f88712a58745e58a71fe988bb967a
#
#   # Specify local validator public key explicitly
#   ./run-remote.sh 100 999 3ecf551aeb957616c6c8aa603634ea55845f88712a58745e58a71fe988bb967a /root/alto/deploy/manual 9fabcf5e25d70ebd590a8d25bc8c8a152ed60983fa2a09bb4b36621b1c5d6a52
#
# Environment variables (override defaults):
#   BASE_DIR: Directory containing config files (default: /root/alto/deploy/manual)
#   SUBMIT_TX_BINARY: Path to submit_tx binary (auto-detected if not set)
#   REPO_ROOT: Repository root directory (default: /root/alto)
#   PUBLIC_KEY: Local validator's public key (auto-detected if not set)

# ============================================================================
# CONFIGURATION
# ============================================================================

NUM_TXS="${1:-100}"
SENDER_SEED="${2:-999}"
RECEIVER_PUBKEY="${3:-3ecf551aeb957616c6c8aa603634ea55845f88712a58745e58a71fe988bb967a}"
BASE_DIR="${BASE_DIR:-${4:-/root/alto/deploy/manual}}"
PUBLIC_KEY="${PUBLIC_KEY:-${5:-}}"
REPO_ROOT="${REPO_ROOT:-/root/alto}"

# Submit transaction binary path (auto-detected if not set)
SUBMIT_TX_BINARY="${SUBMIT_TX_BINARY:-}"

# ============================================================================
# VALIDATION
# ============================================================================

PEERS_FILE="${BASE_DIR}/peers.yaml"

if [[ ! -f "${PEERS_FILE}" ]]; then
    echo "Error: peers.yaml not found at ${PEERS_FILE}" >&2
    exit 1
fi

if [[ ! -d "${BASE_DIR}" ]]; then
    echo "Error: Base directory does not exist: ${BASE_DIR}" >&2
    exit 1
fi

# Auto-detect submit_tx binary if not set
if [[ -z "${SUBMIT_TX_BINARY}" ]]; then
    CANDIDATES=(
        "${REPO_ROOT}/target/release/submit_tx"
        "${REPO_ROOT}/target/debug/submit_tx"
        "/root/alto/target/release/submit_tx"
        "/root/alto/target/debug/submit_tx"
    )
    
    for candidate in "${CANDIDATES[@]}"; do
        if [[ -f "${candidate}" ]]; then
            SUBMIT_TX_BINARY="${candidate}"
            echo "[run-remote] Found submit_tx binary at: ${SUBMIT_TX_BINARY}"
            break
        fi
    done
fi

if [[ -z "${SUBMIT_TX_BINARY}" ]] || [[ ! -f "${SUBMIT_TX_BINARY}" ]]; then
    echo "Error: submit_tx binary not found" >&2
    echo "Please build it with: cd ${REPO_ROOT} && cargo build --release --package alto-client --bin submit_tx" >&2
    echo "Or set SUBMIT_TX_BINARY environment variable" >&2
    exit 1
fi

if [[ ! -x "${SUBMIT_TX_BINARY}" ]]; then
    chmod +x "${SUBMIT_TX_BINARY}" || {
        echo "Error: Failed to make submit_tx binary executable" >&2
        exit 1
    }
fi

# ============================================================================
# DETECT LOCAL VALIDATOR
# ============================================================================

if [[ -z "${PUBLIC_KEY}" ]]; then
    echo "[run-remote] Auto-detecting local validator public key from config files..."
    
    # Find all .yaml files in BASE_DIR (excluding peers.yaml)
    CONFIG_FILES=()
    while IFS= read -r line; do
        [[ -n "$line" ]] && CONFIG_FILES+=("$line")
    done < <(find "${BASE_DIR}" -maxdepth 1 -type f -name "*.yaml" \
        ! -name "peers.yaml" \
        -print | LC_ALL=C sort)
    
    if [[ ${#CONFIG_FILES[@]} -eq 0 ]]; then
        echo "Error: No config files found in ${BASE_DIR}" >&2
        echo "Please provide PUBLIC_KEY as fifth argument or place config files in ${BASE_DIR}" >&2
        exit 1
    fi
    
    # If multiple configs found, use the first one (user should specify if ambiguous)
    if [[ ${#CONFIG_FILES[@]} -gt 1 ]]; then
        echo "Warning: Multiple config files found. Using first one: $(basename "${CONFIG_FILES[0]}")" >&2
        echo "Available configs:" >&2
        for cfg in "${CONFIG_FILES[@]}"; do
            echo "  - $(basename "${cfg}")" >&2
        done
        echo "To use a specific config, provide PUBLIC_KEY as fifth argument" >&2
    fi
    
    CONFIG_FILE="${CONFIG_FILES[0]}"
    PUBLIC_KEY="$(basename "${CONFIG_FILE}" .yaml)"
    echo "[run-remote] Detected local validator public key: ${PUBLIC_KEY}"
else
    CONFIG_FILE="${BASE_DIR}/${PUBLIC_KEY}.yaml"
fi

if [[ ! -f "${CONFIG_FILE}" ]]; then
    echo "Error: Config file not found at ${CONFIG_FILE}" >&2
    exit 1
fi

# ============================================================================
# EXTRACT LOCAL VALIDATOR URL
# ============================================================================

echo "[run-remote] Reading peers.yaml and config files..."
echo "[run-remote] Base directory: ${BASE_DIR}"
echo "[run-remote] Local validator: ${PUBLIC_KEY}"
echo ""

# Find the local validator in peers.yaml
LOCAL_VALIDATOR_URL=""
LOCAL_VALIDATOR_IP=""
LOCAL_VALIDATOR_P2P_PORT=""
LOCAL_VALIDATOR_TX_PORT=""

while IFS= read -r line; do
    # Skip empty lines and comments
    [[ -z "$line" || "$line" =~ ^[[:space:]]*# ]] && continue
    [[ "$line" =~ ^addresses: ]] && continue
    
    # Match pattern: "  <pubkey>: <ip>:<port>"
    if [[ "$line" =~ ^[[:space:]]+([a-f0-9]+):[[:space:]]+([^:]+):([0-9]+) ]]; then
        PUBKEY="${BASH_REMATCH[1]}"
        IP="${BASH_REMATCH[2]}"
        P2P_PORT="${BASH_REMATCH[3]}"
        
        # Check if this is the local validator
        if [[ "${PUBKEY}" == "${PUBLIC_KEY}" ]]; then
            # Read the config file to get transaction_port
            if [[ ! -f "${CONFIG_FILE}" ]]; then
                echo "Error: Config file not found for local validator: ${CONFIG_FILE}" >&2
                exit 1
            fi
            
            # Extract transaction_port from config file
            TRANSACTION_PORT=$(grep "^transaction_port:" "${CONFIG_FILE}" | awk '{print $2}' || echo "")
            
            if [[ -z "${TRANSACTION_PORT}" ]]; then
                echo "Error: transaction_port not found in ${CONFIG_FILE}" >&2
                exit 1
            fi
            
            # Construct validator URL
            LOCAL_VALIDATOR_URL="http://${IP}:${TRANSACTION_PORT}"
            LOCAL_VALIDATOR_IP="${IP}"
            LOCAL_VALIDATOR_P2P_PORT="${P2P_PORT}"
            LOCAL_VALIDATOR_TX_PORT="${TRANSACTION_PORT}"
            break
        fi
    fi
done < "${PEERS_FILE}"

if [[ -z "${LOCAL_VALIDATOR_URL}" ]]; then
    echo "Error: Local validator ${PUBLIC_KEY} not found in peers.yaml" >&2
    exit 1
fi

echo "[run-remote] Local validator details:"
echo "  Public Key: ${PUBLIC_KEY}"
echo "  IP: ${LOCAL_VALIDATOR_IP}"
echo "  P2P Port: ${LOCAL_VALIDATOR_P2P_PORT}"
echo "  Transaction Port: ${LOCAL_VALIDATOR_TX_PORT}"
echo "  URL: ${LOCAL_VALIDATOR_URL}"
echo ""
echo "[run-remote] Will submit ${NUM_TXS} transactions to local validator only"
echo "[run-remote] Sender seed: ${SENDER_SEED}"
echo "[run-remote] Receiver: ${RECEIVER_PUBKEY}"
echo ""
echo "[run-remote] Starting transaction submission..."
echo ""

# ============================================================================
# SUBMIT TRANSACTIONS
# ============================================================================

SUCCESS_COUNT=0
FAIL_COUNT=0

for i in $(seq 1 ${NUM_TXS}); do
    AMOUNT=$i
    echo "[run-remote] Submitting transaction ${i}/${NUM_TXS} (amount: ${AMOUNT})"
    
    # Submit to local validator only
    if "${SUBMIT_TX_BINARY}" \
        --validator "${LOCAL_VALIDATOR_URL}" \
        --sender-seed "${SENDER_SEED}" \
        --receiver "${RECEIVER_PUBKEY}" \
        --amount "${AMOUNT}" \
        >/dev/null 2>&1; then
        echo "  ✓ ${PUBLIC_KEY:0:16}... (${LOCAL_VALIDATOR_URL})"
        SUCCESS_COUNT=$((SUCCESS_COUNT + 1))
    else
        echo "  ✗ ${PUBLIC_KEY:0:16}... (${LOCAL_VALIDATOR_URL}) - FAILED"
        FAIL_COUNT=$((FAIL_COUNT + 1))
    fi
    
    # Small delay between transactions to avoid overwhelming the network
    sleep 0.1
done

echo ""
echo "[run-remote] ========================================="
echo "[run-remote] Transaction submission completed!"
echo "[run-remote] Total transactions: ${NUM_TXS}"
echo "[run-remote] Successful submissions: ${SUCCESS_COUNT}"
if [[ ${FAIL_COUNT} -gt 0 ]]; then
    echo "[run-remote] Failed submissions: ${FAIL_COUNT}" >&2
    exit 1
fi

