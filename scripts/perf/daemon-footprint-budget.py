#!/usr/bin/env python3
"""Measure and enforce macOS daemon settled-footprint and live-window budgets.

The workload can compact one long-lived session every N turns or leave a fleet
of idle sessions resident in one daemon. Measurements use proc_pid_rusage's
physical footprint and CPU counters, avoiding `ps` sampling races.
"""

from __future__ import annotations

import argparse
import ctypes
import json
import os
from pathlib import Path
import sqlite3
import statistics
import subprocess
import sys
import tempfile
import time
from typing import Any


DEFAULT_RUNS = 5
DEFAULT_TURNS = 40
DEFAULT_SETTLE_SECONDS = 60
DEFAULT_COMPACTION_SETTLE_SECONDS = 5
DEFAULT_FLEET_BUDGET_BYTES = 250 * 1024 * 1024
MAX_LOAD_1M = 4.0

# Calibrated at 1.10x the N=5 final release medians (60 s settled, load1m < 4).
# Both are upper bounds, so lower footprints always pass.
DEFAULT_IDLE_BUDGET_BYTES = 6_020_010
DEFAULT_POST_TURNS_BUDGET_BYTES = 18_167_160


class RusageInfoV0(ctypes.Structure):
    _fields_ = [
        ("ri_uuid", ctypes.c_uint8 * 16),
        ("ri_user_time", ctypes.c_uint64),
        ("ri_system_time", ctypes.c_uint64),
        ("ri_pkg_idle_wkups", ctypes.c_uint64),
        ("ri_interrupt_wkups", ctypes.c_uint64),
        ("ri_pageins", ctypes.c_uint64),
        ("ri_wired_size", ctypes.c_uint64),
        ("ri_resident_size", ctypes.c_uint64),
        ("ri_phys_footprint", ctypes.c_uint64),
        ("ri_proc_start_abstime", ctypes.c_uint64),
        ("ri_proc_exit_abstime", ctypes.c_uint64),
    ]


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--daemon", type=Path, required=True)
    parser.add_argument("--driver", type=Path, required=True)
    parser.add_argument("--runs", type=int, default=DEFAULT_RUNS)
    parser.add_argument("--turns", type=int, default=DEFAULT_TURNS)
    parser.add_argument("--compact-every", type=int)
    parser.add_argument(
        "--compaction-settle-seconds",
        type=int,
        default=DEFAULT_COMPACTION_SETTLE_SECONDS,
        help="idle time before each post-compaction footprint checkpoint",
    )
    parser.add_argument(
        "--fleet-sessions",
        type=int,
        default=1,
        help="number of idle sessions to create and drive in the same daemon",
    )
    parser.add_argument("--settle-seconds", type=int, default=DEFAULT_SETTLE_SECONDS)
    parser.add_argument("--attached-settle-seconds", type=int, default=0)
    parser.add_argument(
        "--retention-attribution",
        action="store_true",
        help="sample journal/CAS structure growth after every turn",
    )
    parser.add_argument("--idle-budget-bytes", type=int, default=DEFAULT_IDLE_BUDGET_BYTES)
    parser.add_argument(
        "--post-turns-budget-bytes",
        type=int,
        default=DEFAULT_POST_TURNS_BUDGET_BYTES,
    )
    parser.add_argument(
        "--fleet-budget-bytes", type=int, default=DEFAULT_FLEET_BUDGET_BYTES
    )
    parser.add_argument("--output", type=Path)
    parser.add_argument("--artifacts-dir", type=Path)
    args = parser.parse_args()
    if (
        args.runs < 1
        or args.turns < 1
        or args.fleet_sessions < 1
        or (args.compact_every is not None and args.compact_every < 1)
        or args.settle_seconds < 0
        or args.attached_settle_seconds < 0
        or args.compaction_settle_seconds < 0
    ):
        parser.error(
            "runs, turns, fleet sessions, and compact interval must be positive; "
            "settle times must be non-negative"
        )
    return args


