#!/usr/bin/env bash

set -euo pipefail

# Usage:
#   ./run.sh [NUM_VALIDATORS] [NUM_INSTANCES]
#
# - NUM_VALIDATORS is optional (default: 4) and sets how many peers to generate
#   and how many validators to launch.
# - NUM_INSTANCES is optional (default: 2) and sets --consensus-instances.

NUM_VALIDATORS="${1:-4}"
NUM_INSTANCES="${2:-2}"

# Resolve absolute paths
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# If script is under chain/, REPO_ROOT is parent; else REPO_ROOT is SCRIPT_DIR
if [[ "$(basename "${SCRIPT_DIR}")" == "chain" ]]; then
  REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
else
  REPO_ROOT="${SCRIPT_DIR}"
fi

# Fixed output dir under chain/
OUTPUT_DIR="test"
OUTPUT_DIR_ABS="${REPO_ROOT}/chain/${OUTPUT_DIR}"

# Clean previous output
if [[ -d "${OUTPUT_DIR_ABS}" ]]; then
  echo "Removing existing ${OUTPUT_DIR_ABS}..."
  rm -rf "${OUTPUT_DIR_ABS}"
fi

echo "Generating configs for ${NUM_VALIDATORS} peers into ${OUTPUT_DIR}..."
(
  cd "${REPO_ROOT}/chain" && \
  cargo run --bin setup -- generate \
    --peers "${NUM_VALIDATORS}" \
    --bootstrappers 1 \
    --worker-threads 3 \
    --log-level info \
    --message-backlog 16384 \
    --mailbox-size 16384 \
    --deque-size 10 \
    --output "${OUTPUT_DIR}" \
    local \
    --start-port 3000
)

PEERS_FILE="${OUTPUT_DIR_ABS}/peers.yaml"
if [[ ! -f "${PEERS_FILE}" ]]; then
  echo "Error: peers.yaml not found in ${OUTPUT_DIR_ABS}" >&2
  exit 1
fi

# Collect validator config files
VALIDATOR_CONFIGS=()
while IFS= read -r line; do
  [[ -n "$line" ]] && VALIDATOR_CONFIGS+=("$line")
done < <(find "${OUTPUT_DIR_ABS}" -maxdepth 1 -type f -name "*.yaml" \
  ! -name "peers.yaml" \
  ! -name "config.yaml" \
  -print | LC_ALL=C sort)

