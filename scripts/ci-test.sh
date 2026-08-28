#!/usr/bin/env bash
# Per-crate CI test runner — the CI mirror of the local gate. One
# `cargo test -p` per crate: a single workspace invocation SIGABRTs/flakes
# under load (fd + socket pressure across many tokio test binaries in one
# process tree — the documented haider-client/headless flake family).
# `--no-fail-fast` is load-bearing too: Cargo otherwise stops after the first
# failing test binary and hides failures in the crate's remaining binaries.
# nextest was deliberately not substituted because these tests rely on Cargo's
# exact subprocess/working-directory semantics; Cargo's own no-fail-fast mode
# preserves them and continues to run doc-tests.
#
# HAIDER_DISCOVERY_DISABLED keeps every daemon a test boots hermetic (the
# gate116 lesson: an undisabled daemon adopts the host's real credentials).
# Works under macOS bash 3.2, Linux bash, and Git Bash on Windows.
set -o pipefail
export HAIDER_DISCOVERY_DISABLED=1
export CARGO_INCREMENTAL=0
export CARGO_PROFILE_DEV_DEBUG=0
# In-process daemon tests execute the same large, finite async state machines
# that production runs on explicit 8 MiB main/runtime stacks. Preserve any
# non-empty caller override, but do not leave Linux libtest on its 2 MiB
# default (Gate 17's wb_web_runtime_tests stack-overflow mutation pin).
export RUST_MIN_STACK="${RUST_MIN_STACK:-8388608}"
# Deterministic device for fixed-width render pins: CI hostnames run 60+
# chars and shed row segments host-dependently.
export HAIDER_TEST_DEVICE_NAME=test-mac

crates="haider-platform haider-protocol haider-accounts haider-core haider-pdf \
haider-provider haider-daemon haider-daemond haider-rpc haider-tui haider-cli \
haider-store haider-tools haider-client haider-verify haider-stt xtask"

log_dir="${HAIDER_CI_TEST_LOG_DIR:-target/ci-test-logs}"
mkdir -p "$log_dir"
: > "$log_dir/failure-summary.md"

record_failure() {
  local crate="$1"
  local phase="$2"
  local log="$3"
  local clean_log="$log.clean"
  local escape_char
  local details
  local test_name
  local assertion
  local annotation
  escape_char="$(printf '\033')"
  sed "s/${escape_char}\\[[0-9;]*[mK]//g" "$log" > "$clean_log"

  details="$log.details"
  awk '
    function remember(name) {
      if (!(name in seen)) {
        seen[name] = 1
        order[++count] = name
      }
    }
    /^test .* \.\.\. FAILED$/ {
      name = $0
      sub(/^test /, "", name)
      sub(/ \.\.\. FAILED$/, "", name)
      remember(name)
    }
    /^---- .* stdout ----$/ {
      name = $0
      sub(/^---- /, "", name)
      sub(/ stdout ----$/, "", name)
      remember(name)
      current = name
      captured[current] = 0
      next
    }
    current != "" && /^failures:$/ { current = ""; next }
    current != "" && NF && captured[current] < 8 {
      line = $0
      gsub(/\t/, " ", line)
      detail[current] = detail[current] (detail[current] == "" ? "" : " | ") line
      captured[current]++
    }
    END {
      if (count == 0) {
        print "(test process)\tCargo/test process failed before naming a test; see the attached log."
      } else {
        for (i = 1; i <= count; i++) {
          name = order[i]
          message = detail[name]
          if (message == "") {
            message = "Test failed; see the attached log for its assertion output."
          }
          print name "\t" message
        }
      }
    }
  ' "$clean_log" > "$details"

  {
    echo "### \`$crate\` — $phase"
    while IFS="$(printf '\t')" read -r test_name assertion; do
      echo "- \`$test_name\`: $assertion"
    done < "$details"
    echo
  } >> "$log_dir/failure-summary.md"

  while IFS="$(printf '\t')" read -r test_name assertion; do
    annotation="$crate -> $test_name -> $assertion"
    annotation="${annotation//%/%25}"
    annotation="${annotation//$'\r'/%0D}"
    annotation="${annotation//$'\n'/%0A}"
    echo "::error title=Test failure::$annotation"
  done < "$details"
}

