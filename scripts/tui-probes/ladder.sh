#!/usr/bin/env bash
# The DEMO probe ladder — the 14 runs that gate every W3c3 milestone.
#
# Usage: scripts/tui-probes/ladder.sh [path-to-haider-binary]
# Default binary: target/release/haider (build it first).
#
# Hostile-caller discipline (TUI5 review §4): the ladder is run with
# NO_COLOR=1 CLICOLOR=0 in the PARENT so probelib's env scrub is itself
# exercised. Any nonzero probe exit fails the ladder.
set -uo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"
bin="${1:-$root/target/release/haider}"

if [[ ! -x "$bin" ]]; then
  echo "ladder: no binary at $bin (cargo build --release -p haider-cli)" >&2
  exit 2
fi

runs=(
  "pty-probe.py 118 36"
  "pty-probe.py 90 10"
  "pty-probe.py 90 7"
  "pty-probe.py 90 5"
  "pty-probe.py 90 1"
  "pty-probe-ml.py 118 36"
  "pty-probe-ml.py 90 10"
  "pty-probe-sub.py 118 36"
  "pty-probe-sub.py 90 10"
  "pty-probe-persist.py 118 36"
  "pty-probe-anim.py 118 36"
  "pty-probe-anim.py 90 10"
  "pty-probe-cursor.py 118 36"
  "pty-probe-cursor.py 90 10"
)

fails=0
for run in "${runs[@]}"; do
  # shellcheck disable=SC2206
  parts=($run)
  script="${parts[0]}"
  cols="${parts[1]}"
  rows="${parts[2]}"
  if NO_COLOR=1 CLICOLOR=0 python3 "$here/$script" "$cols" "$rows" "$bin" >/tmp/ladder-out.txt 2>&1; then
    echo "PASS  $script ${cols}x${rows}"
  else
    echo "FAIL  $script ${cols}x${rows}"
    sed 's/^/      /' /tmp/ladder-out.txt | tail -25
    fails=$((fails + 1))
  fi
done

echo "----"
if [[ $fails -eq 0 ]]; then
  echo "ladder: ${#runs[@]}/${#runs[@]} PASS"
  exit 0
fi
echo "ladder: $fails/${#runs[@]} FAILED"
exit 1
