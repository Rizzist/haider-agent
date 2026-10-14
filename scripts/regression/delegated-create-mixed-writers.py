#!/usr/bin/env python3
"""Exercise candidate -> real schema-33 old Store -> candidate receipt replay.

The old writer is an immutable Git revision, not a candidate fixture with a
legacy-shaped request. Build products stay in the worktree; evidence contains
only source identities, commands, text journals, hashes and genuine exit codes.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import sqlite3
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

BASE = "088e5a24026d48378df7256a26bb868f46da0730"
DISK_FLOOR = 6 * 2**30
DISK_RECOVERY = 8 * 2**30


class DiskFloor(RuntimeError):
    """The caller must release its ticket before retrying (exit 75)."""


def terminate_and_join(process, timeout=10):
    # Each command owns a session. TERM lets the slot wrapper run its release
    # trap; KILL also removes descendants that outlive their immediate parent.
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    try:
        process.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        # A shell defers its TERM trap while waiting for a stubborn child.
        # Kill those owned children first so the wrapper can release its ticket.
        try:
            members = subprocess.check_output(["ps", "-A", "-o", "pid=,pgid="], text=True)
        except (OSError, subprocess.CalledProcessError):
            # Even when process inspection is unavailable, join the owner after
            # the group fallback; a dead owner cannot retain a live ticket.
            members = ""
        for row in members.splitlines():
            pid, pgid = map(int, row.split())
            if pgid == process.pid and pid != process.pid:
                try:
                    if os.getpgid(pid) == process.pid:
                        os.kill(pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
        try:
            process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait()
    finally:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.wait()


class CommandRunner:
    def __init__(self, repo, evidence, identity, *, slot_held=False,
                 disk_probe=None, sleep=time.sleep, clock=time.monotonic,
                 poll_seconds=0.5, recovery_seconds=120, wait_seconds=3600):
        self.evidence = evidence
        self.identity = identity
        self.slot_held = slot_held
        self.disk_probe = disk_probe or (lambda: shutil.disk_usage(repo).free)
        self.sleep = sleep
        self.clock = clock
        self.poll_seconds = poll_seconds
        self.recovery_seconds = recovery_seconds
        self.wait_seconds = wait_seconds
        self.records = {}

    def wait_for_disk(self, log):
        if self.slot_held:
            raise DiskFloor("disk floor: exit 75 to release the outer build-slot; retry outside it")
        deadline = self.clock() + self.wait_seconds
        while self.disk_probe() < DISK_RECOVERY:
            remaining = deadline - self.clock()
            if remaining <= 0:
                raise DiskFloor("disk did not recover to 8 GiB within 60 minutes")
            log.write("Waiting outside build-slot for at least 8 GiB free\n")
            log.flush()
            self.sleep(min(self.recovery_seconds, remaining))

    def run(self, name, command, cwd, environment):
        record = self.records[name] = {"command": command, "attempts": []}
        with (self.evidence / (name + ".log")).open("w") as log:
            log.write(self.identity + f"cwd={cwd} command={command!r}\n")
            try:
                while True:
                    free = self.disk_probe()
                    if free < DISK_FLOOR:
                        self.wait_for_disk(log)
                        continue
                    log.write(f"Starting attempt {len(record['attempts']) + 1}\n")
                    log.flush()
                    attempt = {"minimum_free_bytes": free, "disk_floor": False}
                    record["attempts"].append(attempt)
                    process = None
                    try:
                        process = subprocess.Popen(command, cwd=cwd, env=environment,
                                                   stdout=log, stderr=subprocess.STDOUT,
                                                   start_new_session=True)
                        while process.poll() is None:
                            free = self.disk_probe()
                            attempt["minimum_free_bytes"] = min(attempt["minimum_free_bytes"], free)
                            if free < DISK_FLOOR:
                                attempt["disk_floor"] = True
                                log.write("Disk floor: terminating and joining owned command group\n")
                                log.flush()
                                break
                            self.sleep(self.poll_seconds)
                    finally:
                        if process is not None:
                            terminate_and_join(process)
                            attempt["rc"] = process.returncode
                            record["rc"] = process.returncode
                            log.write(f"Attempt {len(record['attempts'])} rc={process.returncode}\n")
                            log.flush()
                            (self.evidence / (name + ".rc")).write_text(str(process.returncode) + "\n")
                    if attempt["disk_floor"]:
                        self.wait_for_disk(log)
                        continue
                    if process.returncode:
                        raise RuntimeError(f"{name} failed rc={process.returncode}")
                    return
            except DiskFloor as error:
                record["runner_rc"] = 75
                log.write(f"Runner rc=75: {error}\n")
                raise
            finally:
                (self.evidence / (name + "-attempts.json")).write_text(json.dumps(record, indent=2) + "\n")


def interrupted(signum, _frame):
    # A second interrupt must not interrupt joining the first one's children.
    for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        signal.signal(sig, signal.SIG_IGN)
    raise SystemExit(128 + signum)


class DiskRecoveryTests(unittest.TestCase):
    """Real child groups and fake tickets/disks; no Cargo or shared resources."""

    sandbox = None

    def setUp(self):
        repo = Path(__file__).resolve().parents[2]
        self.scratch = tempfile.TemporaryDirectory(prefix=".disk-recovery-self-test-", dir=repo)
        self.addCleanup(self.scratch.cleanup)
        self.root = Path(self.scratch.name)
        self.fake = self.root / "fake.py"
        self.fake.write_text('''import os, signal, subprocess, sys, time
from pathlib import Path
root = Path(sys.argv[1])
count_file = root / "count"
count = int(count_file.read_text()) + 1 if count_file.exists() else 1
count_file.write_text(str(count))
def stop(sig, frame):
    raise SystemExit(128 + sig)
signal.signal(signal.SIGTERM, stop)
child = None
try:
    (root / "ticket").write_text(str(os.getpid()))
    delay = 2 if count == 1 and sys.argv[3] == "hold" else 0.02
    child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(float(__import__('sys').argv[1]))", str(delay)])
    (root / ("child-" + str(count))).write_text(str(child.pid))
    child.wait()
finally:
    if child is not None:
        child.terminate()
        child.wait()
    with (root / "released").open("a") as log:
        log.write(str(count) + "\\n")
    (root / "ticket").unlink()
sys.exit(int(sys.argv[2]))
''')
        killpg = os.killpg

        def no_stop(pid, sig):
            self.assertNotEqual(sig, signal.SIGSTOP, "an owned group must never be suspended")
            return killpg(pid, sig)

        self.signals = patch("os.killpg", side_effect=no_stop)
        self.signals.start()
        self.addCleanup(self.signals.stop)

    def assert_released(self, root):
        self.assertFalse((root / "ticket").exists(), "ticket held during recovery/cleanup")
        for path in root.glob("child-*"):
            with self.assertRaises(ProcessLookupError):
                os.kill(int(path.read_text()), 0)

    def command(self, root, rc=0, hold=False):
        return self.sandboxed([sys.executable, str(self.fake), str(root), str(rc), "hold" if hold else "finish"])

    def sandboxed(self, command):
        return ["sandbox-exec", "-f", str(self.sandbox), *command] if self.sandbox else command

    def runner(self, root, probe, **kwargs):
        return CommandRunner(root, root, "self-test\n", disk_probe=probe,
                             poll_seconds=0.005, **kwargs)

    def test_default_releases_ticket_and_restarts_all_phases(self):
        for phase in ("candidate-build", "old-build", "replay"):
            with self.subTest(phase=phase):
                root = self.root / phase
                root.mkdir()
                waits = []

                def disk():
                    if (root / "count").exists() and (root / "count").read_text() == "1":
                        if (root / "released").exists():
                            self.assert_released(root)
                            return DISK_RECOVERY if waits else DISK_RECOVERY - 1
                        if (root / "child-1").exists():
                            return DISK_FLOOR - 1
                    return DISK_RECOVERY + 1

                def sleep(seconds):
                    if seconds == 120:
                        self.assert_released(root)
                        waits.append(seconds)
                    else:
                        time.sleep(seconds)

                runner = self.runner(root, disk, sleep=sleep)
                with patch.dict(os.environ, {"HAIDER_OWNED_DISK_SUPERVISION": "1"}):
                    runner.run(phase, self.command(root, hold=True), root, os.environ.copy())
                attempts = runner.records[phase]["attempts"]
                self.assertEqual([a["rc"] for a in attempts], [143, 0])
                self.assertEqual([a["disk_floor"] for a in attempts], [True, False])
                self.assertEqual(waits, [120])
                self.assertEqual((root / "released").read_text(), "1\n2\n")
                self.assertIn("Attempt 1 rc=143", (root / (phase + ".log")).read_text())
                self.assert_released(root)

    def test_initial_floor_waits_without_starting_a_ticket(self):
        samples = iter([DISK_FLOOR - 1, DISK_RECOVERY - 1, DISK_RECOVERY])
        waits = []

        def sleep(seconds):
            if seconds == 120:
                self.assertFalse((self.root / "count").exists())
                self.assert_released(self.root)
                waits.append(seconds)
            else:
                time.sleep(seconds)

        runner = self.runner(self.root, lambda: next(samples, DISK_RECOVERY), sleep=sleep)
        runner.run("initial", self.command(self.root), self.root, os.environ.copy())
        self.assertEqual(waits, [120])
        self.assertEqual(len(runner.records["initial"]["attempts"]), 1)
        self.assert_released(self.root)

    def test_slot_held_exits_without_recovery_wait(self):
        for active in (False, True):
            with self.subTest(active=active):
                root = self.root / str(active)
                root.mkdir()

                def disk():
                    return DISK_FLOOR - 1 if not active or (root / "child-1").exists() else DISK_RECOVERY

                def sleep(seconds):
                    self.assertLess(seconds, 120, "wait inside an inherited ticket")
                    time.sleep(seconds)

                runner = self.runner(root, disk, sleep=sleep, slot_held=True)
                with self.assertRaises(DiskFloor):
                    runner.run("held", self.command(root, hold=True), root, os.environ.copy())
                attempts = runner.records["held"]["attempts"]
                self.assertEqual([a["rc"] for a in attempts], [143] if active else [])
                self.assertEqual(runner.records["held"]["runner_rc"], 75)
                self.assert_released(root)

    def test_interruption_joins_children_before_cleanup(self):
        for error in (KeyboardInterrupt, lambda: SystemExit(143)):
            root = self.root / str(len(list(self.root.iterdir())))
            root.mkdir()

            def sleep(seconds):
                if (root / "child-1").exists():
                    raise error()
                time.sleep(seconds)

            runner = self.runner(root, lambda: DISK_RECOVERY, sleep=sleep)
            with self.assertRaises((KeyboardInterrupt, SystemExit)):
                runner.run("interrupt", self.command(root, hold=True), root, os.environ.copy())
            self.assertEqual(runner.records["interrupt"]["rc"], 143)
            self.assert_released(root)

    def test_timeout_never_starts_a_command(self):
        elapsed = [0]

        def sleep(seconds):
            elapsed[0] += seconds
            self.assert_released(self.root)

        runner = self.runner(self.root, lambda: DISK_FLOOR - 1, sleep=sleep,
                             clock=lambda: elapsed[0], wait_seconds=3600)
        with self.assertRaises(DiskFloor):
            runner.run("timeout", self.command(self.root), self.root, os.environ.copy())
        self.assertEqual(elapsed[0], 3600)
        self.assertFalse((self.root / "count").exists())

    def test_success_and_failure_at_floor_do_not_retry(self):
        for rc in (0, 42):
            with self.subTest(rc=rc):
                root = self.root / str(rc)
                root.mkdir()
                runner = self.runner(root, lambda: DISK_FLOOR)
                if rc:
                    with self.assertRaisesRegex(RuntimeError, "failed rc=42"):
                        runner.run("control", self.command(root, rc), root, os.environ.copy())
                else:
                    runner.run("control", self.command(root), root, os.environ.copy())
                self.assertEqual(runner.records["control"]["rc"], rc)
                self.assertEqual(len(runner.records["control"]["attempts"]), 1)
                self.assertEqual((root / "control.rc").read_text().strip(), str(rc))
                self.assert_released(root)

    def test_stubborn_child_dies_before_shell_releases_ticket(self):
        child = self.root / "stubborn.py"
        child.write_text('''import os, signal, sys, time
from pathlib import Path
signal.signal(signal.SIGTERM, signal.SIG_IGN)
Path(sys.argv[1]).write_text(str(os.getpid()))
time.sleep(30)
''')
        ticket = self.root / "ticket"
        pid_file = self.root / "child-1"
        # Like build-slot.sh, this shell's signal trap is deferred by its child.
        command = ["/bin/zsh", "-c", 'trap \'rm -f "$1"; exit 143\' TERM; '
                   'echo $$ > "$1"; "$2" "$3" "$4"',
                   "fake-slot", str(ticket), sys.executable, str(child), str(pid_file)]
        process = subprocess.Popen(self.sandboxed(command), start_new_session=True)
        try:
            deadline = time.monotonic() + 5
            while not pid_file.exists():
                self.assertLess(time.monotonic(), deadline)
                time.sleep(0.005)
            terminate_and_join(process, timeout=0.1)
            self.assertEqual(process.returncode, 143)
            self.assert_released(self.root)
        finally:
            terminate_and_join(process, timeout=0.1)

    def test_unavailable_process_inspection_still_joins_owner(self):
        pid_file = self.root / "child-1"
        command = [sys.executable, "-c",
                   "import os, signal, sys, time; from pathlib import Path; "
                   "signal.signal(signal.SIGTERM, signal.SIG_IGN); "
                   "Path(sys.argv[1]).write_text(str(os.getpid())); time.sleep(30)", str(pid_file)]
        process = subprocess.Popen(self.sandboxed(command), start_new_session=True)
        try:
            deadline = time.monotonic() + 5
            while not pid_file.exists():
                self.assertLess(time.monotonic(), deadline)
                time.sleep(0.005)
            with patch("subprocess.check_output", side_effect=PermissionError("fake unavailable ps")):
                terminate_and_join(process, timeout=0.1)
            self.assertEqual(process.returncode, -signal.SIGKILL)
            self.assert_released(self.root)
        finally:
            terminate_and_join(process, timeout=0.1)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true", help="exercise disk recovery with fake commands, no builds")
    parser.add_argument("--self-test-sandbox", type=Path, help="sandbox fake children; keep the cleanup controller outside")
    parser.add_argument("--evidence", type=Path)
    parser.add_argument("--build-slot", type=Path)
    parser.add_argument("--sandbox", type=Path)
    parser.add_argument("--candidate-target", type=Path)
    parser.add_argument("--slot-held", action="store_true",
                        help="use an outer build-slot; exit 75 on low disk, never wait inside it")
    args = parser.parse_args()
    if args.self_test:
        DiskRecoveryTests.sandbox = args.self_test_sandbox
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(DiskRecoveryTests)
        return 0 if unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful() else 1
    for option in ("evidence", "build_slot", "sandbox"):
        if getattr(args, option) is None:
            parser.error("--" + option.replace("_", "-") + " is required")
    for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        signal.signal(sig, interrupted)
    repo = Path(__file__).resolve().parents[2]
    evidence = args.evidence.resolve()
    evidence.mkdir(parents=True, exist_ok=True)
    work = repo / "target/delegated-create-mixed-writers"
    work.mkdir(parents=True, exist_ok=False)
    try:
        run_regression(args, repo, evidence, work)
    finally:
        shutil.rmtree(work)


def run_regression(args, repo, evidence, work):
    sha, tree = subprocess.check_output(
        ["git", "rev-parse", "HEAD", "HEAD^{tree}"], cwd=repo, text=True
    ).splitlines()
    base_tree = subprocess.check_output(
        ["git", "rev-parse", BASE + "^{tree}"], cwd=repo, text=True
    ).strip()
    identity = f"candidate={sha} tree={tree} old={BASE} old_tree={base_tree}\n"
    (evidence / "identity.json").write_text(json.dumps({"candidate": sha, "tree": tree, "old": BASE, "old_tree": base_tree}, indent=2) + "\n")
    env = os.environ.copy()
    env.update(CARGO_PROFILE_DEV_DEBUG="0", CARGO_PROFILE_TEST_DEBUG="0",
               HAIDER_NO_SCCACHE="1", HAIDER_DISCOVERY_DISABLED="1",
               HAIDER_NO_UPDATE_CHECK="1")
    env.pop("RUSTC_WRAPPER", None)
    runtime_env = env.copy()
    for key in ["HOME", "USERPROFILE", "XDG_CONFIG_HOME", "XDG_DATA_HOME",
                "XDG_CACHE_HOME", "XDG_RUNTIME_DIR", "HAIDER_PROFILE_DIR", "HAIDER_RUNTIME_DIR"]:
        path = work / key.lower()
        path.mkdir(exist_ok=True)
        runtime_env[key] = str(path)
    runner = CommandRunner(repo, evidence, identity, slot_held=args.slot_held)
    records = runner.records

    def journal_snapshot(stage, before_preparation=None):
        journals = []
        for db in sorted((work / "profiles").glob("*/store.sqlite")):
            with sqlite3.connect(f"file:{db}?mode=ro", uri=True) as connection:
                rows = connection.execute(
                    "SELECT rowid, session_id, seq, payload_kind, envelope_json FROM events ORDER BY rowid"
                ).fetchall()
                metadata = connection.execute("SELECT id, meta_json FROM sessions ORDER BY id").fetchall()
            journals.append({"case": db.parent.name, "events": rows, "metadata": metadata})
        serialized = json.dumps(journals, indent=2, default=lambda v: {"sqlite_blob_hex": v.hex()})
        (evidence / (stage + "-journals.json")).write_text(serialized + "\n")
        if before_preparation is not None:
            previous = json.loads(before_preparation)
            current = json.loads(serialized)
            assert {row["case"]: row["events"] for row in previous} == {
                row["case"]: row["events"] for row in current
            }, "metadata preparation changed the journal"
            projections = [
                {"case": old["case"], "before": old["metadata"], "after": new["metadata"]}
                for old, new in zip(previous, current) if old["metadata"] != new["metadata"]
            ]
            (evidence / (stage + "-metadata-projections.json")).write_text(json.dumps(projections, indent=2) + "\n")
        return serialized

    def run(name, command, cwd=repo, environment=env):
        runner.run(name, command, cwd, environment)

    binaries = {}
    try:
        old_source = work / "old-source"
        old_source.mkdir()
        # One owned group includes both archive producer and extraction child.
        run("old-source", ["/bin/bash", "-o", "pipefail", "-c", 'git archive "$1" | tar -x -C "$2"',
                           "archive", BASE, str(old_source)])
        for kind, source in [("candidate", repo), ("old", old_source)]:
            target = (args.candidate_target.resolve() if kind == "candidate" and args.candidate_target
                      else work / (kind + "-target"))
            if not target.is_relative_to(repo):
                raise ValueError("target directories must be inside this worktree")
            build_env = env | {"CARGO_TARGET_DIR": str(target)}
            slot = [] if args.slot_held else [str(args.build_slot), "mixed-writer-" + kind, "--"]
            run(kind + "-build", [*slot, "sandbox-exec", "-f", str(args.sandbox), "cargo", "build", "--locked",
                "-p", "haider-store", "--lib"], source, build_env)
            deps = target / "debug/deps"
            binary = work / kind
            command = ["rustc", "--edition=2024", str(Path(__file__).with_suffix(".rs")),
                       "-o", str(binary), "-L", f"dependency={deps}"]
            if kind == "old":
                command += ["--cfg", "old_writer"]
            for name in ["haider_protocol", "serde_json", "haider_store", "blake3"]:
                lib = max(deps.glob(f"lib{name}-*.rlib"), key=lambda p: p.stat().st_mtime)
                command += ["--extern", f"{name}={lib}"]
            run(kind + "-compile", command)
            binaries[kind] = {"path": str(binary), "sha256": hashlib.sha256(binary.read_bytes()).hexdigest()}
            (evidence / (kind + "-binary.sha256")).write_text(binaries[kind]["sha256"] + " " + str(binary) + "\n")
            if target.is_relative_to(work):
                shutil.rmtree(target)
        if binaries["candidate"]["sha256"] == binaries["old"]["sha256"]:
            raise RuntimeError("identical old/candidate binaries")
        before = None
        for stage, kind in [("seed", "candidate"), ("old-write", "old"), ("candidate-open", "candidate"), ("replay", "candidate")]:
            run(stage, ["sandbox-exec", "-f", str(args.sandbox), binaries[kind]["path"],
                        stage, str(work / "profiles")], environment=runtime_env)
            serialized = journal_snapshot(stage, before if stage == "candidate-open" else None)
            if stage in {"old-write", "candidate-open"}:
                before = serialized
            if stage == "replay" and serialized != before:
                raise RuntimeError("replay changed the journals or metadata")
        rows = [json.loads(line) for line in (evidence / "replay.log").read_text().splitlines()
                if line.startswith('{"case":')]
        assert len(rows) == 7 and all(row["pass"] for row in rows), rows
        for stage, kind in [("old-parent-seed", "old"), ("old-parent-clear", "candidate"),
                            ("old-parent-write", "old"), ("old-parent-open", "candidate"), ("old-parent-replay", "candidate")]:
            run(stage, ["sandbox-exec", "-f", str(args.sandbox), binaries[kind]["path"],
                        stage, str(work / "profiles")], environment=runtime_env)
            serialized = journal_snapshot(stage, before if stage == "old-parent-open" else None)
            if stage in {"old-parent-write", "old-parent-open"}:
                before = serialized
            if stage == "old-parent-replay" and serialized != before:
                raise RuntimeError("old-created parent replay changed the journals or metadata")
        old_parent = [json.loads(line) for line in (evidence / "old-parent-replay.log").read_text().splitlines()
                      if line.startswith('{"case":')]
        assert len(old_parent) == 1 and old_parent[0]["pass"], old_parent
        (evidence / "verdict.json").write_text(json.dumps({
            "candidate": sha, "tree": tree, "old": BASE, "old_tree": base_tree,
            "results": rows, "passed": len(rows), "failed": 0, "binaries": binaries,
            "old_created_parent": old_parent,
            "journals_unchanged_by_replay": True, "metadata_prepared_before_replay": True, "commands": records,
        }, indent=2) + "\n")
        print("Real old-writer regression: 7/7 plus old-created parent, journals unchanged", flush=True)
    finally:
        (evidence / "commands.json").write_text(json.dumps(records, indent=2) + "\n")


if __name__ == "__main__":
    try:
        sys.exit(main())
    except DiskFloor as error:
        print(error, file=sys.stderr)
        sys.exit(75)