run_capped() {
  local log="$1"
  local command_status
  shift
  if [ -n "$T" ]; then
    "$T" 900 "$@" 2>&1 | tee "$log"
  else
    "$@" 2>&1 | tee "$log"
  fi
  command_status=${PIPESTATUS[0]}
  return "$command_status"
}

# Compile phase first, uncapped — compilation cannot deadlock, and folding it
# out lets the per-crate EXECUTION cap below be tight. A hanging test then
# fails its crate in minutes with the crate named, instead of burning the
# 6-hour job timeout with no attribution (the first Windows test run hung
# for hours exactly this way).
echo "::group::compile all test binaries"
compile_log="$log_dir/compile.log"
compile_fail=0
cargo test --workspace --no-run --locked 2>&1 | tee "$compile_log"
compile_status=${PIPESTATUS[0]}
if [ "$compile_status" -ne 0 ]; then
  echo "FAIL: compile all test binaries"
  compile_fail=1
  record_failure "workspace" "compile all test binaries" "$compile_log"
fi
echo "::endgroup::"
# Subprocess-based CLI tests may now trust the sibling next to the freshly
# compiled CLI without recursively entering Cargo from a running test binary.
export HAIDER_TEST_SIBLINGS_PREBUILT=1

# Per-crate execution cap (15 min — generous for RUNNING tests).
T="$(command -v timeout || command -v gtimeout || true)"
windows_runner=false
case "${RUNNER_OS:-}" in
  Windows) windows_runner=true ;;
esac
case "${OS:-}" in
  Windows_NT) windows_runner=true ;;
esac
case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*) windows_runner=true ;;
esac

fail="$compile_fail"
for crate in $crates; do
  echo "::group::$crate"
  crate_log="$log_dir/$crate.log"
  crate_failed=0
  case "$crate" in
    haider-daemon)
      run_capped "$crate_log" cargo test --no-fail-fast -p "$crate" --locked -- --test-threads=4 || crate_failed=1
      ;;
    haider-daemond)
      if [ "$windows_runner" = true ]; then
        # Stream only the Windows process-tree test whose phase diagnostics
        # identify the former hang, then keep capture for every other test.
        # The skip prevents a duplicate run; the in-binary gate still
        # serializes all five real-process tests in ordinary Windows runs.
        streamed_log="$log_dir/$crate-streamed-windows-process-tree.log"
        run_capped "$streamed_log" cargo test --no-fail-fast -p "$crate" --locked --test live_turn_rpc_tests w4a2_cancelled_exec_child_process_group_dies -- --exact --test-threads=1 --nocapture || {
          echo "FAIL: $crate (streamed Windows process-tree test)"
          fail=$((fail+1))
          record_failure "$crate" "streamed Windows process-tree test" "$streamed_log"
        }
        run_capped "$crate_log" cargo test --no-fail-fast -p "$crate" --locked -- --test-threads=4 --skip w4a2_cancelled_exec_child_process_group_dies || crate_failed=1
      else
        run_capped "$crate_log" cargo test --no-fail-fast -p "$crate" --locked -- --test-threads=4 || crate_failed=1
      fi
      ;;
    *)
      run_capped "$crate_log" cargo test --no-fail-fast -p "$crate" --locked || crate_failed=1
      ;;
  esac
  if [ "$crate_failed" -ne 0 ]; then
    echo "FAIL: $crate"
    fail=$((fail+1))
    record_failure "$crate" "tests" "$crate_log"
  fi
  echo "::endgroup::"
done
echo "=== per-crate tests done (fail=$fail) ==="
if [ "$fail" -ne 0 ]; then
  echo "=== complete failure summary ==="
  cat "$log_dir/failure-summary.md"
  if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
    {
      echo "## Complete test failure summary"
      cat "$log_dir/failure-summary.md"
      echo
      echo "Full logs are attached as the \`ci-test-failures-*\` artifact."
    } >> "$GITHUB_STEP_SUMMARY"
  fi
fi
exit "$fail"
