"""Run one continuation check, preserving its exact command, environment and exit."""
import json
import os
from pathlib import Path
import subprocess
import sys
import time

evidence = Path(__file__).resolve().parent
name, *command = sys.argv[1:]
env = dict(os.environ)
env.update(RUST_MIN_STACK="8388608", HAIDER_DISCOVERY_DISABLED="1",
           HAIDER_TEST_DEVICE_NAME="test-mac", CARGO_INCREMENTAL="0",
           CARGO_PROFILE_DEV_DEBUG="0", CARGO_TARGET_DIR="/private/tmp/haider-economydiet-target",
           CARGO_BUILD_JOBS="2", HAIDER_TEST_SIBLINGS_PREBUILT="1")
disk = subprocess.check_output(["df", "-m", "/"], text=True)
(evidence / f"{name}.disk").write_text(disk)
print(disk, end="", flush=True)
if int(disk.splitlines()[-1].split()[3]) < 700:
    sys.exit("ENVIRONMENT-BLOCKED: under 700 MiB free")
started = time.time()
with (evidence / f"{name}.log").open("w") as log:
    result = subprocess.run(command, env=env, stdout=log, stderr=subprocess.STDOUT)
(evidence / f"{name}.exit").write_text(f"{result.returncode}\n")
(evidence / f"{name}.json").write_text(json.dumps({
    "command": command, "exit": result.returncode,
    "duration_seconds": time.time() - started,
    "environment": {key: env[key] for key in (
        "RUST_MIN_STACK", "HAIDER_DISCOVERY_DISABLED", "HAIDER_TEST_DEVICE_NAME",
        "CARGO_INCREMENTAL", "CARGO_PROFILE_DEV_DEBUG", "CARGO_TARGET_DIR",
        "CARGO_BUILD_JOBS", "HAIDER_TEST_SIBLINGS_PREBUILT")},
}, indent=2) + "\n")
print((evidence / f"{name}.log").read_text()[-6000:], flush=True)
sys.exit(result.returncode)
