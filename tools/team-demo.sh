#!/usr/bin/env bash
# Synthetic, loopback-only showcase. Build brain first; no implicit downloads/builds.
set -euo pipefail
exec python3 "$(dirname "${BASH_SOURCE[0]}")/team-demo.py" "$@"
