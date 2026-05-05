#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

"${repo_root}/remote-runs/orchestrator-remote.sh" "$@"
"${repo_root}/postprocessing/import-logs.sh"
"${repo_root}/droplets/delete.sh" --force
