#!/usr/bin/env bash
# Export committed source only. Never publish, download, or modify the source tree.
set -euo pipefail
if [[ $# != 1 ]]; then
  echo 'Usage: tools/export-public.sh OUT_DIR' >&2
  exit 2
fi
exec python3 "$(dirname "$0")/export-public.py" "$1"
