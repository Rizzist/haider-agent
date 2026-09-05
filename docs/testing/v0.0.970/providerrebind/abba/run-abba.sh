#!/bin/bash
set -uo pipefail
export RUST_MIN_STACK=8388608 HAIDER_DISCOVERY_DISABLED=1 HAIDER_TEST_DEVICE_NAME=test-mac CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0
mkdir -p /tmp/providerrebind-abba
index=0
for variant in A B B A; do
  index=$((index+1))
  python3 scripts/qa-gate/turn_wall_harness.py --bin-dir "/tmp/providerrebind-release-$variant" --commit-label "$variant-7694ef9c-providerrebind-release" --output "/tmp/providerrebind-abba/$index-$variant.json" > "/tmp/providerrebind-abba/$index-$variant.stdout" 2> "/tmp/providerrebind-abba/$index-$variant.stderr"
  code=$?
  echo "$index-$variant exit=$code"
done
