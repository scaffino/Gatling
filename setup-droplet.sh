#!/usr/bin/env bash

set -euo pipefail

# ============================================================================
# CONFIGURATION
# ============================================================================

# Number of validators to generate
V=2

# Remote validator machine IPs (hardcode these - one IP per validator)
# First IP → first validator, second IP → second validator, etc.
IPS=(
    "167.71.84.48" # gatling-nyc
    "188.166.175.132" # gatling-london
)

# Output directory where setup generates configs (relative to chain/)
OUTPUT_DIR="test-remote"

# Remote base directory where configs will be deployed
REMOTE_BASE_DIR="/root/alto/deploy/manual"

# ============================================================================
# SCRIPT SETUP
# ============================================================================

# Resolve absolute paths
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# If script is under chain/, REPO_ROOT is parent; else REPO_ROOT is SCRIPT_DIR
if [[ "$(basename "${SCRIPT_DIR}")" == "chain" ]]; then
  REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
else
  REPO_ROOT="${SCRIPT_DIR}"
fi

OUTPUT_DIR_ABS="${REPO_ROOT}/chain/${OUTPUT_DIR}"
PEERS_FILE="${OUTPUT_DIR_ABS}/peers.yaml"

# ============================================================================
# VALIDATION
# ============================================================================

