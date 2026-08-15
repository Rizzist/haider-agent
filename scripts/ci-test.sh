#!/usr/bin/env bash
# Per-crate CI test runner — the CI mirror of the local gate. One
# `cargo test -p` per crate: a single workspace invocation SIGABRTs/flakes
# under load (fd + socket pressure across many tokio test binaries in one
# process tree — the documented haider-client/headless flake family).
#
# HAIDER_DISCOVERY_DISABLED keeps every daemon a test boots hermetic (the
# gate116 lesson: an undisabled daemon adopts the host's real credentials).
# Works under macOS bash 3.2, Linux bash, and Git Bash on Windows.
set -o pipefail
export HAIDER_DISCOVERY_DISABLED=1
export CARGO_INCREMENTAL=0

crates="haider-platform haider-protocol haider-accounts haider-core haider-pdf \
haider-provider haider-daemon haider-daemond haider-rpc haider-tui haider-cli \
haider-store haider-tools haider-client haider-verify"

fail=0
for crate in $crates; do
  echo "::group::$crate"
  case "$crate" in
    haider-daemon|haider-daemond)
      cargo test -p "$crate" --locked -- --test-threads=4 || { echo "FAIL: $crate"; fail=$((fail+1)); }
      ;;
    *)
      cargo test -p "$crate" --locked || { echo "FAIL: $crate"; fail=$((fail+1)); }
      ;;
  esac
  echo "::endgroup::"
done
echo "=== per-crate tests done (fail=$fail) ==="
exit "$fail"