if [[ ${#VALIDATOR_CONFIGS[@]} -eq 0 ]]; then
  echo "Error: no validator config files found in ${OUTPUT_DIR_ABS}" >&2
  exit 1
fi

# Ensure validator log directory exists under repo root
LOG_ROOT="${REPO_ROOT}/logs"
VALIDATOR_LOGS_DIR="${LOG_ROOT}/validators"
mkdir -p "${VALIDATOR_LOGS_DIR}"

# Take up to NUM_VALIDATORS configs
CMDS=()
MAX="${NUM_VALIDATORS}"
COUNT=0
for CFG in "${VALIDATOR_CONFIGS[@]}"; do
  REL_CFG="${CFG}"
  IDX=$((COUNT+1))
  LOG_FILE="${VALIDATOR_LOGS_DIR}/validator_${IDX}_${NUM_INSTANCES}.log"
  CMD="cd '${REPO_ROOT}/chain' && cargo run --bin validator -- --peers='${PEERS_FILE}' --config='${REL_CFG}' --no-gossip-txs --gatling --consensus-instances ${NUM_INSTANCES}  2>&1 | tee >(sed 's/\\x1b\[[0-9;]*m//g' > '${LOG_FILE}')"
  CMDS+=("${CMD}")
  COUNT=$((COUNT+1))
  [[ ${COUNT} -ge ${MAX} ]] && break
done

if [[ ${#CMDS[@]} -lt 1 ]]; then
  echo "Error: no validator commands generated" >&2
  exit 1
fi

# Escape commands for AppleScript (quotes and backslashes)
ESC_CMDS=()
for _cmd in "${CMDS[@]}"; do
  ESC_CMDS+=("$(printf '%s' "${_cmd}" | sed 's/\\/\\\\/g; s/"/\\"/g')")
done

open_in_terminal() {
  # Open each command in its own Terminal window (no Accessibility/keystroke required)
  local WINDOW_CMDS=""
  for esc in "${ESC_CMDS[@]}"; do
    WINDOW_CMDS+="  do script \"${esc}\""$'\n'
  done

  /usr/bin/osascript <<APPLESCRIPT
 tell application "Terminal"
  activate
${WINDOW_CMDS}end tell
APPLESCRIPT
}

open_in_iterm() {
  local TAB_CMDS=""
  if [[ ${#ESC_CMDS[@]} -ge 2 ]]; then
    for ((i=1; i<${#ESC_CMDS[@]}; i++)); do
      TAB_CMDS+=$'  tell current window\n'
      TAB_CMDS+=$'    create tab with default profile\n'
      TAB_CMDS+=$'    tell current session\n'
      TAB_CMDS+="      write text \"${ESC_CMDS[$i]}\""$'\n'
      TAB_CMDS+=$'    end tell\n'
      TAB_CMDS+=$'  end tell\n'
    done
  fi

  /usr/bin/osascript <<APPLESCRIPT
 tell application "iTerm"
  activate
  set newWindow to (create window with default profile)
  tell current session of newWindow
    write text "${ESC_CMDS[0]}"
  end tell
${TAB_CMDS}end tell
APPLESCRIPT
}

# Prefer iTerm if installed and running; otherwise use Terminal
if /usr/bin/osascript -e 'id of application "iTerm"' >/dev/null 2>&1; then
  open_in_iterm
else
  open_in_terminal
fi

echo "Launched ${#CMDS[@]} validator(s) in new terminal tab(s)."

# Build submit_tx binary if it doesn't exist
if [[ ! -f "${REPO_ROOT}/target/release/submit_tx" ]]; then
  echo "Building submit_tx binary..."
  if (
    cd "${REPO_ROOT}" && \
    cargo build --package alto-client --bin submit_tx --release
  ); then
    if [[ -f "${REPO_ROOT}/target/release/submit_tx" ]]; then
      echo "submit_tx binary built successfully."
    else
      echo "ERROR: Build completed but binary not found at ${REPO_ROOT}/target/release/submit_tx" >&2
      exit 1
    fi
  else
    echo "ERROR: Failed to build submit_tx binary." >&2
    exit 1
  fi
else
  echo "submit_tx binary already exists, skipping build."
fi

# Verify binary exists and is executable before scheduling
if [[ ! -f "${REPO_ROOT}/target/release/submit_tx" ]]; then
  echo "ERROR: submit_tx binary not found at ${REPO_ROOT}/target/release/submit_tx" >&2
  exit 1
fi

if [[ ! -x "${REPO_ROOT}/target/release/submit_tx" ]]; then
  echo "ERROR: submit_tx binary exists but is not executable" >&2
  exit 1
fi

# Schedule submitTx after 3 minutes
mkdir -p "${LOG_ROOT}"
SUBMIT_LOG_FILE="${LOG_ROOT}/submitTx.log"
(
  sleep 120
  echo "[submitTx] Starting after 2 minutes..." | tee -a "${SUBMIT_LOG_FILE}"
  cd "${REPO_ROOT}"
  if [[ -x "./submitTx.sh" ]]; then
    ./submitTx.sh 100 2>&1 | tee -a "${SUBMIT_LOG_FILE}"
  else
    echo "[submitTx] ERROR: submitTx.sh not found or not executable in ${REPO_ROOT}" | tee -a "${SUBMIT_LOG_FILE}"
  fi
) &
echo "Scheduled submitTx.sh to run in 2 minutes (logs: ${SUBMIT_LOG_FILE})."
