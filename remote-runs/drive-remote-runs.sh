#!/usr/bin/env bash

set -euo pipefail

# =============================================================================
# Local driver script to refresh validator configs and trigger remote runs
# =============================================================================
#
# This script runs from your laptop. For each desired (instances, run) pair it:
#   1. Regenerates validator configs locally via setup-droplet.sh.
#   2. Builds per-validator peers.yaml files with remote IPs.
#   3. Copies the fresh configs to each remote validator host.
#   4. Invokes orchestrator-remote.sh on every remote host for that pair.
#
# Update the configuration section below to match your environment. The
# defaults mirror the existing remote scripts in this repository.
#
# Environment variables:
#   MAX_INSTANCES        Override max consensus instances to test.
#   RUNS_PER_INSTANCE    Override number of runs per instance value.
#   SLEEP_SECONDS        Override sleep duration passed to orchestrator.
#   ENABLE_TX_SUBMISSION Forwarded to orchestrator (default: 1).
#   TX_COUNT / TX_WAVES / TX_INTERVAL / TX_START_DELAY forwarded as-is.
#   DRY_RUN              When set to 1, prints remote actions without running.
#   SKIP_SETUP           When set to 1, reuses existing configs (no regenerate).
#
# Requirements:
#   - Passwordless ssh/scp access to each remote host listed below.
#   - Remote repository cloned at ${REMOTE_REPO_DIR}.
#   - remote-runs/orchestrator-remote.sh present and executable on remotes.
#
# =============================================================================

## --- Configuration ---------------------------------------------------------

# Remote validator hosts (order must match setup-droplet.sh configuration)
REMOTE_HOSTS=(
    "root@167.71.84.48"
    "root@188.166.175.132"
    "root@167.71.226.93"
)

# Additional ssh/scp options (edit as needed)
SSH_OPTS=(
    -o BatchMode=yes
    -o StrictHostKeyChecking=no
    -o UserKnownHostsFile=/dev/null
)

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

SETUP_SCRIPT="${SCRIPT_DIR}/setup-droplet.sh"
CONFIG_OUTPUT_DIR="${REPO_ROOT}/chain/test-remote"

REMOTE_REPO_DIR="${REMOTE_REPO_DIR:-/root/alto}"
REMOTE_BASE_DIR_ROOT="${REMOTE_BASE_DIR:-/root/alto/deploy/manual}"
REMOTE_LOG_BASE="${REMOTE_LOG_BASE:-/root/alto/logs/validator}"

MAX_INSTANCES="${MAX_INSTANCES:-10}"
RUNS_PER_INSTANCE="${RUNS_PER_INSTANCE:-1}"
SLEEP_SECONDS="${SLEEP_SECONDS:-600}"

ENABLE_TX_SUBMISSION="${ENABLE_TX_SUBMISSION:-1}"
TX_COUNT="${TX_COUNT:-25}"
TX_WAVES="${TX_WAVES:-4}"
TX_INTERVAL="${TX_INTERVAL:-20}"
TX_START_DELAY="${TX_START_DELAY:-360}"

DRY_RUN="${DRY_RUN:-0}"
SKIP_SETUP="${SKIP_SETUP:-0}"

## --- Internal state --------------------------------------------------------

CONFIG_FILES=()
PUBLIC_KEYS=()
REMOTE_IPS=()
ACTIVE_SSH_PIDS=()

# =============================================================================
# Helper functions
# =============================================================================

log() {
    printf '[drive-remote-runs] %s\n' "$*"
}

err() {
    printf '[drive-remote-runs] ERROR: %s\n' "$*" >&2
}

fatal() {
    err "$@"
    exit 1
}

command_exists() {
    command -v "$1" >/dev/null 2>&1
}

run_setup() {
    if [[ "${SKIP_SETUP}" == "1" ]]; then
        log "SKIP_SETUP=1 – reusing existing configs in ${CONFIG_OUTPUT_DIR}"
        return
    fi

    log "Generating fresh validator configs with setup-droplet.sh"
    if [[ "${DRY_RUN}" == "1" ]]; then
        log "[dry-run] ${SETUP_SCRIPT} (skipped)"
        return
    fi

    SKIP_REMOTE_DEPLOY=1 "${SETUP_SCRIPT}"
}