def proc_rusage(pid: int) -> dict[str, int]:
    if sys.platform != "darwin":
        raise RuntimeError("proc_pid_rusage footprint guard requires macOS")
    library = ctypes.CDLL(None, use_errno=True)
    function = library.proc_pid_rusage
    function.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_void_p]
    function.restype = ctypes.c_int
    info = RusageInfoV0()
    if function(pid, 0, ctypes.byref(info)) != 0:
        error = ctypes.get_errno()
        raise OSError(error, os.strerror(error), pid)
    return {
        "user_cpu_ns": int(info.ri_user_time),
        "system_cpu_ns": int(info.ri_system_time),
        "cpu_ns": int(info.ri_user_time + info.ri_system_time),
        "rss_bytes": int(info.ri_resident_size),
        "footprint_bytes": int(info.ri_phys_footprint),
    }


def fake_script(turns: int, sessions: int, compact_every: int | None) -> str:
    steps: list[dict[str, Any]] = []
    for session in range(1, sessions + 1):
        for turn in range(1, turns + 1):
            call_id = f"memdaemon-{session}-{turn}"
            steps.extend(
                [
                    {
                        "step": "emit_tool_call",
                        "call_id": call_id,
                        "name": "process_exec",
                        "args": {"command": ":"},
                    },
                    {"step": "finish", "reason": "tool_use"},
                    {"step": "expect_tool_result", "call_id": call_id},
                    {"step": "finish", "reason": "end_turn"},
                ]
            )
            if compact_every is not None and turn % compact_every == 0:
                steps.extend(
                    [
                        {
                            "step": "emit_text",
                            "text": f"summary through session {session} turn {turn}",
                        },
                        {"step": "finish", "reason": "end_turn"},
                    ]
                )
    return json.dumps(steps, separators=(",", ":"))


def checkpoint(pid: int) -> dict[str, Any]:
    sample = proc_rusage(pid)
    sample["load_1m"] = os.getloadavg()[0]
    sample["monotonic_ns"] = time.monotonic_ns()
    return sample


def capture_process_reports(
    artifacts_dir: Path | None, pid: int, attempt: int, phase: str
) -> dict[str, Any]:
    if artifacts_dir is None:
        return {}
    artifacts_dir.mkdir(parents=True, exist_ok=True)
    reports: dict[str, Any] = {}
    # vmmap is unavailable in the managed runner (registry #44). Keep the
    # optional artifact seam useful without retrying a known-denied command.
    for name, command in (("footprint", ["/usr/bin/footprint", str(pid)]),):
        completed = subprocess.run(command, capture_output=True, text=True, timeout=60)
        path = artifacts_dir / f"run-{attempt}-{phase}-{name}.txt"
        path.write_text(
            completed.stdout + completed.stderr,
            encoding="utf-8",
        )
        reports[name] = {
            "path": str(path),
            "return_code": completed.returncode,
        }
    return reports


def acknowledge_checkpoint(process: subprocess.Popen[str]) -> None:
    if process.stdin is None:
        raise RuntimeError("workload stdin pipe was not created")
    process.stdin.write("continue\n")
    process.stdin.flush()


