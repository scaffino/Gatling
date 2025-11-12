#!/bin/bash
REGIONS=(sgp1) # nyc3 ams3 fra1 tor1 sfo2 blr1 sfo3 syd1 nyc1 nyc2)

# Get SSH key IDs by name
SSH_KEY_1=$(doctl compute ssh-key list --format ID,Name --no-header | grep "giuliascaf" | awk '{print $1}')
SSH_KEY_2=$(doctl compute ssh-key list --format ID,Name --no-header | grep "jneu-key-2025-05-22" | awk '{print $1}')
SSH_KEYS="${SSH_KEY_1},${SSH_KEY_2}"

# Get project ID by name
PROJECT_ID=$(doctl projects list --format ID,Name --no-header | grep "2025-gatling-experiments" | awk '{print $1}')

if [ -z "$PROJECT_ID" ]; then
  echo "Error: Project '2025-gatling-experiments' not found"
  exit 1
fi

# Create a droplet in each region
for region in "${REGIONS[@]}"; do
  echo "Creating droplet in region: $region"
  doctl compute droplet create "gatling-${region}" \
    --region "$region" \
    --size s-2vcpu-2gb \
    --image ubuntu-22-04-x64 \
    --ssh-keys "$SSH_KEYS" \
    --user-data-file setup.yaml \
    --tag-name dev \
    --project-id "$PROJECT_ID" \
    --wait
  echo "✓ Droplet created in $region and assigned to project"
done

echo "All droplets created and assigned successfully!"