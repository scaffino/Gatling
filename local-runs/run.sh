#!/usr/bin/env bash

set -euo pipefail

# Usage:
#   ./run.sh [NUM_VALIDATORS] [NUM_INSTANCES] [RUN_INDEX] [--headless]
#
# - NUM_VALIDATORS is optional (default: 4) and sets how many peers to generate
#   and how many validators to launch.
# - NUM_INSTANCES is optional (default: 2) and sets --consensus-instances.

NUM_VALIDATORS="${1:-4}"
NUM_INSTANCES="${2:-2}"
RUN_INDEX="${3:-1}"
# Optional 4th arg or env var enables headless mode (no Terminal/iTerm windows)
HEADLESS="${HEADLESS:-0}"
if [[ "${4:-}" == "--headless" ]]; then
  HEADLESS=1
fi

# Resolve absolute paths
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# If script is under chain/ or local-runs/, REPO_ROOT is parent; else REPO_ROOT is SCRIPT_DIR
if [[ "$(basename "${SCRIPT_DIR}")" == "chain" ]] || [[ "$(basename "${SCRIPT_DIR}")" == "local-runs" ]]; then
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

# Session tag used to label tabs/windows for later cleanup by orchestrator
SESSION_TAG="alto_${NUM_VALIDATORS}v_${NUM_INSTANCES}i_${RUN_INDEX}r_$(date +%s)"

# Take up to NUM_VALIDATORS configs
CMDS=()
MAX="${NUM_VALIDATORS}"
COUNT=0
for CFG in "${VALIDATOR_CONFIGS[@]}"; do
  REL_CFG="${CFG}"
  IDX=$((COUNT+1))
  LOG_FILE="${VALIDATOR_LOGS_DIR}/v${IDX}_i${NUM_INSTANCES}_r${RUN_INDEX}.log"
  CMD="printf '\\e]1;${SESSION_TAG}\\a'; printf '\\e]0;${SESSION_TAG}\\a'; ulimit -n 65536 && cd '${REPO_ROOT}/chain' && cargo run --bin validator -- --peers='${PEERS_FILE}' --config='${REL_CFG}' --no-gossip-txs --gatling --consensus-instances ${NUM_INSTANCES}  2>&1 | tee >(sed 's/\\x1b\[[0-9;]*m//g' > '${LOG_FILE}')"
  CMDS+=("${CMD}")
  COUNT=$((COUNT+1))
  [[ ${COUNT} -ge ${MAX} ]] && break
done