def retention_store_snapshot(root: Path) -> dict[str, Any]:
    store = root / "store"
    database = store / "store.sqlite"
    hook_snapshot = store / "hook-engine.snapshot.msgpack"
    snapshot: dict[str, Any] = {
        "sqlite_file_bytes": database.stat().st_size if database.is_file() else 0,
        "sqlite_wal_bytes": (store / "store.sqlite-wal").stat().st_size
        if (store / "store.sqlite-wal").is_file()
        else 0,
        "sqlite_shm_bytes": (store / "store.sqlite-shm").stat().st_size
        if (store / "store.sqlite-shm").is_file()
        else 0,
        "hook_snapshot_bytes": hook_snapshot.stat().st_size
        if hook_snapshot.is_file()
        else 0,
    }
    if not database.is_file():
        return snapshot
    connection = sqlite3.connect(f"file:{database}?mode=ro", uri=True, timeout=2)
    try:
        scalar_queries = {
            "events": "SELECT count(*) FROM events",
            "journal_json_bytes": "SELECT coalesce(sum(length(envelope_json)), 0) FROM events",
            "effect_records": (
                "SELECT count(*) FROM events "
                "WHERE lower(coalesce(payload_kind, '')) LIKE '%effect%'"
            ),
            "hook_outbox_rows": "SELECT count(*) FROM hook_dispatch_outbox",
            "receipt_rows": "SELECT count(*) FROM command_receipts",
            "receipt_json_bytes": (
                "SELECT coalesce(sum(length(request_json)), 0) + "
                "coalesce(sum(length(response_json)), 0) FROM command_receipts"
            ),
            "run_head_rows": "SELECT count(*) FROM run_heads",
            "run_head_json_bytes": "SELECT coalesce(sum(length(state_json)), 0) FROM run_heads",
            "provider_view_requests": "SELECT count(*) FROM provider_view_requests",
            "provider_view_blocks": "SELECT count(*) FROM provider_view_blocks",
            "provider_view_ref_bytes": (
                "SELECT coalesce(sum(byte_len), 0) FROM provider_view_blocks"
            ),
            "graph_projection_rows": "SELECT count(*) FROM graph_telemetry_projection",
            "graph_projection_bytes": (
                "SELECT coalesce(sum(length(tool_state) + length(projection)), 0) "
                "FROM graph_telemetry_projection"
            ),
            "graph_dirty_rows": "SELECT count(*) FROM graph_telemetry_dirty",
            "projection_checkpoint_rows": "SELECT count(*) FROM session_projection_checkpoints",
            "projection_checkpoint_bytes": (
                "SELECT coalesce(sum(length(payload)), 0) FROM session_projection_checkpoints"
            ),
            "sqlite_page_count": "PRAGMA page_count",
            "sqlite_freelist_count": "PRAGMA freelist_count",
            "sqlite_page_size": "PRAGMA page_size",
        }
        for name, query in scalar_queries.items():
            snapshot[name] = int(connection.execute(query).fetchone()[0] or 0)
        snapshot["event_kinds"] = {
            str(kind): int(count)
            for kind, count in connection.execute(
                "SELECT coalesce(payload_kind, '<null>'), count(*) "
                "FROM events GROUP BY payload_kind ORDER BY payload_kind"
            )
        }
    finally:
        connection.close()
    cas = store / "provider-view-cas"
    cas_files = [path for path in cas.rglob("*") if path.is_file()] if cas.is_dir() else []
    snapshot["provider_view_cas_files"] = len(cas_files)
    snapshot["provider_view_cas_bytes"] = sum(path.stat().st_size for path in cas_files)
    return snapshot


def retention_trace(log_path: Path) -> list[dict[str, Any]]:
    if not log_path.is_file():
        return []
    snapshots: list[dict[str, Any]] = []
    for line in log_path.read_text(encoding="utf-8", errors="replace").splitlines():
        marker = "haider_retention "
        if marker not in line:
            continue
        try:
            snapshots.append(json.loads(line.split(marker, 1)[1]))
        except json.JSONDecodeError:
            continue
    return snapshots


