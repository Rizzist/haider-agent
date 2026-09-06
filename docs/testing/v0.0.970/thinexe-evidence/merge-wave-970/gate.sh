#!/bin/sh
set -eu
export RUST_MIN_STACK=8388608 HAIDER_DISCOVERY_DISABLED=1 HAIDER_TEST_DEVICE_NAME=test-mac CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 HAIDER_TEST_SIBLINGS_PREBUILT=1 CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/private/tmp/haider-thinexe-target
check_disk() { df -m /; test "$(df -m / | awk 'NR==2 {print $4}')" -ge 700; }
check_disk
cargo run -q -p xtask -- test-count --update
check_disk
cargo build -q -p haider-cli -p haider-daemond -p haider-tui-exe -p haider-tools --bins
stat -f '%N %z bytes' "$CARGO_TARGET_DIR/debug/haider" "$CARGO_TARGET_DIR/debug/haiderd" "$CARGO_TARGET_DIR/debug/haider-tui"
test "$(stat -f %z "$CARGO_TARGET_DIR/debug/haiderd")" -gt 10485760
check_disk
cargo test -q --workspace --no-fail-fast
check_disk
cargo clippy --workspace --tests -- -D warnings
check_disk
cargo test -q -p haider-cli --test payload_routing_tests
check_disk
cargo test -q -p haider-cli --lib routing::tests