# Validate that number of IPs matches number of validators
if [[ ${#IPS[@]} -ne ${V} ]]; then
    echo "Error: Number of IPs (${#IPS[@]}) does not match number of validators (${V})" >&2
    echo "Please update the IPS array to have exactly ${V} entries" >&2
    exit 1
fi

# ============================================================================
# RUN SETUP EXECUTABLE LOCALLY
# ============================================================================

echo "Cleaning previous output directory (if exists)..."
if [[ -d "${OUTPUT_DIR_ABS}" ]]; then
    echo "Removing existing ${OUTPUT_DIR_ABS}..."
    rm -rf "${OUTPUT_DIR_ABS}"
fi

echo "Generating configs for ${V} validators into ${OUTPUT_DIR}..."
(
    cd "${REPO_ROOT}/chain" && \
    cargo run --bin setup -- generate \
        --peers "${V}" \
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

# Verify peers.yaml was created
if [[ ! -f "${PEERS_FILE}" ]]; then
    echo "Error: peers.yaml not found in ${OUTPUT_DIR_ABS}" >&2
    exit 1
fi

echo "Setup completed successfully. peers.yaml created at ${PEERS_FILE}"

# ============================================================================
# COLLECT VALIDATOR PUBLIC KEYS
# ============================================================================

echo "Collecting validator config files..."
VALIDATOR_CONFIGS=()
VALIDATOR_PUBLIC_KEYS=()

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

if [[ ${#VALIDATOR_CONFIGS[@]} -ne ${V} ]]; then
    echo "Error: Expected ${V} validator config files, found ${#VALIDATOR_CONFIGS[@]}" >&2
    exit 1
fi

# Extract public keys from filenames
for config_file in "${VALIDATOR_CONFIGS[@]}"; do
    public_key="$(basename "${config_file}" .yaml)"
    VALIDATOR_PUBLIC_KEYS+=("${public_key}")
done

echo "Found ${#VALIDATOR_PUBLIC_KEYS[@]} validators:"
for i in "${!VALIDATOR_PUBLIC_KEYS[@]}"; do
    echo "  Validator $((i+1)): ${VALIDATOR_PUBLIC_KEYS[i]} → ${IPS[i]}"
done

# ============================================================================
# CREATE MODIFIED PEERS.YAML FOR EACH VALIDATOR AND DEPLOY
# ============================================================================

# Create a temporary directory for modified peers.yaml files
TEMP_DIR=$(mktemp -d)
trap "rm -rf '${TEMP_DIR}'" EXIT

echo ""
echo "Processing validators and deploying configs..."

for i in "${!VALIDATOR_PUBLIC_KEYS[@]}"; do
    CURRENT_PUBLIC_KEY="${VALIDATOR_PUBLIC_KEYS[i]}"
    CURRENT_IP="${IPS[i]}"
    CURRENT_CONFIG_FILE="${VALIDATOR_CONFIGS[i]}"
    
    echo ""
    echo "Processing validator $((i+1))/${#VALIDATOR_PUBLIC_KEYS[@]}: ${CURRENT_PUBLIC_KEY}"
    echo "  IP: ${CURRENT_IP}"
    echo "  Config file: $(basename "${CURRENT_CONFIG_FILE}")"
    
    # Create modified peers.yaml for this validator
    MODIFIED_PEERS_FILE="${TEMP_DIR}/peers_${CURRENT_PUBLIC_KEY}.yaml"
    
    # Read original peers.yaml and modify it
    # For the current validator: keep entry unchanged (127.0.0.1)
    # For other validators: replace 127.0.0.1 with their remote IP
    {
        # Process the peers.yaml file line by line
        while IFS= read -r line || [[ -n "$line" ]]; do
            # Handle the "addresses:" header line
            if [[ "$line" =~ ^addresses:[[:space:]]*$ ]]; then
                echo "$line"
                continue
            fi
            
            # Skip empty lines
            if [[ -z "$line" ]] || [[ "$line" =~ ^[[:space:]]*$ ]]; then
                echo "$line"
                continue
            fi
            
            # Extract public key and address from line
            # Format: "  <public_key>: 127.0.0.1:<port>" or "  <public_key>: <ip>:<port>"
            if [[ "$line" =~ ^([[:space:]]+)([^:[:space:]]+):[[:space:]]*(.+)$ ]]; then
                indent="${BASH_REMATCH[1]}"
                peer_key="${BASH_REMATCH[2]}"
                peer_addr="${BASH_REMATCH[3]}"
                
                # Extract port from address (format: IP:PORT or 127.0.0.1:PORT)
                if [[ "$peer_addr" =~ ^([^:]+):(.+)$ ]]; then
                    peer_ip_part="${BASH_REMATCH[1]}"
                    peer_port="${BASH_REMATCH[2]}"
                    
                    # Check if this is the current validator
                    if [[ "$peer_key" == "$CURRENT_PUBLIC_KEY" ]]; then
                        # Keep current validator's entry unchanged
                        echo "${indent}${peer_key}: ${peer_addr}"
                    else
                        # Find the IP for this peer key
                        PEER_IP=""
                        for j in "${!VALIDATOR_PUBLIC_KEYS[@]}"; do
                            if [[ "${VALIDATOR_PUBLIC_KEYS[j]}" == "$peer_key" ]]; then
                                PEER_IP="${IPS[j]}"
                                break
                            fi
                        done
                        
                        if [[ -n "$PEER_IP" ]]; then
                            # Replace IP part with remote IP, keep port
                            echo "${indent}${peer_key}: ${PEER_IP}:${peer_port}"
                        else
                            echo "Warning: Could not find IP for peer ${peer_key}, keeping original entry" >&2
                            echo "${indent}${peer_key}: ${peer_addr}"
                        fi
                    fi
                else
                    # Address format not recognized (no port), keep original
                    echo "${indent}${peer_key}: ${peer_addr}"
                fi
            else
                # Line doesn't match expected format, keep as-is
                echo "$line"
            fi
        done < "${PEERS_FILE}"
    } > "${MODIFIED_PEERS_FILE}"
    
    # Verify modified peers.yaml was created
    if [[ ! -f "${MODIFIED_PEERS_FILE}" ]]; then
        echo "Error: Failed to create modified peers.yaml for validator ${CURRENT_PUBLIC_KEY}" >&2
        exit 1
    fi
    
    echo "  Created modified peers.yaml"
    
    # SCP validator config file to remote machine
    echo "  Copying config file to root@${CURRENT_IP}:${REMOTE_BASE_DIR}/..."
    if ! scp "${CURRENT_CONFIG_FILE}" "root@${CURRENT_IP}:${REMOTE_BASE_DIR}/"; then
        echo "Error: Failed to copy config file to ${CURRENT_IP}" >&2
        exit 1
    fi
    
    # SCP modified peers.yaml to remote machine
    echo "  Copying peers.yaml to root@${CURRENT_IP}:${REMOTE_BASE_DIR}/..."
    if ! scp "${MODIFIED_PEERS_FILE}" "root@${CURRENT_IP}:${REMOTE_BASE_DIR}/peers.yaml"; then
        echo "Error: Failed to copy peers.yaml to ${CURRENT_IP}" >&2
        exit 1
    fi
    
    echo "  ✓ Successfully deployed to ${CURRENT_IP}"
done

echo ""
echo "============================================================================"
echo "Deployment completed successfully!"
echo "============================================================================"
echo "Summary:"
echo "  - Generated ${V} validator configurations"
echo "  - Deployed configs to ${#IPS[@]} remote machines"
echo ""
echo "Remote directory: ${REMOTE_BASE_DIR}"
echo "Each remote machine now has:"
echo "  - Its validator config file (<public_key>.yaml)"
echo "  - peers.yaml (with localhost entries replaced with remote IPs)"
echo ""

