#!/usr/bin/env python3
"""Measure a single growing native session through the real CLI and daemon."""

from __future__ import annotations

import argparse
from collections import Counter
from datetime import datetime, timezone
import json
from pathlib import Path
import shutil
import sqlite3
import statistics
import sys
import tempfile

import phase_attribution
from turnperf_support import (
    FakeProvider,
    MODEL_ID,
    PROVIDER_ID,
    ProofError,
    ThrowawayProfile,
    assert_tool_effect,
    load_one_minute,
    process_cpu_times,
    sha256_file,
    parse_json_lines,
    validate_jsonl,
    wait_session_idle,
    _darwin_rusage,
)

QUIET_REQUEST = Path("/Users/rizzist/Developer/haiderharness/state/quiet-request")
QUIET_GRANTED = Path("/Users/rizzist/Developer/haiderharness/state/quiet-granted")


def file_sizes(root: Path) -> dict[str, int]:
    """Measure all profile files without opening or changing SQLite files."""
    return {
        str(path.relative_to(root)): path.stat().st_size
        for path in root.rglob("*")
        if path.is_file()
    }


def file_size_changes(before: dict[str, int], after: dict[str, int]) -> dict[str, int]:
    return {
        path: size - before.get(path, 0)
        for path, size in after.items()
        if size != before.get(path, 0)
    }


def sqlite_page_owners(database: Path) -> dict[str, int]:
    """Read SQLite's page ownership without modifying the live database."""
    with sqlite3.connect(f"file:{database}?mode=ro", uri=True) as connection:
        return {
            name: pages
            for name, pages in connection.execute(
                "SELECT name, SUM(pgsize) FROM dbstat GROUP BY name ORDER BY name"
            )
        }


def provider_history_shape(database: Path) -> dict:
    """Inspect new request cursors and their exact shared-prefix relationship."""
    with sqlite3.connect(f"file:{database}?mode=ro", uri=True) as connection:
        if not connection.execute(
            "SELECT 1 FROM sqlite_master WHERE name = 'provider_view_request_history'"
        ).fetchone():
            return {}
        requests = connection.execute(
            "SELECT r.request_ordinal, h.segment_id, h.block_count "
            "FROM provider_view_requests r "
            "JOIN provider_view_request_history h USING(session_id, request_ordinal) "
            "ORDER BY r.request_ordinal DESC LIMIT 3"
        ).fetchall()
        histories = []
        for ordinal, segment, count in reversed(requests):
            refs = connection.execute(
                "WITH RECURSIVE chain(segment_id, cutoff) AS ("
                "SELECT ?, ? UNION ALL "
                "SELECT s.parent_segment_id, MIN(chain.cutoff, s.parent_block_count) "
                "FROM chain JOIN provider_view_history_segments s "
                "ON s.id = chain.segment_id WHERE s.parent_segment_id IS NOT NULL) "
                "SELECT b.content_hash FROM chain "
                "JOIN provider_view_history_segments s ON s.id = chain.segment_id "
                "JOIN provider_view_history_blocks b ON b.segment_id = s.id "
                "AND b.block_ordinal >= s.parent_block_count "
                "AND b.block_ordinal < chain.cutoff ORDER BY b.block_ordinal",
                (segment, count),
            ).fetchall()
            histories.append((ordinal, segment, count, [row[0] for row in refs]))
        matches = []
        for previous, current in zip(histories, histories[1:]):
            lcp = next(
                (i for i, (left, right) in enumerate(zip(previous[3], current[3]))
                 if left != right),
                min(len(previous[3]), len(current[3])),
            )
            matches.append({"previous": previous[0], "current": current[0], "lcp": lcp})
        return {
            "segments": connection.execute(
                "SELECT COUNT(*) FROM provider_view_history_segments"
            ).fetchone()[0],
            "blocks": connection.execute(
                "SELECT COUNT(*) FROM provider_view_history_blocks"
            ).fetchone()[0],
            "recent": [{"ordinal": row[0], "segment": row[1], "count": row[2],
                        "first_hashes": [value[:15] for value in row[3][:3]],
                        "last_hashes": [value[:15] for value in row[3][-3:]]}
                       for row in histories],
            "matches": matches,
        }


def event_bytes_by_kind(database: Path) -> dict[str, dict[str, int]]:
    with sqlite3.connect(f"file:{database}?mode=ro", uri=True) as connection:
        return {
            kind or "null": {"count": count, "bytes": size, "max": largest}
            for kind, count, size, largest in connection.execute(
                "SELECT payload_kind, COUNT(*), SUM(length(envelope_json)), "
                "MAX(length(envelope_json)) FROM events GROUP BY payload_kind"
            )
        }


def wal_frames(path: Path) -> tuple[bytes | None, list[int]]:
    """Read only valid-salt WAL frame page numbers from the current cycle."""
    if not path.exists():
        return None, []
    data = path.read_bytes()
    if len(data) < 32:
        return None, []
    page_size = int.from_bytes(data[8:12], "big") or 65_536
    salt = data[16:24]
    frame_size = 24 + page_size
    pages = []
    for offset in range(32, len(data) - frame_size + 1, frame_size):
        if data[offset + 8:offset + 16] != salt:
            break
        pages.append(int.from_bytes(data[offset:offset + 4], "big"))
    return salt, pages


