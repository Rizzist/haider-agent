#!/usr/bin/env bash
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# probelib's refusal law accepts only descendants of Python's temp root. Pin
# that root before Python imports tempfile so every check gets a genuinely
# short macOS UDS path instead of silently falling back outside its sandbox.
if [[ "${OS:-}" != "Windows_NT" ]]; then
  export TMPDIR="${HAIDER_QA_TMPDIR:-/tmp}"
fi

exec python3 "$here/runner.py" "$@"
