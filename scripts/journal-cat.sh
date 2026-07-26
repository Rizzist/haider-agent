#!/bin/bash
# Merge per-run supervision journals in timestamp order.
set -u

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd) || exit 1
JOURNAL_DIR=${1:-"$SCRIPT_DIR/run-journal"}

if [ "$#" -gt 1 ]; then
    echo "Usage: journal-cat.sh [journal-directory]" >&2
    exit 2
fi
if [ ! -d "$JOURNAL_DIR" ]; then
    echo "journal-cat.sh: journal directory not found: $JOURNAL_DIR" >&2
    exit 1
fi

set -- "$JOURNAL_DIR"/*.jsonl
[ -e "$1" ] || exit 0
LC_ALL=C sort -s -t '"' -k4,4 "$@"
