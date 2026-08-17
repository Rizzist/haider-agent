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
# Deterministic device for fixed-width render pins: CI hostnames run 60+
# chars and shed row segments host-dependently.
export HAIDER_TEST_DEVICE_NAME=test-mac

crates="haider-platform haider-protocol haider-accounts haider-core haider-pdf \
haider-provider haider-daemon haider-daemond haider-rpc haider-tui haider-cli \
haider-store haider-tools haider-client haider-verify"

# Compile phase first, uncapped — compilation cannot deadlock, and folding it
# out lets the per-crate EXECUTION cap below be tight. A hanging test then
# fails its crate in minutes with the crate named, instead of burning the
# 6-hour job timeout with no attribution (the first Windows test run hung
# for hours exactly this way).
echo "::group::compile all test binaries"
cargo test --workspace --no-run --locked || exit 1
echo "::endgroup::"
# Subprocess-based CLI tests may now trust the sibling next to the freshly
# compiled CLI without recursively entering Cargo from a running test binary.
export HAIDER_TEST_SIBLINGS_PREBUILT=1

# Per-crate execution cap (15 min — generous for RUNNING tests).
T="$(command -v timeout || command -v gtimeout || true)"

fail=0
for crate in $crates; do
  echo "::group::$crate"
  case "$crate" in
    haider-daemon|haider-daemond)
      ${T:+"$T" 900} cargo test -p "$crate" --locked -- --test-threads=4 || { echo "FAIL: $crate"; fail=$((fail+1)); }
      ;;
    *)
      ${T:+"$T" 900} cargo test -p "$crate" --locked || { echo "FAIL: $crate"; fail=$((fail+1)); }
      ;;
  esac
  echo "::endgroup::"
done
echo "=== per-crate tests done (fail=$fail) ==="
exit "$fail"
