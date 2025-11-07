#!/usr/bin/env bash

set -euo pipefail

# Orchestrates sequential runs of run.sh with:
# - validators: configurable via env var NUM_VALIDATORS (default: 2)
# - instances = 1..MAX_INSTANCES, each repeated RUNS_PER_INSTANCES times


SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="${SCRIPT_DIR}"
RUNS_PER_INSTANCES=1
SLEEP_SECONDS=240
SETTLE_SECONDS=4
NUM_VALIDATORS="${NUM_VALIDATORS:-2}"
MAX_INSTANCES="${MAX_INSTANCES:-10}"

close_terminal_tabs_by_tag() {
  local tag="$1"
  /usr/bin/osascript <<APPLESCRIPT >/dev/null 2>&1 || true
tell application "Terminal"
  repeat with w in windows
    try
      set tabsToClose to {}
      repeat with t in tabs of w
        try
          set tabName to (name of t as text)
          set tabCustom to ""
          try
            set tabCustom to (custom title of t as text)
          end try
          if tabName contains "$tag" or tabCustom contains "$tag" then set end of tabsToClose to t
        end try
      end repeat
      repeat with t in tabsToClose
        try
          close t
        end try
      end repeat
    end try
  end repeat
end tell
APPLESCRIPT
}

close_iterm_tabs_by_tag() {
  local tag="$1"
  /usr/bin/osascript <<APPLESCRIPT >/dev/null 2>&1 || true
tell application "iTerm"
  repeat with w in windows
    repeat with t in tabs of w
      repeat with s in sessions of t
        try
          set sessName to ""
          set sessTitle to ""
          try
            set sessName to (name of s as text)
          end try
          try
            set sessTitle to (title of s as text)
          end try
          if sessName contains "$tag" or sessTitle contains "$tag" then
            close s
          end if
        end try
      end repeat
    end repeat
  end repeat
end tell
APPLESCRIPT
}

close_iterm2_tabs_by_tag() {
  local tag="$1"
  /usr/bin/osascript <<APPLESCRIPT >/dev/null 2>&1 || true
tell application "iTerm2"
  repeat with w in windows
    repeat with t in tabs of w
      repeat with s in sessions of t
        try
          set sessName to ""
          set sessTitle to ""
          try
            set sessName to (name of s as text)
          end try
          try
            set sessTitle to (title of s as text)
          end try
          if sessName contains "$tag" or sessTitle contains "$tag" then
            close s
          end if
        end try
      end repeat
    end repeat
  end repeat
end tell
APPLESCRIPT
}

close_all_by_tag() {
  local tag="$1"
  # Try both apps; ignore failures
  close_iterm2_tabs_by_tag "$tag" || true
  close_iterm_tabs_by_tag "$tag" || true
  close_terminal_tabs_by_tag "$tag" || true
}

kill_validators_and_submitters() {
  local instances="$1"
  local tag="${2:-}"
  echo "[orchestrator] Stopping processes for instances=${instances} tag='${tag}'"

  # Patterns to match validators/submitters reliably
  local patterns=(
    "cargo run --bin validator"
    "validator --"
    "submitTx.sh"
    "submit_tx"
  )
  # Add session tag if available to narrow matches
  if [[ -n "${tag}" ]]; then
    patterns+=("${tag}")
  fi

  # Graceful stop first (SIGINT)
  for pat in "${patterns[@]}"; do
    # pgrep -fl prints "PID CMD..."; extract PID and send SIGINT
    pgrep -fl "$pat" 2>/dev/null | awk '{print $1}' | xargs -r kill -INT 2>/dev/null || true
  done

  # Short grace period
  sleep 2

  # Force kill any leftovers (SIGKILL)
  for pat in "${patterns[@]}"; do
    pgrep -fl "$pat" 2>/dev/null | awk '{print $1}' | xargs -r kill -KILL 2>/dev/null || true
  done
}

kill_fallback_processes() {
  local instances="$1"
  # Best-effort kill of validators and submitters if any remain
  pkill -f "cargo run --bin validator -- .*--consensus-instances ${instances}" || true
  pkill -f "submitTx.sh" || true
  pkill -f "submit_tx" || true
}

for instances in $(seq 1 ${MAX_INSTANCES}); do
  for runIndex in $(seq 1 ${RUNS_PER_INSTANCES}); do
    echo "[orchestrator] Starting run: instances=${instances}, run=${runIndex}"
    # Pre-run cleanup to avoid blocking in setup generate due to leftover processes/locks
    echo "[orchestrator] Pre-clean: stopping any leftover validators/submitters..."
    kill_validators_and_submitters "${instances}" || true
    kill_fallback_processes "${instances}" || true
    # Best-effort close of previously tagged tabs (matches our SESSION_TAG prefix)
    close_all_by_tag "alto_" || true
    sleep 3
    # Capture SESSION_TAG line from run.sh output
    SESSION_TAG=$("${REPO_ROOT}/run.sh" "${NUM_VALIDATORS}" "${instances}" "${runIndex}" --headless \
      | tee >(cat >&2) \
      | sed -n 's/^SESSION_TAG=//p' \
      | tail -1 || true)
    if [[ -z "${SESSION_TAG}" ]]; then
      echo "[orchestrator] WARNING: SESSION_TAG not captured; cleanup may use fallback kill only" >&2
    else
      echo "[orchestrator] SESSION_TAG=${SESSION_TAG}"
    fi

    echo "[orchestrator] Sleeping ${SLEEP_SECONDS}s"
    sleep "${SLEEP_SECONDS}"

    echo "[orchestrator] Stopping validators/submitters..."
    kill_validators_and_submitters "${instances}" "${SESSION_TAG:-}"

    echo "[orchestrator] Closing windows/tabs by tag..."
    if [[ -n "${SESSION_TAG:-}" ]]; then
      close_all_by_tag "${SESSION_TAG}"
    fi

    echo "[orchestrator] Settle ${SETTLE_SECONDS}s"
    sleep "${SETTLE_SECONDS}"
  done
done

echo "[orchestrator] Completed all runs."


