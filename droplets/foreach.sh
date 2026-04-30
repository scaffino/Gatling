#!/usr/bin/env bash
set -euo pipefail

# Run an arbitrary command on every droplet (host list from remote-runs/ips.txt).
#
# Examples:
#   ./droplets/foreach.sh -- "cd /root/Gatling && git pull"
#   ./droplets/foreach.sh -j 8 -- "uptime"
#   IPS_FILE=remote-runs/ips.txt ./droplets/foreach.sh -- "hostname; df -h"
#
# Notes:
# - Hosts in the file can be "root@IP" or just "IP" (root@ will be prepended).
# - This script is compatible with macOS's default bash (3.2): no mapfile/readarray.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

IPS_FILE="${IPS_FILE:-${REPO_ROOT}/remote-runs/ips.txt}"
JOBS="${JOBS:-0}"          # 0 => sequential (default)
DRY_RUN="${DRY_RUN:-0}"    # 1 => print what would run
CONNECT_TIMEOUT_S="${CONNECT_TIMEOUT_S:-10}"

SSH_OPTS=(
  -o BatchMode=yes
  -o StrictHostKeyChecking=no
  -o UserKnownHostsFile=/dev/null
  -o LogLevel=ERROR
  -o ServerAliveInterval=5
  -o ServerAliveCountMax=3
)

log() { printf '[droplets/foreach] %s\n' "$*"; }
err() { printf '[droplets/foreach] ERROR: %s\n' "$*" >&2; }
fatal() { err "$@"; exit 1; }

usage() {
  cat <<EOF
Usage: $(basename "$0") [options] -- "<remote command>"

Options:
  -f, --ips-file PATH   Hosts file (default: ${IPS_FILE})
  -j, --jobs N          Max parallel SSH jobs (default: 0 = sequential)
  -n, --dry-run         Print SSH commands without running them
  -t, --connect-timeout Seconds for SSH connect timeout (default: ${CONNECT_TIMEOUT_S})
  -h, --help            Show this help

Env vars:
  IPS_FILE, JOBS, DRY_RUN, CONNECT_TIMEOUT_S

Examples:
  $(basename "$0") -- "cd /root/Gatling && git pull"
  $(basename "$0") -j 10 -- "uname -a"
EOF
}

host_id() {
  local host="$1"
  if [[ "${host}" == *@* ]]; then
    echo "${host##*@}"
  else
    echo "${host}"
  fi
}