if [[ ${#CMDS[@]} -lt 1 ]]; then
  echo "Error: no validator commands generated" >&2
  exit 1
fi

if [[ "${HEADLESS}" == "0" ]]; then
  # Escape commands for AppleScript (quotes and backslashes)
  ESC_CMDS=()
  for _cmd in "${CMDS[@]}"; do
    ESC_CMDS+=("$(printf '%s' "${_cmd}" | sed 's/\\/\\\\/g; s/"/\\"/g')")
  done
fi

open_in_terminal() {
  # Open each command in its own Terminal window (no Accessibility/keystroke required)
  local WINDOW_CMDS=""
  if [[ ${#ESC_CMDS[@]} -ge 1 ]]; then
    # First command: open (new window) and set custom title
    WINDOW_CMDS+="  set firstTab to do script \"${ESC_CMDS[0]}\""$'\n'
    WINDOW_CMDS+="  set custom title of firstTab to \"${SESSION_TAG}\""$'\n'
  fi
  if [[ ${#ESC_CMDS[@]} -ge 2 ]]; then
    # Subsequent commands: open in front window as new tabs and set custom title
    for ((i=1; i<${#ESC_CMDS[@]}; i++)); do
      WINDOW_CMDS+=$'  tell front window\n'
      WINDOW_CMDS+="    set newTab to do script \"${ESC_CMDS[$i]}\""$'\n'
      WINDOW_CMDS+="    set custom title of newTab to \"${SESSION_TAG}\""$'\n'
      WINDOW_CMDS+=$'  end tell\n'
    done
  fi

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
      TAB_CMDS+="      set name to \"${SESSION_TAG}\""$'\n'
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
    set name to "${SESSION_TAG}"
    write text "${ESC_CMDS[0]}"
  end tell
${TAB_CMDS}end tell
APPLESCRIPT
}

if [[ "${HEADLESS}" == "1" ]]; then
  # Headless mode: launch validators as background processes, log to files only
  IDX=0
  for CFG in "${VALIDATOR_CONFIGS[@]}"; do
    IDX=$((IDX+1))
    [[ ${IDX} -gt ${MAX} ]] && break
    LOG_FILE="${VALIDATOR_LOGS_DIR}/v${IDX}_i${NUM_INSTANCES}_r${RUN_INDEX}.log"
    # Build the plain validator command without terminal title escape sequences
    VAL_CMD="ulimit -n 65536 && cd '${REPO_ROOT}/chain' && cargo run --bin validator -- --peers='${PEERS_FILE}' --config='${CFG}' --no-gossip-txs --gatling --consensus-instances ${NUM_INSTANCES} 2>&1 | sed 's/\\x1b\[[0-9;]*m//g' >> '${LOG_FILE}'"
    nohup bash -lc "${VAL_CMD}" >/dev/null 2>&1 &
  done
  echo "Launched ${MAX} validator(s) headlessly. Logs: ${VALIDATOR_LOGS_DIR}"
  echo "SESSION_TAG=${SESSION_TAG}"
else
  # Prefer iTerm if installed and running; otherwise use Terminal
  if /usr/bin/osascript -e 'id of application "iTerm"' >/dev/null 2>&1; then
    open_in_iterm
  else
    open_in_terminal
  fi
  echo "Launched ${#CMDS[@]} validator(s) in new terminal tab(s)."
  echo "SESSION_TAG=${SESSION_TAG}"
fi

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

# Schedule submit transactions at 1.5, 2.0, and 2.5 minutes
mkdir -p "${LOG_ROOT}"
SUBMIT_LOG_FILE="${LOG_ROOT}/submitTx.log"
(
  sleep 100
  echo "[submitTx] Starting 20 tx after 100 seconds..." | tee -a "${SUBMIT_LOG_FILE}"
  cd "${REPO_ROOT}"
  if [[ -x "${SCRIPT_DIR}/submitTx.sh" ]]; then
    "${SCRIPT_DIR}/submitTx.sh" 25 2>&1 | tee -a "${SUBMIT_LOG_FILE}"
  else
    echo "[submitTx] ERROR: submitTx.sh not found or not executable at ${SCRIPT_DIR}/submitTx.sh" | tee -a "${SUBMIT_LOG_FILE}"
  fi
) &
(
  sleep 120
  echo "[submitTx] Starting 100 tx after 150 seconds..." | tee -a "${SUBMIT_LOG_FILE}"
  cd "${REPO_ROOT}"
  if [[ -x "${SCRIPT_DIR}/submitTx.sh" ]]; then
    "${SCRIPT_DIR}/submitTx.sh" 25 2>&1 | tee -a "${SUBMIT_LOG_FILE}"
  else
    echo "[submitTx] ERROR: submitTx.sh not found or not executable at ${SCRIPT_DIR}/submitTx.sh" | tee -a "${SUBMIT_LOG_FILE}"
  fi
) &
(
  sleep 140
  echo "[submitTx] Starting 100 tx after 150 seconds..." | tee -a "${SUBMIT_LOG_FILE}"
  cd "${REPO_ROOT}"
  if [[ -x "${SCRIPT_DIR}/submitTx.sh" ]]; then
    "${SCRIPT_DIR}/submitTx.sh" 25 2>&1 | tee -a "${SUBMIT_LOG_FILE}"
  else
    echo "[submitTx] ERROR: submitTx.sh not found or not executable at ${SCRIPT_DIR}/submitTx.sh" | tee -a "${SUBMIT_LOG_FILE}"
  fi
) &
(
  sleep 160
  echo "[submitTx] Starting 100 tx after 150 seconds..." | tee -a "${SUBMIT_LOG_FILE}"
  cd "${REPO_ROOT}"
  if [[ -x "${SCRIPT_DIR}/submitTx.sh" ]]; then
    "${SCRIPT_DIR}/submitTx.sh" 25 2>&1 | tee -a "${SUBMIT_LOG_FILE}"
  else
    echo "[submitTx] ERROR: submitTx.sh not found or not executable at ${SCRIPT_DIR}/submitTx.sh" | tee -a "${SUBMIT_LOG_FILE}"
  fi
) &
echo "Scheduled submitTx.sh waves at 100s (20), 150s (100). Logs: ${SUBMIT_LOG_FILE}."