def run_once(args: argparse.Namespace, attempt: int) -> dict[str, Any]:
    # Darwin's AF_UNIX path limit is 103 bytes. The runner's default TMPDIR
    # lives under /private/var/folders and leaves too little room for the
    # daemon's authenticated endpoint name, so use the stable short /tmp
    # alias for this throwaway profile.
    with tempfile.TemporaryDirectory(
        prefix=f"hmd-{attempt}-", dir="/tmp"
    ) as temporary:
        root = Path(temporary)
        home = root / "home"
        home.mkdir()
        environment = os.environ.copy()
        environment.update(
            {
                # The daemon's lockdown ledger is profile-adjacent but still
                # resolves its default root through HOME. Keep every write in
                # the throwaway measurement profile instead of touching the
                # runner's real account state.
                "HOME": str(home),
                "RUST_MIN_STACK": "8388608",
                "HAIDER_DISCOVERY_DISABLED": "1",
                "HAIDER_TEST_DEVICE_NAME": "test-mac",
                "HAIDER_TEST_FAKE_PROVIDER": fake_script(
                    args.turns, args.fleet_sessions, args.compact_every
                ),
            }
        )
        if args.retention_attribution:
            environment["HAIDER_DAEMON_RETENTION_TRACE"] = "1"
        command = [
            str(args.driver),
            "--daemon",
            str(args.daemon),
            "--root",
            str(root),
            "--turns",
            str(args.turns),
            "--sessions",
            str(args.fleet_sessions),
            "--settle-seconds",
            str(args.settle_seconds),
            "--attached-settle-seconds",
            str(args.attached_settle_seconds),
            "--checkpoint-acks",
        ]
        if args.compact_every is not None:
            command.extend(["--compact-every", str(args.compact_every)])
        uptime_before = subprocess.run(
            ["uptime"], check=True, capture_output=True, text=True
        ).stdout.strip()
        process = subprocess.Popen(
            command,
            env=environment,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            stdin=subprocess.PIPE,
            text=True,
            bufsize=1,
        )
        if process.stdout is None:
            process.kill()
            raise RuntimeError("workload stdout pipe was not created")
        samples: dict[str, dict[str, Any]] = {}
        reports: dict[str, dict[str, Any]] = {}
        daemon_pid: int | None = None
        stdout_lines: list[str] = []
        retention_turns: list[dict[str, Any]] = []
        try:
            for raw_line in process.stdout:
                line = raw_line.strip()
                if not line:
                    continue
                stdout_lines.append(line)
                event = json.loads(line)
                phase = event.get("phase")
                if phase == "ready":
                    daemon_pid = int(event["pid"])
                    samples["ready"] = checkpoint(daemon_pid)
                    acknowledge_checkpoint(process)
                elif phase == "idle_settled" and daemon_pid is not None:
                    samples["idle_settled"] = checkpoint(daemon_pid)
                    reports["idle_settled"] = capture_process_reports(
                        args.artifacts_dir, daemon_pid, attempt, "idle-settled"
                    )
                    samples["workload_start"] = checkpoint(daemon_pid)
                    acknowledge_checkpoint(process)
                elif phase == "turn" and daemon_pid is not None:
                    turn = int(event["turn"])
                    session = int(event.get("session", 1))
                    if args.retention_attribution:
                        retention_turns.append(
                            {
                                "session": session,
                                "turn": turn,
                                **retention_store_snapshot(root),
                            }
                        )
                    if args.fleet_sessions == 1 and turn in (min(20, args.turns), args.turns):
                        samples[f"turn_{turn}"] = checkpoint(daemon_pid)
                    if args.compact_every is not None and turn % args.compact_every == 0:
                        samples[f"pre_compaction_s{session}_t{turn}"] = checkpoint(
                            daemon_pid
                        )
                elif phase == "compaction" and daemon_pid is not None:
                    session = int(event.get("session", 1))
                    turn = int(event["turn"])
                    time.sleep(args.compaction_settle_seconds)
                    samples[f"post_compaction_s{session}_t{turn}"] = checkpoint(
                        daemon_pid
                    )
                    acknowledge_checkpoint(process)
                elif phase == "session_complete" and daemon_pid is not None:
                    session = int(event["session"])
                    if session in {1, 10, 25, 50, 75, args.fleet_sessions}:
                        samples[f"fleet_sessions_{session}"] = checkpoint(daemon_pid)
                elif phase == "turns_complete" and daemon_pid is not None:
                    samples["fleet_complete"] = checkpoint(daemon_pid)
                elif phase == "attached_settled" and daemon_pid is not None:
                    samples["attached_settled"] = checkpoint(daemon_pid)
                    acknowledge_checkpoint(process)
                elif phase == "post_turns_settled" and daemon_pid is not None:
                    samples["post_turns_settled"] = checkpoint(daemon_pid)
                    reports["post_turns_settled"] = capture_process_reports(
                        args.artifacts_dir, daemon_pid, attempt, "post-turns-settled"
                    )
                    acknowledge_checkpoint(process)
            stderr = process.stderr.read() if process.stderr is not None else ""
            return_code = process.wait(timeout=10)
        except BaseException:
            process.kill()
            process.wait(timeout=10)
            raise
        if return_code != 0:
            daemon_log_path = root / "haiderd.log"
            daemon_log = (
                daemon_log_path.read_text(encoding="utf-8", errors="replace")
                if daemon_log_path.is_file()
                else "<missing>\n"
            )
            raise RuntimeError(
                f"workload driver failed ({return_code})\n"
                f"stdout:\n{''.join(line + chr(10) for line in stdout_lines)}"
                f"stderr:\n{stderr}"
                f"haiderd.log:\n{daemon_log}"
            )
        middle_turn = min(20, args.turns)
        required = {
            "ready",
            "idle_settled",
            "workload_start",
            "fleet_complete",
            "post_turns_settled",
        }
        if args.fleet_sessions == 1:
            required.update({f"turn_{middle_turn}", f"turn_{args.turns}"})
        # A fleet run drives its sessions back to back, so the driver skips
        # the attached checkpoint that belongs to one long-lived client.
        if args.attached_settle_seconds > 0 and args.fleet_sessions == 1:
            required.add("attached_settled")
        missing = required.difference(samples)
        if missing:
            raise RuntimeError(f"workload omitted checkpoints: {sorted(missing)}")
        idle = samples["idle_settled"]
        turn_middle = samples.get(f"turn_{middle_turn}", samples["fleet_complete"])
        turn_last = samples.get(f"turn_{args.turns}", samples["fleet_complete"])
        post = samples["post_turns_settled"]
        loads = [float(sample["load_1m"]) for sample in samples.values()]
        compaction_returns: list[dict[str, Any]] = []
        previous_footprint = idle["footprint_bytes"]
        if args.compact_every is not None:
            for session in range(1, args.fleet_sessions + 1):
                for turn in range(args.compact_every, args.turns + 1, args.compact_every):
                    pre = samples[f"pre_compaction_s{session}_t{turn}"][
                        "footprint_bytes"
                    ]
                    compacted = samples[f"post_compaction_s{session}_t{turn}"][
                        "footprint_bytes"
                    ]
                    growth = max(0, pre - previous_footprint)
                    retained = max(0, compacted - previous_footprint)
                    returned = max(0, pre - compacted)
                    compaction_returns.append(
                        {
                            "session": session,
                            "turn": turn,
                            "before_bytes": pre,
                            "after_bytes": compacted,
                            "returned_bytes": returned,
                            "return_percent_of_growth": (
                                100.0 * returned / growth if growth else 0.0
                            ),
                            "residual_percent_of_pre_growth": (
                                100.0 * retained / growth if growth else 0.0
                            ),
                        }
                    )
                    previous_footprint = compacted
        return {
            "attempt": attempt,
            "accepted": max(loads) < MAX_LOAD_1M,
            "uptime_before": uptime_before,
            "load_1m_max": max(loads),
            "daemon_pid": daemon_pid,
            "samples": samples,
            "compaction_returns": compaction_returns,
            "reports": reports,
            "retention_store_turns": retention_turns,
            "retention_runtime": retention_trace(root / "haiderd.log"),
            "idle_cpu_ns": idle["cpu_ns"] - samples["ready"]["cpu_ns"],
            "turn_middle_cpu_ns": turn_middle["cpu_ns"]
            - samples["workload_start"]["cpu_ns"],
            "workload_cpu_ns": samples["fleet_complete"]["cpu_ns"]
            - samples["workload_start"]["cpu_ns"],
            "workload_wall_ns": samples["fleet_complete"]["monotonic_ns"]
            - samples["workload_start"]["monotonic_ns"],
            "immediate_growth_bytes": turn_last["footprint_bytes"]
            - idle["footprint_bytes"],
            "settled_growth_bytes": post["footprint_bytes"] - idle["footprint_bytes"],
            "settled_bytes_per_turn": (
                post["footprint_bytes"] - idle["footprint_bytes"]
            )
            / (args.turns * args.fleet_sessions),
        }


