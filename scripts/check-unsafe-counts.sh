#!/usr/bin/env bash
set -euo pipefail

if command -v python3 >/dev/null 2>&1; then
  exec python3 scripts/check-unsafe-counts.py "$@"
fi
exec python scripts/check-unsafe-counts.py "$@"
