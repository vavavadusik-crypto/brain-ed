#!/usr/bin/env bash
# Synthetic admin + manager + workers using invite/accept on loopback TLS.
# Build brain first; no implicit downloads/builds. --via-relay retains the legacy demo.
set -euo pipefail
exec python3 "$(dirname "${BASH_SOURCE[0]}")/team-demo.py" "$@"