def median_and_mad(values: list[int | float]) -> tuple[float, float]:
    median = float(statistics.median(values))
    mad = float(statistics.median(abs(value - median) for value in values))
    return median, mad


def main() -> int:
    args = parse_args()
    if not args.daemon.is_file() or not os.access(args.daemon, os.X_OK):
        raise SystemExit(f"daemon is not executable: {args.daemon}")
    if args.daemon.stat().st_size <= 10 * 1024 * 1024:
        raise SystemExit("daemon binary is implausibly small (must exceed 10 MiB)")
    if not args.driver.is_file() or not os.access(args.driver, os.X_OK):
        raise SystemExit(f"workload driver is not executable: {args.driver}")

    accepted: list[dict[str, Any]] = []
    rejected: list[dict[str, Any]] = []
    attempts = 0
    while len(accepted) < args.runs:
        attempts += 1
        if attempts > args.runs * 4:
            raise SystemExit("could not admit N runs below load1m < 4")
        while os.getloadavg()[0] >= MAX_LOAD_1M:
            time.sleep(5)
        result = run_once(args, attempts)
        print(json.dumps(result, separators=(",", ":")), flush=True)
        (accepted if result["accepted"] else rejected).append(result)

    idle_values = [
        run["samples"]["idle_settled"]["footprint_bytes"] for run in accepted
    ]
    post_values = [
        run["samples"]["post_turns_settled"]["footprint_bytes"] for run in accepted
    ]
    idle_cpu_values = [run["idle_cpu_ns"] for run in accepted]
    turn_cpu_values = [run["turn_middle_cpu_ns"] for run in accepted]
    workload_cpu_values = [run["workload_cpu_ns"] for run in accepted]
    workload_wall_values = [run["workload_wall_ns"] for run in accepted]
    growth_values = [run["settled_growth_bytes"] for run in accepted]
    compaction_return_values = [
        cycle["return_percent_of_growth"]
        for run in accepted
        for cycle in run["compaction_returns"]
    ]
    compaction_residual_values = [
        cycle["residual_percent_of_pre_growth"]
        for run in accepted
        for cycle in run["compaction_returns"]
    ]
    post_compaction_values = [
        cycle["after_bytes"]
        for run in accepted
        for cycle in run["compaction_returns"]
    ]
    idle_median, idle_mad = median_and_mad(idle_values)
    post_median, post_mad = median_and_mad(post_values)
    idle_cpu_median, idle_cpu_mad = median_and_mad(idle_cpu_values)
    turn_cpu_median, turn_cpu_mad = median_and_mad(turn_cpu_values)
    workload_cpu_median, workload_cpu_mad = median_and_mad(workload_cpu_values)
    workload_wall_median, workload_wall_mad = median_and_mad(workload_wall_values)
    growth_median, growth_mad = median_and_mad(growth_values)
    compaction_return = (
        median_and_mad(compaction_return_values) if compaction_return_values else None
    )
    compaction_residual = (
        median_and_mad(compaction_residual_values) if compaction_residual_values else None
    )
    post_compaction = (
        median_and_mad(post_compaction_values) if post_compaction_values else None
    )
    summary = {
        "schema": "haider.daemon-footprint.v2",
        "runs": args.runs,
        "turns": args.turns,
        "fleet_sessions": args.fleet_sessions,
        "compact_every": args.compact_every,
        "compaction_settle_seconds": args.compaction_settle_seconds,
        "settle_seconds": args.settle_seconds,
        "attached_settle_seconds": args.attached_settle_seconds,
        "load_1m_limit": MAX_LOAD_1M,
        "rejected_runs": len(rejected),
        "idle": {"median_bytes": idle_median, "mad_bytes": idle_mad},
        "post_turns": {"median_bytes": post_median, "mad_bytes": post_mad},
        "settled_growth": {
            "median_bytes": growth_median,
            "mad_bytes": growth_mad,
            "median_bytes_per_turn": growth_median
            / (args.turns * args.fleet_sessions),
        },
        "compaction_return": None
        if compaction_return is None
        else {
            "median_percent_of_growth": compaction_return[0],
            "mad_percentage_points": compaction_return[1],
            "median_residual_percent_of_pre_growth": compaction_residual[0],
            "residual_mad_percentage_points": compaction_residual[1],
            "post_compaction_median_bytes": post_compaction[0],
            "post_compaction_mad_bytes": post_compaction[1],
            "post_compaction_span_bytes": max(post_compaction_values)
            - min(post_compaction_values),
        },
        "fleet": {
            "sessions": args.fleet_sessions,
            "turns_per_session": args.turns,
            "measured_turns": args.fleet_sessions * args.turns,
            "settled_median_bytes": post_median,
            "settled_mad_bytes": post_mad,
            "budget_bytes": args.fleet_budget_bytes,
        },
        "idle_cpu": {"median_ns": idle_cpu_median, "mad_ns": idle_cpu_mad},
        "turn_middle_cpu": {"median_ns": turn_cpu_median, "mad_ns": turn_cpu_mad},
        "workload_cpu": {
            "median_ns": workload_cpu_median,
            "mad_ns": workload_cpu_mad,
        },
        "workload_wall": {
            "median_ns": workload_wall_median,
            "mad_ns": workload_wall_mad,
        },
        "budgets": {
            "idle_bytes": args.idle_budget_bytes,
            "post_turns_bytes": args.post_turns_budget_bytes,
            "fleet_bytes": args.fleet_budget_bytes,
            "calibrated_idle_1_10x": int(idle_median * 1.10 + 0.999),
            "calibrated_post_turns_1_10x": int(post_median * 1.10 + 0.999),
        },
        "accepted_runs": accepted,
        "rejected": rejected,
    }
    rendered = json.dumps(summary, indent=2, sort_keys=True) + "\n"
    if args.output is not None:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(rendered, encoding="utf-8")
    print(rendered, end="")

    failures: list[str] = []
    if args.idle_budget_bytes and idle_median > args.idle_budget_bytes:
        failures.append(
            f"idle median {idle_median:.0f} > budget {args.idle_budget_bytes}"
        )
    if (
        args.fleet_sessions == 1
        and args.post_turns_budget_bytes
        and post_median > args.post_turns_budget_bytes
    ):
        failures.append(
            f"post-turn median {post_median:.0f} > budget {args.post_turns_budget_bytes}"
        )
    if (
        args.fleet_sessions > 1
        and args.fleet_budget_bytes
        and post_median > args.fleet_budget_bytes
    ):
        failures.append(
            f"fleet settled median {post_median:.0f} > budget {args.fleet_budget_bytes}"
        )
    if failures:
        print("daemon footprint budget: FAIL: " + "; ".join(failures), file=sys.stderr)
        return 1
    print("daemon footprint budget: PASS", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
