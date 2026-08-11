#!/usr/bin/env bash
# The probe ladder — 14 DEMO runs plus the 2 LIVE runs that gate the swap.
#
# Usage: scripts/tui-probes/ladder.sh [path-to-haider-binary] [path-to-haiderd]
# Defaults: target/release/{haider,haiderd} (build them first).
#
# The live rows boot a REAL haiderd on a throwaway profile with the
# test-only FakeProvider seam — no network, no credentials. They are the
# ONLY gate proving bare `haider` reaches live mode (the v0.0.12 keystone),
# so they are part of the ladder, not a side script. If the haiderd binary
# is absent the ladder REFUSES to report a pass: a green demo ladder must
# never be mistaken for a green swap.
#
# Hostile-caller discipline (TUI5 review §4): the ladder is run with
# NO_COLOR=1 CLICOLOR=0 in the PARENT so probelib's env scrub is itself
# exercised. Any nonzero probe exit fails the ladder.
set -uo pipefail

# Hermetic: the live rows' REAL daemon must never run startup auto-adoption
# (A2) against the host's credential stores — adopted real accounts widen the
# status bar and shed the /voice flash at 90 cols (ladder caught it 1/16).
export HAIDER_DISCOVERY_DISABLED=1

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"
bin="${1:-$root/target/release/haider}"
daemon_bin="${2:-$root/target/release/haiderd}"

if [[ ! -x "$bin" ]]; then
  echo "ladder: no binary at $bin (cargo build --release -p haider-cli)" >&2
  exit 2
fi
if [[ ! -x "$daemon_bin" ]]; then
  echo "ladder: no daemon binary at $daemon_bin (cargo build --release -p haider-daemond)" >&2
  echo "ladder: the live rows are mandatory — refusing to report a demo-only pass" >&2
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

# The live rows take the daemon binary as a fourth argument.
live_runs=(
  "pty-probe-live.py 118 36"
  "pty-probe-live.py 90 10"
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

for run in "${live_runs[@]}"; do
  # shellcheck disable=SC2206
  parts=($run)
  script="${parts[0]}"
  cols="${parts[1]}"
  rows="${parts[2]}"
  if NO_COLOR=1 CLICOLOR=0 python3 "$here/$script" "$cols" "$rows" "$bin" "$daemon_bin" \
      >/tmp/ladder-out.txt 2>&1; then
    echo "PASS  $script ${cols}x${rows} (live)"
  else
    echo "FAIL  $script ${cols}x${rows} (live)"
    sed 's/^/      /' /tmp/ladder-out.txt | tail -25
    fails=$((fails + 1))
  fi
done

total=$(( ${#runs[@]} + ${#live_runs[@]} ))
echo "----"
if [[ $fails -eq 0 ]]; then
  echo "ladder: $total/$total PASS (${#runs[@]} demo + ${#live_runs[@]} live)"
  exit 0
fi
echo "ladder: $fails/$total FAILED"
exit 1
