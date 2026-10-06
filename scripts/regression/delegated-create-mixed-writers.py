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
import time

BASE = "088e5a24026d48378df7256a26bb868f46da0730"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--evidence", type=Path, required=True)
    parser.add_argument("--build-slot", type=Path, required=True)
    parser.add_argument("--sandbox", type=Path, required=True)
    parser.add_argument("--candidate-target", type=Path)
    parser.add_argument("--slot-held", action="store_true",
                        help="run only as a descendant of an already acquired build-slot")
    args = parser.parse_args()
    repo = Path(__file__).resolve().parents[2]
    evidence = args.evidence.resolve()
    evidence.mkdir(parents=True, exist_ok=True)
    work = repo / "target/delegated-create-mixed-writers"
    work.mkdir(parents=True, exist_ok=False)
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
    records = {}
    supervised = env.get("HAIDER_OWNED_DISK_SUPERVISION") == "1"

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
        # A low shared-disk sample is a hard stop, never a timeout-and-proceed.
        if shutil.disk_usage(repo).free < 6 * 2**30:
            raise RuntimeError("disk floor: less than 6 GiB free")
        with (evidence / (name + ".log")).open("w") as log:
            log.write(identity + f"cwd={cwd} command={command!r}\n")
            log.flush()
            process = subprocess.Popen(command, cwd=cwd, env=environment,
                                       stdout=log, stderr=subprocess.STDOUT,
                                       start_new_session=not supervised)
            paused = False
            pauses = []
            minimum = shutil.disk_usage(repo).free
            while process.poll() is None:
                free = shutil.disk_usage(repo).free
                minimum = min(minimum, free)
                if not supervised and free < 6 * 2**30 and not paused:
                    os.killpg(process.pid, signal.SIGSTOP)
                    paused = True
                    pauses.append({"paused_at": time.time(), "free_bytes": free})
                    log.write("Disk floor: paused owned command group\n")
                    log.flush()
                if paused and free >= 8 * 2**30:
                    os.killpg(process.pid, signal.SIGCONT)
                    paused = False
                    pauses[-1]["resumed_at"] = time.time()
                time.sleep(0.5)
            rc = process.returncode
        (evidence / (name + ".rc")).write_text(str(rc) + "\n")
        records[name] = {"rc": rc, "command": command,
                         "minimum_free_bytes": minimum, "disk_pauses": pauses}
        if rc:
            raise RuntimeError(f"{name} failed rc={rc}")

    old_source = work / "old-source"
    old_source.mkdir()
    archive = subprocess.Popen(["git", "archive", BASE], cwd=repo, stdout=subprocess.PIPE)
    subprocess.run(["tar", "-x", "-C", str(old_source)], stdin=archive.stdout, check=True)
    archive.stdout.close()
    if archive.wait():
        raise RuntimeError("old source archive failed")
    binaries = {}
    try:
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
        shutil.rmtree(work)


if __name__ == "__main__":
    main()
