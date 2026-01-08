#!/bin/bash
REGIONS=(nyc2 sgp1 ams3 fra1 tor1 sfo3 blr1 syd1 lon1 nyc3)

# Get the directory where this script is located
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Determine path to setup.yaml relative to script location
if [ -f "$SCRIPT_DIR/setup.yaml" ]; then
  # Script is in droplets folder, setup.yaml is in same directory
  SETUP_YAML="$SCRIPT_DIR/setup.yaml"
elif [ -f "$SCRIPT_DIR/droplets/setup.yaml" ]; then
  # Script is in alto folder (or somewhere else), setup.yaml is in droplets subdirectory
  SETUP_YAML="$SCRIPT_DIR/droplets/setup.yaml"
else
  echo "Error: setup.yaml not found. Expected at $SCRIPT_DIR/setup.yaml or $SCRIPT_DIR/droplets/setup.yaml"
  exit 1
fi

# Get SSH key IDs by name
SSH_KEY_1=$(doctl compute ssh-key list --format ID,Name --no-header | grep "mykey" | awk '{print $1}')
SSH_KEY_2=$(doctl compute ssh-key list --format ID,Name --no-header | grep "another-key" | awk '{print $1}')
SSH_KEYS="${SSH_KEY_1},${SSH_KEY_2}"

# Get project ID by name
PROJECT_ID=$(doctl projects list --format ID,Name --no-header | grep "2025-gatling-experiments" | awk '{print $1}')

if [ -z "$PROJECT_ID" ]; then
  echo "Error: Project '2025-gatling-experiments' not found"
  exit 1
fi

# Determine path to remote-runs/ips.txt relative to script location
if [ -d "$SCRIPT_DIR/../remote-runs" ]; then
  # Script is in droplets folder, remote-runs is sibling directory
  IPS_FILE="$SCRIPT_DIR/../remote-runs/ips.txt"
elif [ -d "$SCRIPT_DIR/remote-runs" ]; then
  # Script is in alto folder, remote-runs is subdirectory
  IPS_FILE="$SCRIPT_DIR/remote-runs/ips.txt"
else
  echo "Error: remote-runs directory not found. Expected at $SCRIPT_DIR/../remote-runs or $SCRIPT_DIR/remote-runs"
  exit 1
fi

# Initialize IPs array
declare -a DROPLET_IPS=()

# Create a droplet in each region
for region in "${REGIONS[@]}"; do
  echo "Creating droplet in region: $region"
  doctl compute droplet create "gatling-${region}" \
    --region "$region" \
    --size s-2vcpu-2gb \
    --image ubuntu-22-04-x64 \
    --ssh-keys "$SSH_KEYS" \
    --user-data-file "$SETUP_YAML" \
    --tag-name dev \
    --project-id "$PROJECT_ID" \
    --wait
  
  # Get the IP address of the created droplet
  DROPLET_IP=$(doctl compute droplet list "gatling-${region}" --format PublicIPv4 --no-header | head -n1 | tr -d ' ')
  
  if [ -z "$DROPLET_IP" ]; then
    echo "Warning: Could not retrieve IP address for droplet gatling-${region}"
  else
    DROPLET_IPS+=("$DROPLET_IP")
    echo "✓ Droplet created in $region with IP: $DROPLET_IP"
  fi
done

# Write IPs to file
echo "# Remote host IPs (one per line)" > "$IPS_FILE"
echo "# Format: root@IP or just IP (root@ will be prepended automatically)" >> "$IPS_FILE"
echo "# Lines starting with # are comments and will be ignored" >> "$IPS_FILE"
echo "# Empty lines are ignored" >> "$IPS_FILE"
echo "" >> "$IPS_FILE"
echo "# Droplets created on $(date)" >> "$IPS_FILE"

for ip in "${DROPLET_IPS[@]}"; do
  echo "root@${ip}" >> "$IPS_FILE"
done

echo ""
echo "All droplets created and assigned successfully!"
echo "IP addresses written to: $IPS_FILE"
echo "Total droplets: ${#DROPLET_IPS[@]}"