load_hosts() {
  local hosts=()
  [[ -f "${IPS_FILE}" ]] || fatal "ips file not found: ${IPS_FILE}"

  while IFS= read -r line || [[ -n "${line}" ]]; do
    line="${line%%#*}"
    line="${line#"${line%%[![:space:]]*}"}"
    line="${line%"${line##*[![:space:]]}"}"
    [[ -z "${line}" ]] && continue

    if [[ ! "${line}" =~ ^[^@[:space:]]+@ ]]; then
      line="root@${line}"
    fi
    hosts+=("${line}")
  done < "${IPS_FILE}"

  [[ ${#hosts[@]} -gt 0 ]] || fatal "no hosts found in ${IPS_FILE}"
  printf '%s\n' "${hosts[@]}"
}

epoch_s() {
  # macOS date supports %s.
  date +%s
}

print_prefixed() {
  local prefix="$1"
  local file="$2"
  local line=""
  while IFS= read -r line || [[ -n "${line}" ]]; do
    printf '[%s] %s\n' "${prefix}" "${line}"
  done < "${file}"
}

run_one() {
  local host="$1"
  local remote_cmd="$2"
  local hid
  hid="$(host_id "${host}")"

  if [[ "${DRY_RUN}" == "1" ]]; then
    printf '[dry-run] ssh %s -o ConnectTimeout=%q %q %q\n' "${SSH_OPTS[*]}" "${CONNECT_TIMEOUT_S}" "${host}" "${remote_cmd}"
    return 0
  fi

  local out
  out="$(mktemp -t droplets-foreach.out.XXXXXX)"

  local start_s end_s dur_s rc
  start_s="$(epoch_s)"
  rc=0
  if ! ssh "${SSH_OPTS[@]}" -o ConnectTimeout="${CONNECT_TIMEOUT_S}" "${host}" "${remote_cmd}" >"${out}" 2>&1; then
    rc=$?
  fi
  end_s="$(epoch_s)"
  dur_s="$((end_s - start_s))"

  print_prefixed "${hid}" "${out}"
  rm -f "${out}" || true

  if [[ "${rc}" -eq 0 ]]; then
    printf '[%s] -- OK (rc=%s, %ss)\n' "${hid}" "${rc}" "${dur_s}"
  else
    printf '[%s] -- FAIL (rc=%s, %ss)\n' "${hid}" "${rc}" "${dur_s}"
  fi

  return "${rc}"
}

run_limited_parallel() {
  local max_jobs="$1"; shift
  local remote_cmd="$1"; shift
  local -a hosts=("$@")
  local -a pids=()      # pid per job
  local -a tmpfiles=()  # output per job
  local -a job_hosts=() # host per job
  local -a ok_hosts=()
  local -a fail_hosts=()

  local h
  for h in "${hosts[@]}"; do
    while [[ ${#pids[@]} -ge ${max_jobs} ]]; do
      local i finished_idx=-1
      for i in "${!pids[@]}"; do
        if ! kill -0 "${pids[i]}" 2>/dev/null; then
          finished_idx="${i}"
          break
        fi
      done

      if [[ "${finished_idx}" -ge 0 ]]; then
        local job_rc=0
        if ! wait "${pids[finished_idx]}"; then job_rc=$?; fi
        cat "${tmpfiles[finished_idx]}" || true
        rm -f "${tmpfiles[finished_idx]}" || true

        if [[ "${job_rc}" -eq 0 ]]; then
          ok_hosts+=("${job_hosts[finished_idx]}")
        else
          fail_hosts+=("${job_hosts[finished_idx]}")
        fi

        unset 'pids[finished_idx]'
        unset 'tmpfiles[finished_idx]'
        unset 'job_hosts[finished_idx]'
        pids=("${pids[@]}")           # compact
        tmpfiles=("${tmpfiles[@]}")   # compact
        job_hosts=("${job_hosts[@]}") # compact
      fi

      sleep 0.1
    done

    local tf
    tf="$(mktemp -t droplets-foreach.XXXXXX)"
    tmpfiles+=("${tf}")

    (run_one "${h}" "${remote_cmd}") >"${tf}" &
    pids+=("$!")
    job_hosts+=("${h}")
  done

  local i
  for i in "${!pids[@]}"; do
    local job_rc=0
    if ! wait "${pids[i]}"; then job_rc=$?; fi
    cat "${tmpfiles[i]}" || true
    rm -f "${tmpfiles[i]}" || true

    if [[ "${job_rc}" -eq 0 ]]; then
      ok_hosts+=("${job_hosts[i]}")
    else
      fail_hosts+=("${job_hosts[i]}")
    fi
  done

  if [[ ${#fail_hosts[@]} -gt 0 ]]; then
    err "Summary: ok=${#ok_hosts[@]} fail=${#fail_hosts[@]} (command did NOT complete successfully on all hosts)"
    err "Failed hosts:"
    local fh
    for fh in "${fail_hosts[@]}"; do
      err "  - ${fh}"
    done
    return 1
  fi

  log "Summary: ok=${#ok_hosts[@]} fail=0"
  return 0
}

main() {
  local remote_cmd=""

  while [[ $# -gt 0 ]]; do
    case "$1" in
      -f|--ips-file)
        shift
        [[ $# -gt 0 ]] || { err "Missing value for --ips-file"; usage; exit 2; }
        IPS_FILE="$1"
        ;;
      -j|--jobs)
        shift
        [[ $# -gt 0 ]] || { err "Missing value for --jobs"; usage; exit 2; }
        JOBS="$1"
        ;;
      -n|--dry-run)
        DRY_RUN=1
        ;;
      -t|--connect-timeout)
        shift
        [[ $# -gt 0 ]] || { err "Missing value for --connect-timeout"; usage; exit 2; }
        CONNECT_TIMEOUT_S="$1"
        ;;
      -h|--help)
        usage
        exit 0
        ;;
      --)
        shift
        remote_cmd="$*"
        break
        ;;
      *)
        err "Unknown argument: $1"
        usage
        exit 2
        ;;
    esac
    shift
  done

  [[ -n "${remote_cmd}" ]] || { usage; exit 2; }
  [[ "${JOBS}" =~ ^[0-9]+$ ]] || fatal "jobs must be an integer, got '${JOBS}'"
  [[ "${CONNECT_TIMEOUT_S}" =~ ^[0-9]+$ ]] || fatal "connect-timeout must be an integer, got '${CONNECT_TIMEOUT_S}'"

  local -a hosts=()
  while IFS= read -r h; do
    [[ -n "${h}" ]] && hosts+=("${h}")
  done < <(load_hosts)

  log "Hosts: ${#hosts[@]} (ips file: ${IPS_FILE})"
  log "Command: ${remote_cmd}"
  log "Connect timeout: ${CONNECT_TIMEOUT_S}s"

  if [[ "${JOBS}" -le 1 ]]; then
    local h
    local rc=0
    for h in "${hosts[@]}"; do
      run_one "${h}" "${remote_cmd}" || rc=1
    done

    if [[ "${rc}" -ne 0 ]]; then
      err "Summary: one or more hosts failed (see -- FAIL lines above)"
    else
      log "Summary: all hosts OK"
    fi
    exit "${rc}"
  else
    run_limited_parallel "${JOBS}" "${remote_cmd}" "${hosts[@]}"
  fi
}

main "$@"
