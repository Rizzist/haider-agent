import os, subprocess, sys, time
from pathlib import Path
E = Path(__file__).resolve().parent
env = dict(os.environ, RUST_MIN_STACK="8388608", HAIDER_DISCOVERY_DISABLED="1", HAIDER_TEST_DEVICE_NAME="test-mac", CARGO_INCREMENTAL="0", CARGO_PROFILE_DEV_DEBUG="0", HAIDER_TEST_SIBLINGS_PREBUILT="1", CARGO_BUILD_JOBS="2", RUST_TEST_THREADS="4")
def run(name, command, extra=None):
    disk = subprocess.check_output(["df", "-m", "/"], text=True)
    (E / ("disk-" + name + ".log")).write_text(disk)
    if int(disk.splitlines()[1].split()[3]) < 700:
        print("ENVIRONMENT-BLOCKED: disk below 700 MiB", flush=True)
        sys.exit(90)
    print(name + ": " + command, flush=True)
    started = time.time()
    with (E / (name + ".log")).open("w") as log:
        result = subprocess.run(command, shell=True, executable="/bin/zsh", env=env | (extra or {}), stdout=log, stderr=subprocess.STDOUT)
    (E / (name + ".exit")).write_text(str(result.returncode) + "\n")
    print(f"{name}: exit={result.returncode} elapsed={time.time()-started:.1f}s", flush=True)
    print("\n".join((E / (name + ".log")).read_text().splitlines()[-8:]), flush=True)
    if result.returncode:
        sys.exit(result.returncode)
if sys.argv[1] == "fixtures":
    run("golden-oneshot", "cargo test -q -p haider-cli --test oneshot_boot_tests one_shot_jsonl_stream_matches_the_normalized_golden -- --exact --nocapture", {"HAIDER_ONESHOT_GOLDEN_UPDATE":"1"})
    run("golden-turnhygiene", "cargo test -q -p haider-cli --test turnhygiene_pin_tests run_jsonl_ -- --nocapture", {"UPDATE_FIXTURES":"1"})
    run("golden-provider", "cargo test -q -p haider-cli --test turnhygiene_pin_tests provider_request_body_is_budget_independent_and_matches_the_golden_ledger -- --exact --nocapture", {"UPDATE_FIXTURES":"1"})
    run("test-count", "cargo run -q -p xtask -- test-count --update")
    run("instruct-pin", "cargo test -q -p haider-daemon --lib permissions_core_tests::instruct_pipe_shrinks_the_advertised_wire_pack -- --exact --nocapture")
    run("handshake-pin", "cargo test -q -p haider-daemon --lib welcome_features_pin_served_management_families -- --nocapture")
    run("cap-before-refresh", "cargo test -q -p haider-core --test runtime_tests request_budget_laws::hard_request_bound_preserves_typed_terminal_before_provider_rebind_refresh -- --exact --nocapture")
elif sys.argv[1] == "gates":
    handshake = (E / "handshake-pin.log").read_text()
    assert "handshake pin: served_features=115" in handshake
    assert "1 passed; 0 failed" in handshake
    run("fmt", "cargo fmt --all -- --check")
    run("workspace-tests", "cargo test -q --workspace --no-fail-fast")
    run("workspace-clippy", "cargo clippy --workspace --tests -- -D warnings")
