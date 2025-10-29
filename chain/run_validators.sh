#!/usr/bin/env bash

set -euo pipefail

# Usage:
#   ./run_validators.sh [OUTPUT_DIR]
#
# OUTPUT_DIR is the directory created by `setup generate ... --output <OUTPUT_DIR>`
# It must contain `peers.yaml` and one or more `<public_key>.yaml` validator configs.

OUTPUT_DIR="${1:-test}"

# Resolve absolute paths
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
OUTPUT_DIR_ABS="${REPO_ROOT}/${OUTPUT_DIR}"

if [[ ! -d "${OUTPUT_DIR_ABS}" ]]; then
  echo "Error: output dir does not exist: ${OUTPUT_DIR_ABS}" >&2
  exit 1
fi

PEERS_FILE="${OUTPUT_DIR_ABS}/peers.yaml"
if [[ ! -f "${PEERS_FILE}" ]]; then
  echo "Error: peers.yaml not found in ${OUTPUT_DIR_ABS}" >&2
  exit 1
fi

# Collect validator config files (exclude non-validator YAMLs) – portable across macOS bash
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

# Take up to the first 4 configs
CMDS=()
MAX=4
COUNT=0
for CFG in "${VALIDATOR_CONFIGS[@]}"; do
  REL_CFG="${CFG}"
  CMD="cd '${REPO_ROOT}/chain' && cargo run --bin validator -- --peers='${PEERS_FILE}' --config='${REL_CFG}' --gatling --consensus-instances 5"
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


