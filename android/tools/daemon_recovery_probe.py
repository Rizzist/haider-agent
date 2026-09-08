#!/usr/bin/env python3
"""Observe recovery without instrumentation's package-wide start/finish teardown."""

import argparse
import json
import subprocess
import time


PACKAGE = "ai.diffforge.haider"
COMPONENT = PACKAGE + "/.daemon.HaiderDaemonService"
STATE_FILE = "no_backup/haider/enabled-state/state-v1.json"
MARKER = "HAIDER_DAEMON_STATE "


class RecoveryProbe:
    def __init__(self, serial, run=subprocess.run):
        self.serial = serial
        self.run = run

    def execute(self, *args):
        return self.run(
            ["adb", "-s", self.serial, "shell", *args],
            check=False, capture_output=True, text=True, timeout=15,
        )

    def shell(self, *args):
        result = self.execute(*args)
        result.check_returncode()
        return result.stdout

    def snapshot(self):
        output = self.shell("dumpsys", "activity", "service", COMPONENT)
        for line in output.splitlines():
            if line.strip().startswith(MARKER):
                return json.loads(line.strip()[len(MARKER):])
        if "No services match:" in output:
            return None  # Absence must not create a service merely to observe it.
        if f"SERVICE {COMPONENT} " in output and "pid=(not running)" in output:
            # Pending is neither Ready nor absence (cleanup must keep waiting).
            return {"phase": None, "frameworkProcessPending": True}
        raise RuntimeError(f"Service diagnostics unavailable: {output}")

    def assert_ready(self, timeout=130):
        deadline = time.monotonic() + timeout
        while True:
            state = self.snapshot()
            if state and state["phase"] == "READY":
                if not (state["enabled"] and state["started"] and state["hasEndpoint"]
                        and state["generation"] > 0 and not state["destroyed"]):
                    raise AssertionError(f"Invalid Ready lifetime: {state}")
                return state
            if time.monotonic() >= deadline:
                raise AssertionError(f"Daemon did not reach Ready: {state}")
            time.sleep(0.1)

    def assert_disabled(self):
        state = json.loads(self.shell("run-as", PACKAGE, "cat", STATE_FILE))
        if state["enabled"] or state["active"] or state.get("updateUntil") is not None:
            raise AssertionError(f"Lifecycle remains enabled/active: {state}")
        return state

    def cleanup(self):
        # Only the service component owned by this fixture. No package-wide stop or PID sweep.
        # Like framework destruction, this retains opt-in; use the app's Stop to disable it.
        result = self.execute("run-as", PACKAGE, "am", "stopservice", "--user", "0", "-n", COMPONENT)
        output = result.stdout + result.stderr
        # Android's am returns -1 (ADB 255) even for these two successful stop outcomes.
        # Preserve that raw status; accept it only with an exact outcome AND observed absence.
        outcomes = {"Service stopped", "Service not stopped: was not running."}
        if result.returncode not in (0, 255) or not outcomes.intersection(output.splitlines()):
            result.check_returncode()
            raise RuntimeError(f"Unrecognized service stop outcome: {output}")
        deadline = time.monotonic() + 5
        while self.snapshot() is not None:
            if time.monotonic() >= deadline:
                raise AssertionError("Owned service still present after cleanup")
            time.sleep(0.1)
        return {"commandExit": result.returncode, "output": output, "serviceAbsent": True}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--serial", required=True, help="Explicit owned emulator/device serial")
    parser.add_argument("action", choices=("snapshot", "assert-ready", "assert-disabled", "cleanup"))
    args = parser.parse_args()
    probe = RecoveryProbe(args.serial)
    result = getattr(probe, args.action.replace("-", "_"))()
    print(json.dumps(result, sort_keys=True))


if __name__ == "__main__":
    main()
