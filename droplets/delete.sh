#!/bin/bash

# Script to delete all DigitalOcean droplets
# Usage: ./manage.sh [--force] to skip confirmation

set -e

FORCE=false
if [[ "$1" == "--force" ]]; then
    FORCE=true
fi

# List all droplets
echo "Fetching list of droplets..."
DROPLETS=$(doctl compute droplet list --format ID,Name --no-header)

if [ -z "$DROPLETS" ]; then
    echo "No droplets found."
    exit 0
fi

echo ""
echo "Found the following droplets:"
echo "$DROPLETS" | while read -r line; do
    if [ -n "$line" ]; then
        echo "  - $line"
    fi
done

# Count droplets
DROPLET_COUNT=$(echo "$DROPLETS" | grep -v '^$' | wc -l | tr -d ' ')
echo ""
echo "Total droplets: $DROPLET_COUNT"

# Confirm deletion
if [ "$FORCE" = false ]; then
    echo ""
    read -p "Are you sure you want to delete ALL $DROPLET_COUNT droplet(s)? (yes/no): " -r
    echo
    if [[ ! $REPLY =~ ^[Yy][Ee][Ss]$ ]]; then
        echo "Deletion cancelled."
        exit 0
    fi
fi

# Delete each droplet
echo ""
echo "Deleting droplets..."
echo "$DROPLETS" | while read -r line; do
    if [ -n "$line" ]; then
        DROPLET_ID=$(echo "$line" | awk '{print $1}')
        DROPLET_NAME=$(echo "$line" | awk '{print $2}')
        
        if [ -n "$DROPLET_ID" ]; then
            echo "Deleting droplet: $DROPLET_NAME (ID: $DROPLET_ID)..."
            if doctl compute droplet delete "$DROPLET_ID" --force; then
                echo "  ✓ Successfully deleted $DROPLET_NAME"
            else
                echo "  ✗ Failed to delete $DROPLET_NAME"
            fi
        fi
    fi
done

echo ""
echo "Done!"

