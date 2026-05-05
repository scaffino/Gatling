#!/usr/bin/env bash

# Script to strip ANSI escape codes from log files in logs/gatling and logs/validator
# This makes log files readable in text editors and when processing

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

GATLING_DIR="${REPO_ROOT}/logs/gatling"
VALIDATOR_DIR="${REPO_ROOT}/logs/validator"

# ANSI escape code pattern: \x1b[ followed by numbers/semicolons and ending with 'm'
ANSI_PATTERN='s/\x1b\[[0-9;]*m//g'

# Function to strip ANSI codes from a file
# Returns 0 if cleaned, 1 if skipped, 2 on error
strip_ansi_from_file() {
    local file="$1"
    if [[ ! -f "$file" ]]; then
        echo "Warning: File not found: $file" >&2
        return 2
    fi
    
    # Check if file contains ANSI codes (using grep with -a to treat binary as text)
    if grep -aq $'\x1b\[' "$file" 2>/dev/null; then
        # Create temporary file with process ID to avoid conflicts
        local tmp_file="${file}.tmp.$$"
        # Strip ANSI codes and write to temp file
        sed "$ANSI_PATTERN" "$file" > "$tmp_file" 2>/dev/null || {
            echo "Warning: Failed to process $file" >&2
            rm -f "$tmp_file"
            return 2
        }
        # Replace original with cleaned version
        mv "$tmp_file" "$file"
        echo "Cleaned: $file"
        return 0
    else
        echo "Skipped (no ANSI codes): $file"
        return 1
    fi
}

# Process all files in a directory
process_directory() {
    local dir="$1"
    local dir_name="$2"
    
    if [[ ! -d "$dir" ]]; then
        echo "Directory not found: $dir (skipping)"
        return 0
    fi
    
    echo "Processing $dir_name directory: $dir"
    local count=0
    local cleaned=0
    
    # Find all files (not directories) and process them
    while IFS= read -r -d '' file; do
        count=$((count + 1))
        # strip_ansi_from_file returns 0 if cleaned, 1 if skipped, 2 on error
        if strip_ansi_from_file "$file"; then
            cleaned=$((cleaned + 1))
        fi
    done < <(find "$dir" -type f -print0 2>/dev/null || true)
    
    echo "Processed $count file(s) in $dir_name, cleaned $cleaned file(s)"
    echo ""
}

# Main execution
main() {
    echo "=========================================="
    echo "ANSI Code Stripper for Log Files"
    echo "=========================================="
    echo ""
    
    process_directory "$GATLING_DIR" "gatling"
    process_directory "$VALIDATOR_DIR" "validator"
    
    echo "=========================================="
    echo "Done!"
    echo "=========================================="
}

main "$@"