collect_remote_ips() {
    REMOTE_IPS=()
    local host
    for host in "${REMOTE_HOSTS[@]}"; do
        if [[ "${host}" == *@* ]]; then
            REMOTE_IPS+=("${host##*@}")
        else
            REMOTE_IPS+=("${host}")
        fi
    done
}

collect_configs() {
    CONFIG_FILES=()
    PUBLIC_KEYS=()

    local has_files=0
    while IFS= read -r line; do
        [[ -n "${line}" ]] || continue
        has_files=1
        CONFIG_FILES+=("${line}")
        PUBLIC_KEYS+=("$(basename "${line}" .yaml)")
    done < <(find "${CONFIG_OUTPUT_DIR}" -maxdepth 1 -type f -name "*.yaml" ! -name "peers.yaml" ! -name "config.yaml" -print | LC_ALL=C sort)

    if [[ ${has_files} -eq 0 ]]; then
        fatal "No validator config files found in ${CONFIG_OUTPUT_DIR}"
    fi

    local expected="${#REMOTE_HOSTS[@]}"
    if [[ ${#CONFIG_FILES[@]} -ne ${expected} ]]; then
        fatal "Expected ${expected} validator configs, found ${#CONFIG_FILES[@]}"
    fi
}

generate_modified_peers() {
    local peers_template="$1"
    local current_key="$2"
    local output_file="$3"

    {
        while IFS= read -r line || [[ -n "$line" ]]; do
            if [[ "$line" =~ ^addresses:[[:space:]]*$ ]]; then
                echo "$line"
                continue
            fi

            if [[ -z "$line" ]] || [[ "$line" =~ ^[[:space:]]*$ ]]; then
                echo "$line"
                continue
            fi

            if [[ "$line" =~ ^([[:space:]]+)([^:[:space:]]+):[[:space:]]*(.+)$ ]]; then
                local indent="${BASH_REMATCH[1]}"
                local peer_key="${BASH_REMATCH[2]}"
                local peer_addr="${BASH_REMATCH[3]}"

                if [[ "$peer_addr" =~ ^([^:]+):(.+)$ ]]; then
                    local peer_port="${BASH_REMATCH[2]}"

                    if [[ "${peer_key}" == "${current_key}" ]]; then
                        echo "${indent}${peer_key}: ${peer_addr}"
                    else
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
                    echo "${indent}${peer_key}: ${peer_addr}"
                fi
            else
                echo "$line"
            fi
        done < "${peers_template}"
    } > "${output_file}"
}

remote_cmd() {
    local host="$1"
    shift
    local cmd="$*"
    if [[ "${DRY_RUN}" == "1" ]]; then
        log "[dry-run] ssh ${host} ${cmd}"
        return 0
    fi
    ssh "${SSH_OPTS[@]}" "${host}" "${cmd}"
}

copy_file() {
    local source="$1"
    local host="$2"
    local destination="$3"

    if [[ "${DRY_RUN}" == "1" ]]; then
        log "[dry-run] scp ${source} ${host}:${destination}"
        return 0
    fi

    scp "${SSH_OPTS[@]}" "${source}" "${host}:${destination}"
}

build_sequences() {
    INSTANCE_VALUES=()
    RUN_VALUES=()

    local inst
    for inst in $(seq 1 "${MAX_INSTANCES}"); do
        INSTANCE_VALUES+=("${inst}")
    done

    local run
    for run in $(seq 1 "${RUNS_PER_INSTANCE}"); do
        RUN_VALUES+=("${run}")
    done
}

prepare_remote_dirs() {
    local host="$1"
    local remote_dir="$2"
    local log_dir="$3"
    remote_cmd "${host}" "mkdir -p '${remote_dir}' '${log_dir}'"
}

invoke_remote_orchestrator() {
    local host="$1"
    local public_key="$2"
    local instances="$3"
    local run_idx="$4"
    local remote_dir="$5"
    local log_dir="$6"

    local cmd
    cmd=$(cat <<EOF
cd '${REMOTE_REPO_DIR}' && \
INSTANCE_SEQUENCE='${instances}' RUN_SEQUENCE='${run_idx}' \
BASE_DIR='${remote_dir}' LOG_DIR='${log_dir}' \
ENABLE_TX_SUBMISSION='${ENABLE_TX_SUBMISSION}' \
TX_COUNT='${TX_COUNT}' TX_WAVES='${TX_WAVES}' \
TX_INTERVAL='${TX_INTERVAL}' TX_START_DELAY='${TX_START_DELAY}' \
MAX_INSTANCES='${instances}' RUNS_PER_INSTANCES='1' \
./remote-runs/orchestrator-remote.sh '${public_key}' '${instances}' '1' '${SLEEP_SECONDS}'
EOF
)

    remote_cmd "${host}" "${cmd}"
}

validate_prerequisites() {
    command_exists ssh || fatal "ssh not found"
    command_exists scp || fatal "scp not found"
    [[ -x "${SETUP_SCRIPT}" ]] || fatal "setup script not executable: ${SETUP_SCRIPT}"
    [[ ${#REMOTE_HOSTS[@]} -gt 0 ]] || fatal "REMOTE_HOSTS array is empty"
    mkdir -p "${CONFIG_OUTPUT_DIR}"
}

cleanup_on_interrupt() {
    log "Interrupt received. Stopping remote orchestrators..."
    for pid in "${ACTIVE_SSH_PIDS[@]}"; do
        if [[ -n "${pid}" ]] && kill -0 "${pid}" 2>/dev/null; then
            kill "${pid}" 2>/dev/null || true
        fi
    done
    wait
    log "Cleanup complete."
    exit 130
}

# =============================================================================
# Main orchestration
# =============================================================================

main() {
    validate_prerequisites
    collect_remote_ips
    build_sequences
    trap cleanup_on_interrupt INT TERM

    local peers_template="${CONFIG_OUTPUT_DIR}/peers.yaml"
    local instances run_idx

    for instances in "${INSTANCE_VALUES[@]}"; do
        for run_idx in "${RUN_VALUES[@]}"; do
            local run_label
            run_label="$(date +%Y%m%d-%H%M%S)_i${instances}_r${run_idx}"
            local remote_dir="${REMOTE_BASE_DIR_ROOT}/${run_label}"
            local remote_log_dir="${REMOTE_LOG_BASE}/${run_label}"

            log "================================================================"
            log "Run label: ${run_label}"
            log "  Instances : ${instances}"
            log "  Run index : ${run_idx}"
            log "  Remote dir: ${remote_dir}"
            log "  Remote log: ${remote_log_dir}"

            run_setup

            if [[ ! -f "${peers_template}" ]]; then
                fatal "peers.yaml not found at ${peers_template}"
            fi

            collect_configs

            local temp_dir
            temp_dir=$(mktemp -d)
            trap 'rm -rf "${temp_dir}"' EXIT

            local idx host config_file public_key peers_output
            for idx in "${!REMOTE_HOSTS[@]}"; do
                host="${REMOTE_HOSTS[idx]}"
                config_file="${CONFIG_FILES[idx]}"
                public_key="${PUBLIC_KEYS[idx]}"

                peers_output="${temp_dir}/peers_${public_key}.yaml"
                generate_modified_peers "${peers_template}" "${public_key}" "${peers_output}"

                prepare_remote_dirs "${host}" "${remote_dir}" "${remote_log_dir}"

                copy_file "${config_file}" "${host}" "${remote_dir}/${public_key}.yaml"
                copy_file "${peers_output}" "${host}" "${remote_dir}/peers.yaml"
            done

            log "Starting remote orchestrators..."
            local pids=()
            ACTIVE_SSH_PIDS=()

            for idx in "${!REMOTE_HOSTS[@]}"; do
                host="${REMOTE_HOSTS[idx]}"
                public_key="${PUBLIC_KEYS[idx]}"

                if [[ "${DRY_RUN}" == "1" ]]; then
                    log "[dry-run] would run orchestrator on ${host} for ${public_key}"
                    continue
                fi

                invoke_remote_orchestrator "${host}" "${public_key}" "${instances}" "${run_idx}" "${remote_dir}" "${remote_log_dir}" &
                pid="$!"
                pids+=("${pid}")
                ACTIVE_SSH_PIDS+=("${pid}")
            done

            if [[ "${DRY_RUN}" != "1" ]]; then
                local pid status=0
                for pid in "${pids[@]}"; do
                    if ! wait "${pid}"; then
                        status=1
                    fi
                done

                if [[ "${status}" -ne 0 ]]; then
                    fatal "One or more remote orchestrations failed for ${run_label}"
                fi
            fi

            ACTIVE_SSH_PIDS=()

            rm -rf "${temp_dir}"
            trap - EXIT

            log "Completed run ${run_label}"
            log ""
        done
    done

    log "All requested runs completed."
    trap - INT TERM
}

main "$@"