def wal_frame_owners(database: Path, pages: list[int]) -> dict[str, int]:
    with sqlite3.connect(f"file:{database}?mode=ro", uri=True) as connection:
        owners = {page: name for name, page in connection.execute(
            "SELECT name, pageno FROM dbstat"
        )}
    return dict(Counter(owners.get(page, "unowned") for page in pages))


def theil_sen_per_100(values: list[float]) -> float:
    """Match row 49's median pairwise per-turn slope, expressed per 100 turns."""
    return 100 * statistics.median(
        (later - earlier) / (j - i)
        for i, earlier in enumerate(values)
        for j, later in enumerate(values[i + 1 :], start=i + 1)
    )


def require_quiet() -> float:
    load = load_one_minute()
    if not QUIET_REQUEST.is_file() or not QUIET_GRANTED.is_file() or load >= 2:
        raise ProofError(
            f"quiet grant/request and load<2 required: grant={QUIET_GRANTED.is_file()} "
            f"request={QUIET_REQUEST.is_file()} load1={load:.3f}"
        )
    return load


def run_session(bin_dir: Path, turns: int, *, phases: bool, quiet: bool,
                disk: bool, wal_pages: bool, wal_autocheckpoint: int | None,
                synchronous: str | None = None) -> dict:
    if turns < 20:
        raise ValueError("at least 20 turns are needed for decile slopes")
    root = Path(tempfile.mkdtemp(prefix="hsg-", dir="/tmp"))
    trace_dir = root / "phase"
    if phases:
        trace_dir.mkdir()
    profile = None
    try:
        with FakeProvider(root / "provider-ledger.jsonl") as provider:
            profile = ThrowawayProfile(bin_dir, provider.base_url, root=root / "x")
            overrides = {
                phase_attribution.ENV: str(trace_dir) if phases else None,
                "HAIDER_DIAG_WAL_AUTOCHECKPOINT_PAGES": (
                    str(wal_autocheckpoint) if wal_autocheckpoint is not None else None
                ),
                "HAIDER_STORE_SYNCHRONOUS": synchronous,
            }
            profile.ready(overrides)
            pid, generation, _ = profile.status()
            session_id = None
            rows = []
            previous_sizes = file_sizes(profile.root) if disk else {}
            previous_written = _darwin_rusage(pid).ri_diskio_byteswritten if disk else 0
            previous_wal = wal_frames(profile.profile / "store.sqlite-wal") if wal_pages else (None, [])
            for turn in range(1, turns + 1):
                load = require_quiet() if quiet else load_one_minute()
                shape = "tool" if turn % 10 == 0 else "single"
                case_id = provider.state.begin_case(shape)
                if session_id is None:
                    args = ["run", "-p", "session growth fixture", "--provider", PROVIDER_ID,
                            "--model", MODEL_ID, "--auto-allow", "--allow-writes",
                            "--allow-exec"]
                else:
                    args = ["run", "--session", session_id, "-p", "session growth fixture"]
                args += ["--output", "jsonl", "--timeout", "20s"]
                daemon_before = process_cpu_times(pid)
                result = profile.command(args, timeout=40, overrides=overrides, observe_pid=pid)
                daemon_cpu = process_cpu_times(pid).delta(daemon_before)
                if result.returncode or result.timed_out:
                    raise ProofError(f"turn {turn} exit={result.returncode} timeout={result.timed_out}: "
                                     f"{result.stderr[-500:]}")
                try:
                    # The daemon can commit an unrelated asynchronous fact
                    # between accepted.head_seq and this run's first event.
                    # Validate this run's stream independently of that gap.
                    parsed = validate_jsonl(
                        result.stdout, shape, continuation=turn > 1,
                        allow_intervening=True,
                    )
                except ProofError as error:
                    documents = parse_json_lines(result.stdout, "failed turn JSONL")
                    raise ProofError(
                        f"turn {turn}: {error}; accepted={documents[0] if documents else None}; "
                        f"first_envelope={documents[1] if len(documents) > 1 else None}"
                    ) from error
                session_id = session_id or parsed["session_id"]
                if parsed["session_id"] != session_id:
                    raise ProofError(f"turn {turn} changed session")
                wait_session_idle(profile, session_id)
                if not provider.state.wait_idle(2):
                    raise ProofError(f"turn {turn} provider did not settle")
                requests = provider.state.snapshot_case()
                if len(requests) != (2 if shape == "tool" else 1):
                    raise ProofError(f"turn {turn} has {len(requests)} provider requests")
                if shape == "tool":
                    assert_tool_effect(profile.root, case_id)
                disk_change = None
                if disk:
                    current_sizes = file_sizes(profile.root)
                    current_written = _darwin_rusage(pid).ri_diskio_byteswritten
                    disk_change = {
                        "daemon_write_bytes": current_written - previous_written,
                        "file_size_changes": file_size_changes(previous_sizes, current_sizes),
                        "sqlite_bytes": current_sizes.get("p/store.sqlite", 0),
                        "wal_bytes": current_sizes.get("p/store.sqlite-wal", 0),
                    }
                    disk_change["sqlite_page_owners"] = sqlite_page_owners(
                        profile.profile / "store.sqlite"
                    )
                    disk_change["provider_history_shape"] = provider_history_shape(
                        profile.profile / "store.sqlite"
                    )
                    disk_change["event_bytes_by_kind"] = event_bytes_by_kind(
                        profile.profile / "store.sqlite"
                    )
                    if wal_pages:
                        current_wal = wal_frames(profile.profile / "store.sqlite-wal")
                        reused = current_wal[0] == previous_wal[0]
                        new_pages = current_wal[1][len(previous_wal[1]):] if reused else current_wal[1]
                        disk_change["wal_new_frames"] = len(new_pages)
                        disk_change["wal_salt_changed"] = not reused
                        disk_change["wal_page_owners"] = wal_frame_owners(
                            profile.profile / "store.sqlite", new_pages
                        )
                        previous_wal = current_wal
                    previous_sizes, previous_written = current_sizes, current_written
                rows.append({
                    "turn": turn, "shape": shape, "case_id": case_id,
                    "stdout_bytes": len(result.stdout.encode("utf-8")),
                    "event_count": len(parsed["events"]),
                    "wall_ms": result.wall_ms, "client_cpu_ms": result.cpu_ms,
                    "daemon_cpu_ms": daemon_cpu.self_ms,
                    "daemon_reaped_children_cpu_ms": daemon_cpu.reaped_children_ms,
                    "combined_cpu_ms": result.cpu_ms + daemon_cpu.self_ms + daemon_cpu.reaped_children_ms,
                    "load1": load, "request_count": len(requests),
                    **({"disk": disk_change} if disk else {}),
                    "boundary": {
                        "start_ns": result.started_clock_ns, "end_ns": result.ended_clock_ns,
                        "cpu_ns": round((result.cpu_ms + daemon_cpu.self_ms + daemon_cpu.reaped_children_ms) * 1_000_000),
                        "expected_pids": [result.client_pid, pid],
                        "reaped_children_cpu_ns": round(daemon_cpu.reaped_children_ms * 1_000_000),
                    },
                })
            actual_pid, actual_generation, _ = profile.status()
            if (actual_pid, actual_generation) != (pid, generation):
                raise ProofError("daemon identity changed during the session")
            stopped = profile.stop()
            if stopped.returncode:
                raise ProofError(f"profile-scoped stop failed: {stopped.stderr[-300:]}")
            if phases:
                records, pids = phase_attribution.read_records(trace_dir)
                for row in rows:
                    row["phases"] = phase_attribution.partition(
                        records, **row.pop("boundary"), available_pids=pids
                    )
                    row["phases"].pop("records", None)
            else:
                for row in rows:
                    row.pop("boundary")
            widths = [row["wall_ms"] for row in rows]
            cpu = [row["combined_cpu_ms"] for row in rows]
            decile = turns // 10
            return {
                "created_at_utc": datetime.now(timezone.utc).isoformat(),
                "bin_dir": str(bin_dir),
                "sha256": {name: sha256_file(bin_dir / name) for name in ("haider", "haiderd")},
                "turns": turns, "phases": phases, "quiet": quiet, "disk": disk,
                "wal_pages": wal_pages,
                "wal_autocheckpoint": wal_autocheckpoint,
                "daemon_pid": pid, "session_id": session_id,
                "summary": {
                    "wall_slope_ms_per_100_turns": theil_sen_per_100(widths),
                    "cpu_slope_ms_per_100_turns": theil_sen_per_100(cpu),
                    "first_decile_wall_p50_ms": statistics.median(widths[:decile]),
                    "last_decile_wall_p50_ms": statistics.median(widths[-decile:]),
                    "max_load1": max(row["load1"] for row in rows),
                },
                "rows": rows,
            }
    finally:
        if profile is not None:
            profile.dispose()
        shutil.rmtree(root, ignore_errors=True)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--turns", type=int, default=100)
    parser.add_argument("--repetitions", type=int, default=3)
    parser.add_argument("--phases", action="store_true")
    parser.add_argument("--require-quiet", action="store_true")
    parser.add_argument("--disk", action="store_true")
    parser.add_argument("--wal-pages", action="store_true")
    parser.add_argument("--wal-autocheckpoint", type=int)
    args = parser.parse_args()
    if args.phases and args.turns > 40:
        parser.error("phase tracing is limited to 40 turns to stay below the record cap")
    runs = []
    for index in range(args.repetitions):
        run = run_session(args.bin_dir, args.turns, phases=args.phases,
                          quiet=args.require_quiet, disk=args.disk,
                          wal_pages=args.wal_pages,
                          wal_autocheckpoint=args.wal_autocheckpoint)
        runs.append(run)
        print(f"run {index + 1}/{args.repetitions}: {run['summary']}", file=sys.stderr, flush=True)
        args.output.write_text(json.dumps({"runs": runs}, indent=2) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, ProofError) as error:
        print(f"session growth fixture: {error}", file=sys.stderr)
        sys.exit(1)
