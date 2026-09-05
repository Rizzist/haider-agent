#!/bin/zsh
set -eu
cd /Users/rizzist/haider-run/lane-970-journalview
export RUST_MIN_STACK=8388608 HAIDER_DISCOVERY_DISABLED=1 HAIDER_TEST_DEVICE_NAME=test-mac CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 CARGO_BUILD_JOBS=2 HAIDER_TEST_SIBLINGS_PREBUILT=1
if [[ $(cat /tmp/journalview-cont-merged-ref) != $(git rev-parse origin/wave-970) ]]; then
  print MERGE-FORWARD-REQUIRED
  exit 98
fi
gate_result=0
lane_step() {
  local lane_name=$1
  shift
  local lane_rc=0
  "$@" > "/tmp/journalview-final-${lane_name}.log" 2>&1 || lane_rc=$?
  print "${lane_name}: exit=${lane_rc}"
  if (( lane_rc != 0 )); then gate_result=1; fi
}
lane_build() {
  df -m /
  local journalview_free_mib=$(df -Pm / | awk 'NR==2 {print $4}')
  if (( journalview_free_mib < 700 )); then print ENVIRONMENT-BLOCKED; exit 99; fi
  lane_step "$@"
}
lane_step fmt cargo fmt --all --check
lane_step diff git diff --check
lane_step unsafe bash scripts/check-unsafe-counts.sh
lane_step qa-selftests bash scripts/qa-gate/run.sh test
lane_build rebind-regression cargo test -q --locked -p haider-core --lib journalview_rebind_failure_closes_recovered_items_under_the_source_request
if (( gate_result != 0 )); then
  print PRECHECK-FAILED
  exit $gate_result
fi
lane_build workspace cargo test -q --workspace --locked --no-fail-fast
lane_build clippy cargo clippy --workspace --tests --locked -- -D warnings
lane_build xtask cargo run -q --locked -p xtask -- check
if [[ $(cat /tmp/journalview-cont-merged-ref) != $(git rev-parse origin/wave-970) ]]; then
  print MERGE-FORWARD-REQUIRED
  exit 98
fi
print "FINAL-GATE-RESULT=${gate_result}"
exit $gate_result
