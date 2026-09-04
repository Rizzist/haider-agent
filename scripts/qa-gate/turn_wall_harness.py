#!/usr/bin/env python3
"""Steady-state warm-daemon turn wall harness (stdlib only)."""

from __future__ import annotations

import argparse
from datetime import datetime, timezone
import json
import math
import os
from pathlib import Path
import platform
import subprocess
import sys
import tempfile
from typing import Any

from turnperf_support import (
    CLIENT_TRACE_ENV,
    FakeProvider,
    ProofError,
    TRACE_ENV,
    ThrowawayProfile,
    assert_provider_ledger,
    assert_tool_effect,
    load_one_minute,
    median_mad,
    process_cpu_ms,
    profile_lock_is_free,
    process_peak_rss_kib,
    process_rss_kib,
    profile_daemon_pid,
    runtime_socket_paths,
    run_arguments,
    sha256_file,
    validate_jsonl,
    wait_pid_gone,
    wait_session_idle,
)


WARMUPS_PER_SHAPE = 5
MEASURED_PER_SHAPE = 25
ONE_SHOT_WARMUPS = 5
ONE_SHOT_MEASURED = 21
ONE_SHOT_MIN_MEASURED = 21
LOAD_LIMIT = 3.0  # baseline conditions (loads 2.4-2.5); lane load yields ENV_BLOCKED, never a false FAIL
OWNER_TARGET_MS = {"single": 40.0, "tool": 60.0}


def _abba(rounds: int) -> list[str]:
    order: list[str] = []
    for index in range(rounds):
        order.extend(("single", "tool") if index % 2 == 0 else ("tool", "single"))
    return order


def _trace_records(text: str) -> list[dict[str, int | str]]:
    records: list[dict[str, int | str]] = []
    for line in text.splitlines():
        if "target=haider.turn" not in line:
            continue
        record: dict[str, int | str] = {}
        for token in line.split():
            if "=" not in token:
                continue
            key, value = token.split("=", 1)
            if key in {
                "operation_micros",
                "turn_ordinal",
                "request_ordinal",
                "txn_ordinal",
                "start_us_from_accept",
                "end_us_from_accept",
                "unix_micros",
            }:
                try:
                    record[key] = int(value)
                except ValueError:
                    continue
            elif key in {
                "target",
                "phase",
                "level",
                "session_id",
                "run_id",
                "turn_id",
                "request_kind",
            }:
                record[key] = value
        if record.get("target") == "haider.turn" and isinstance(record.get("phase"), str):
            records.append(record)
    return records


def _lifecycle_trace_records(text: str) -> list[dict[str, int | str]]:
    records: list[dict[str, int | str]] = []
    for line in text.splitlines():
        if "target=haider.lifecycle" not in line:
            continue
        record: dict[str, int | str] = {}
        for token in line.split():
            if "=" not in token:
                continue
            key, value = token.split("=", 1)
            if key in {"operation_micros", "unix_micros"}:
                try:
                    record[key] = int(value)
                except ValueError:
                    continue
            elif key in {"target", "phase", "level"}:
                record[key] = value
        if record.get("target") == "haider.lifecycle" and record.get("phase") in {
            "spawn_ready",
            "client_accepted_seen",
        }:
            records.append(record)
    return records


def _daemon_trace(profile: ThrowawayProfile) -> str:
    path = profile.profile / "daemon.log"
    try:
        return path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return ""


def _git_commit() -> str | None:
    try:
        result = subprocess.run(
            ["git", "rev-parse", "HEAD"],
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=2,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired):
        return None
    value = result.stdout.strip()
    return value if result.returncode == 0 and value else None


def _host_facts() -> dict[str, Any]:
    return {
        "platform": platform.platform(),
        "machine": platform.machine(),
        "processor": platform.processor(),
        "logical_cpus": os.cpu_count(),
        "python": platform.python_version(),
        "power": os.environ.get("HAIDER_TURNPERF_POWER", "unrecorded"),
    }


def _one_shot_parameter_reasons(
    warmups: int, measured: int, load_limit: float
) -> list[str]:
    reasons: list[str] = []
    if warmups < ONE_SHOT_WARMUPS:
        reasons.append(
            f"warmups={warmups} is below one-shot proof minimum {ONE_SHOT_WARMUPS}"
        )
    if measured < ONE_SHOT_MIN_MEASURED:
        reasons.append(
            f"measured={measured} is below one-shot proof minimum {ONE_SHOT_MIN_MEASURED}"
        )
    if not math.isfinite(load_limit) or load_limit != LOAD_LIMIT:
        reasons.append(f"load_limit={load_limit!r} is not proof pin {LOAD_LIMIT:.1f}")
    return reasons


def _stage_summary(values: list[float]) -> dict[str, float | int]:
    median, mad = median_mad(values)
    return {"count": len(values), "median": median, "mad": mad}


def run_one_shot_harness(
    bin_dir: Path,
    *,
    warmups: int = ONE_SHOT_WARMUPS,
    measured: int = ONE_SHOT_MEASURED,
    load_limit: float = LOAD_LIMIT,
    trace: bool = False,
    keep_root: bool = False,
    budget_ms: float | None = None,
    cpu_total_21_budget_ms: float | None = None,
    peak_rss_budget_kib: float | None = None,
    commit_label: str | None = None,
) -> dict[str, Any]:
    """Measure complete TTL=0 run lifecycles on a virgin profile per sample."""
    if warmups < 0 or measured < 1:
        raise ProofError("warmups must be non-negative and measured must be positive")
    for name, value in (
        ("wall budget", budget_ms),
        ("CPU-total budget", cpu_total_21_budget_ms),
        ("peak-RSS budget", peak_rss_budget_kib),
    ):
        if value is not None and (not math.isfinite(value) or value <= 0):
            raise ProofError(f"{name} must be finite and positive")
    root = Path(tempfile.mkdtemp(prefix="htp-one-shot-"))
    proxy_ledger = root / "provider-ledger.jsonl"
    samples: list[dict[str, Any]] = []
    warmup_rows: list[dict[str, Any]] = []
    correctness_failures: list[str] = []
    budget_failures: list[str] = []
    load: dict[str, float] = {}
    trace_records: list[dict[str, int | str]] = []
    lifecycle_trace_records: list[dict[str, int | str]] = []
    retained_profiles: list[str] = []
    try:
        with FakeProvider(proxy_ledger) as proxy:

            def one_case(sample_index: int, reported: bool) -> None:
                phase = "measured" if reported else "warmup"
                profile = ThrowawayProfile(
                    bin_dir,
                    proxy.base_url,
                    root=root / f"{phase}-{sample_index:03d}",
                )
                case_id = proxy.state.begin_case("single")
                overrides = {
                    "HAIDER_RUN_DAEMON_IDLE_TTL_MS": "0",
                    # One-shot lifecycle markers are client-only: propagating
                    # the daemon's full trace port perturbs pre-Ready startup.
                    CLIENT_TRACE_ENV: "1" if trace else None,
                    TRACE_ENV: None,
                }
                observed_pid: int | None = None
                try:
                    result = profile.command(
                        run_arguments("single"),
                        timeout=82,
                        overrides=overrides,
                        observe_spawned_daemon=True,
                    )
                    observed_pid = result.observed_pid
                    daemon_trace = _daemon_trace(profile)
                    if result.timed_out or result.returncode != 0:
                        raise ProofError(
                            f"one-shot case={case_id} process failed exit={result.returncode} "
                            f"timed_out={result.timed_out} stderr={result.stderr[-500:]!r}"
                        )
                    parsed = validate_jsonl(result.stdout, "single")
                    if not proxy.state.wait_idle(2):
                        raise ProofError(
                            f"one-shot case={case_id} proxy handlers did not settle"
                        )
                    ledger = proxy.state.snapshot_case()
                    assert_provider_ledger(ledger, "single")
                    if proxy.state.read_disk_ledger() != proxy.state.snapshot_all():
                        raise ProofError("on-disk provider ledger diverged from proxy memory")
                    if result.observed_pid is None or result.observed_peak_rss_kib <= 0:
                        raise ProofError(
                            f"one-shot case={case_id} did not observe spawned daemon RSS"
                        )
                    if not wait_pid_gone(result.observed_pid, 2):
                        raise ProofError(
                            f"one-shot case={case_id} returned before observed daemon exited "
                            f"pid={result.observed_pid}"
                        )
                    owner_after_exit = profile_daemon_pid(profile.profile)
                    if owner_after_exit is not None or (profile.profile / "lock.owner").exists():
                        raise ProofError(
                            f"one-shot case={case_id} returned with daemon owner "
                            f"pid={owner_after_exit!r}"
                        )
                    if not profile_lock_is_free(profile.profile):
                        raise ProofError(
                            f"one-shot case={case_id} returned with profile lock held"
                        )
                    sockets_after_exit = runtime_socket_paths(profile.runtime)
                    if sockets_after_exit:
                        raise ProofError(
                            f"one-shot case={case_id} left runtime sockets "
                            f"{[str(path) for path in sockets_after_exit]}"
                        )
                    no_spawn_status = profile.command(
                        ["status", "--json", "--no-spawn"], timeout=10
                    )
                    if no_spawn_status.returncode != 69:
                        raise ProofError(
                            f"one-shot case={case_id} left a reachable daemon; "
                            f"status exit={no_spawn_status.returncode}"
                        )
                    row: dict[str, Any] = {
                        "index": sample_index,
                        "case_id": case_id,
                        "wall_ms": result.wall_ms,
                        # The CLI reaps its owned daemon, so RUSAGE_CHILDREN for
                        # the CLI process includes the daemon it waited for.
                        "process_tree_cpu_ms": result.cpu_ms,
                        "client_peak_rss_kib": result.child_peak_rss_kib,
                        "daemon_peak_rss_kib": result.observed_peak_rss_kib,
                        "combined_peak_rss_kib": result.combined_peak_rss_kib,
                        "daemon_pid": result.observed_pid,
                        "terminal_kind": parsed["terminal_kind"],
                        "terminal_seq": parsed["terminal_seq"],
                        "provider_requests": len(ledger),
                    }
                    if trace:
                        case_turn_records = _trace_records(result.stderr + daemon_trace)
                        case_lifecycle_records = _lifecycle_trace_records(result.stderr)
                        client_terminal = [
                            record
                            for record in case_turn_records
                            if record.get("phase") == "client_terminal_seen"
                        ]
                        spawn_ready = [
                            record
                            for record in case_lifecycle_records
                            if record.get("phase") == "spawn_ready"
                        ]
                        client_accepted = [
                            record
                            for record in case_lifecycle_records
                            if record.get("phase") == "client_accepted_seen"
                        ]
                        if (
                            len(client_terminal) != 1
                            or len(spawn_ready) != 1
                            or len(client_accepted) != 1
                        ):
                            raise ProofError(
                                f"one-shot case={case_id} trace cardinality "
                                f"terminal={len(client_terminal)} spawn_ready={len(spawn_ready)} "
                                f"client_accepted={len(client_accepted)}"
                            )
                        terminal_unix = client_terminal[0].get("unix_micros")
                        if not isinstance(terminal_unix, int):
                            raise ProofError(
                                f"one-shot case={case_id} terminal trace lacks unix_micros"
                            )
                        terminal_to_exit = result.ended_unix_micros - terminal_unix
                        if terminal_to_exit < 0:
                            raise ProofError(
                                f"one-shot case={case_id} terminal trace follows process exit"
                            )
                        row["stages_ms"] = {
                            "spawn_to_ready": float(spawn_ready[0]["operation_micros"])
                            / 1_000,
                            "ready_to_accept": float(
                                client_accepted[0]["operation_micros"]
                            )
                            / 1_000,
                            "accept_to_terminal": float(
                                client_terminal[0]["operation_micros"]
                            )
                            / 1_000,
                            "terminal_to_exit": float(terminal_to_exit) / 1_000,
                        }
                        if reported:
                            trace_records.extend(case_turn_records)
                            lifecycle_trace_records.extend(case_lifecycle_records)
                    (samples if reported else warmup_rows).append(row)
                finally:
                    if keep_root:
                        retained_profiles.append(str(profile.root))
                    profile.dispose(
                        observed_pid=observed_pid,
                        remove_root=not keep_root,
                    )

            for index in range(1, warmups + 1):
                one_case(index, False)

            load["start"] = load_one_minute()
            midpoint = measured // 2
            for index in range(1, measured + 1):
                one_case(index, True)
                if index == midpoint:
                    load["mid"] = load_one_minute()
            load["end"] = load_one_minute()
    finally:
        if not keep_root:
            try:
                proxy_ledger.unlink()
                root.rmdir()
            except OSError:
                pass

    overload = [name for name, value in load.items() if value >= load_limit]
    measurement_reasons = [
        f"load {name}={load[name]:.2f} is not below {load_limit:.2f}" for name in overload
    ] + _one_shot_parameter_reasons(warmups, measured, load_limit)
    measurement_accepted = not measurement_reasons
    if len(samples) != measured:
        correctness_failures.append(
            f"measured count expected={measured} actual={len(samples)}"
        )
    wall_median, wall_mad = median_mad([row["wall_ms"] for row in samples])
    cpu_median, cpu_mad = median_mad(
        [row["process_tree_cpu_ms"] for row in samples]
    )
    cpu_total = sum(row["process_tree_cpu_ms"] for row in samples)
    cpu_total_21 = cpu_total * 21 / measured
    peak_rss = max(row["combined_peak_rss_kib"] for row in samples)
    if measurement_accepted and budget_ms is not None and wall_median > budget_ms:
        budget_failures.append(
            f"one-shot wall median {wall_median:.3f}ms exceeds budget {budget_ms:.3f}ms"
        )
    if (
        measurement_accepted
        and cpu_total_21_budget_ms is not None
        and cpu_total_21 > cpu_total_21_budget_ms
    ):
        budget_failures.append(
            f"one-shot CPU total normalized to 21 {cpu_total_21:.3f}ms exceeds "
            f"budget {cpu_total_21_budget_ms:.3f}ms"
        )
    if (
        measurement_accepted
        and peak_rss_budget_kib is not None
        and peak_rss > peak_rss_budget_kib
    ):
        budget_failures.append(
            f"one-shot combined peak RSS {peak_rss:.0f}KiB exceeds budget "
            f"{peak_rss_budget_kib:.0f}KiB"
        )
    stage_summary: dict[str, Any] = {}
    if trace:
        for stage in (
            "spawn_to_ready",
            "ready_to_accept",
            "accept_to_terminal",
            "terminal_to_exit",
        ):
            stage_summary[stage] = _stage_summary(
                [row["stages_ms"][stage] for row in samples]
            )
    report = {
        "schema": "haider.turn-wall.v1",
        "mode": "one-shot",
        "created_at_utc": datetime.now(timezone.utc).isoformat(),
        "commit": commit_label or _git_commit(),
        "host": _host_facts(),
        "binaries": {
            "haider": {
                "path": str((bin_dir / "haider").resolve()),
                "sha256": sha256_file(bin_dir / "haider"),
            },
            "haiderd": {
                "path": str((bin_dir / "haiderd").resolve()),
                "sha256": sha256_file(bin_dir / "haiderd"),
            },
            "proxy_source_sha256": sha256_file(
                Path(__file__).with_name("turnperf_support.py")
            ),
            "harness_source_sha256": sha256_file(Path(__file__)),
        },
        "parameters": {
            "warmups": warmups,
            "measured": measured,
            "order": "fresh-profile-per-sample",
            "daemon_idle_ttl_ms": 0,
            "load_limit_one_minute": load_limit,
            "trace": trace,
        },
        "load_one_minute": load,
        "measurement_accepted": measurement_accepted,
        "measurement_reasons": measurement_reasons,
        "warmups": warmup_rows,
        "samples": samples,
        "summary": {
            "wall_ms": {"median": wall_median, "mad": wall_mad},
            "process_tree_cpu_ms": {"median": cpu_median, "mad": cpu_mad},
            "process_tree_cpu_total_ms": cpu_total,
            "process_tree_cpu_total_21_normalized_ms": cpu_total_21,
            "peak_rss_kib": peak_rss,
            "client_peak_rss_kib": max(row["client_peak_rss_kib"] for row in samples),
            "daemon_peak_rss_kib": max(row["daemon_peak_rss_kib"] for row in samples),
            "budget_ms": budget_ms,
            "budget_pass": (
                None
                if budget_ms is None or not measurement_accepted
                else wall_median <= budget_ms
            ),
            "cpu_total_21_budget_ms": cpu_total_21_budget_ms,
            "cpu_total_21_budget_pass": (
                None
                if cpu_total_21_budget_ms is None or not measurement_accepted
                else cpu_total_21 <= cpu_total_21_budget_ms
            ),
            "peak_rss_budget_kib": peak_rss_budget_kib,
            "peak_rss_budget_pass": (
                None
                if peak_rss_budget_kib is None or not measurement_accepted
                else peak_rss <= peak_rss_budget_kib
            ),
        },
        "trace_records": trace_records,
        "lifecycle_trace_records": lifecycle_trace_records,
        "trace_stage_summary": stage_summary,
        "correctness_failures": correctness_failures,
        "budget_failures": budget_failures,
        "failures": correctness_failures + budget_failures,
        "passed": not correctness_failures and not budget_failures and measurement_accepted,
    }
    if keep_root:
        report["retained_root"] = str(root)
        report["retained_profiles"] = retained_profiles
    return report


def run_harness(
    bin_dir: Path,
    *,
    warmups: int = WARMUPS_PER_SHAPE,
    measured: int = MEASURED_PER_SHAPE,
    load_limit: float = LOAD_LIMIT,
    trace: bool = False,
    keep_root: bool = False,
    budget_ms: dict[str, float] | None = None,
    resource_baseline: dict[str, dict[str, float]] | None = None,
    commit_label: str | None = None,
) -> dict[str, Any]:
    if warmups < 0 or measured < 1:
        raise ProofError("warmups must be non-negative and measured must be positive")
    root = Path(tempfile.mkdtemp(prefix="htp-run-", dir="/tmp"))
    proxy_ledger = root / "provider-ledger.jsonl"
    profile: ThrowawayProfile | None = None
    correctness_failures: list[str] = []
    budget_failures: list[str] = []
    samples: dict[str, list[dict[str, Any]]] = {"single": [], "tool": []}
    warmup_rows: list[dict[str, Any]] = []
    load: dict[str, float] = {}
    identity: tuple[int, int] | None = None
    daemon_peak_rss_kib = 0
    stop_evidence: dict[str, Any] = {}
    trace_stderr: list[str] = []
    provider_ledger: list[dict[str, Any]] = []
    try:
        with FakeProvider(proxy_ledger) as proxy:
            profile = ThrowawayProfile(bin_dir, proxy.base_url, root=root / "profile-root")
            trace_override = {TRACE_ENV: "1" if trace else None}
            profile.ready(trace_override)
            pid, generation, _status = profile.status()
            identity = (pid, generation)
            daemon_peak_rss_kib = max(daemon_peak_rss_kib, process_peak_rss_kib(pid))

            def one_case(shape: str, sample_index: int, reported: bool) -> None:
                nonlocal daemon_peak_rss_kib
                case_id = proxy.state.begin_case(shape)
                daemon_cpu_before = process_cpu_ms(pid)
                result = profile.command(
                    run_arguments(shape),
                    timeout=82,
                    overrides=trace_override,
                    observe_pid=pid,
                )
                daemon_cpu_after = process_cpu_ms(pid)
                daemon_peak = result.observed_peak_rss_kib
                if result.child_peak_rss_kib <= 0 or daemon_peak <= 0:
                    raise ProofError(
                        f"{shape} case={case_id} peak RSS unavailable "
                        f"client={result.child_peak_rss_kib} daemon={daemon_peak}"
                    )
                if result.timed_out or result.returncode != 0:
                    raise ProofError(
                        f"{shape} case={case_id} process failed exit={result.returncode} "
                        f"timed_out={result.timed_out} stderr={result.stderr[-500:]!r}"
                    )
                parsed = validate_jsonl(result.stdout, shape)
                wait_session_idle(profile, parsed["session_id"])
                if shape == "tool":
                    assert_tool_effect(profile.root, case_id)
                if not proxy.state.wait_idle(2):
                    raise ProofError(f"{shape} case={case_id} proxy handlers did not settle")
                ledger = proxy.state.snapshot_case()
                assert_provider_ledger(ledger, shape)
                if proxy.state.read_disk_ledger() != proxy.state.snapshot_all():
                    raise ProofError("on-disk provider ledger diverged from proxy memory")
                actual_pid, actual_generation, _ = profile.status()
                if (actual_pid, actual_generation) != identity:
                    raise ProofError(
                        "daemon identity changed during steady-state run "
                        f"expected={identity} actual={(actual_pid, actual_generation)}"
                    )
                daemon_rss = process_rss_kib(pid)
                daemon_cpu_ms = max(0.0, daemon_cpu_after - daemon_cpu_before)
                daemon_peak_rss_kib = max(daemon_peak_rss_kib, daemon_peak)
                client_trace = _trace_records(result.stderr) if trace else []
                client_terminal = [
                    record
                    for record in client_trace
                    if record.get("phase") == "client_terminal_seen"
                ]
                if trace and len(client_terminal) != 1:
                    raise ProofError(
                        f"{shape} case={case_id} client terminal trace expected=1 "
                        f"actual={len(client_terminal)}"
                    )
                row = {
                    "index": sample_index,
                    "case_id": case_id,
                    "wall_ms": result.wall_ms,
                    "client_cpu_ms": result.cpu_ms,
                    "daemon_cpu_ms": daemon_cpu_ms,
                    "combined_cpu_ms": result.cpu_ms + daemon_cpu_ms,
                    "client_peak_rss_kib": result.child_peak_rss_kib,
                    "daemon_rss_kib": daemon_rss,
                    "daemon_peak_rss_kib": daemon_peak,
                    "combined_peak_rss_kib": result.combined_peak_rss_kib,
                    "provider_requests": len(ledger),
                    "terminal_kind": parsed["terminal_kind"],
                    "terminal_seq": parsed["terminal_seq"],
                    "session_id": ledger[0]["turn_id"].split("/")[0],
                    "run_id": ledger[0]["turn_id"].split("/")[1],
                    "turn_ordinal": int(ledger[0]["turn_id"].split("/")[2]),
                }
                if reported:
                    samples[shape].append(row)
                else:
                    warmup_rows.append({"shape": shape, **row})
                if trace and result.stderr:
                    trace_stderr.append(result.stderr)

            for index, shape in enumerate(_abba(warmups), start=1):
                one_case(shape, index, False)

            load["start"] = load_one_minute()
            measured_order = _abba(measured)
            midpoint = len(measured_order) // 2
            per_shape_index = {"single": 0, "tool": 0}
            for position, shape in enumerate(measured_order):
                per_shape_index[shape] += 1
                one_case(shape, per_shape_index[shape], True)
                if position + 1 == midpoint:
                    load["mid"] = load_one_minute()
            load["end"] = load_one_minute()

            final_pid, final_generation, _ = profile.status()
            if (final_pid, final_generation) != identity:
                correctness_failures.append(
                    f"final daemon identity expected={identity} actual={(final_pid, final_generation)}"
                )
            stop = profile.stop()
            try:
                stop_document = json.loads(stop.stdout)
            except json.JSONDecodeError:
                stop_document = {}
            stop_evidence = {
                "returncode": stop.returncode,
                "outcome": stop_document.get("outcome"),
            }
            if stop.returncode != 0 or stop_document.get("outcome") != "stopped_cleanly":
                correctness_failures.append(f"exact daemon stop failed: {stop_evidence}")
            provider_ledger = proxy.state.read_disk_ledger()
    finally:
        if profile is not None and not stop_evidence:
            stop = profile.stop()
            stop_evidence = {"returncode": stop.returncode, "stdout": stop.stdout.strip()}
            if stop.returncode != 0:
                error = ProofError(f"exact daemon cleanup failed: {stop_evidence}")
                active = sys.exc_info()[1]
                if active is None:
                    raise error
                active.add_note(str(error))
        if sys.exc_info()[0] is not None and profile is not None:
            profile.dispose()

    overload = [name for name, value in load.items() if value >= load_limit]
    parameter_reasons = []
    if warmups != WARMUPS_PER_SHAPE:
        parameter_reasons.append(
            f"warmups_per_shape={warmups} is not proof pin {WARMUPS_PER_SHAPE}"
        )
    if measured != MEASURED_PER_SHAPE:
        parameter_reasons.append(
            f"measured_per_shape={measured} is not proof pin {MEASURED_PER_SHAPE}"
        )
    if not math.isfinite(load_limit) or load_limit != LOAD_LIMIT:
        parameter_reasons.append(
            f"load_limit={load_limit!r} is not proof pin {LOAD_LIMIT:.1f}"
        )
    measurement_reasons = [
        f"load {name}={load[name]:.2f} is not below {load_limit:.2f}" for name in overload
    ] + parameter_reasons
    measurement_accepted = not measurement_reasons

    summary: dict[str, Any] = {}
    for shape in ("single", "tool"):
        if len(samples[shape]) != measured:
            correctness_failures.append(
                f"{shape} measured count expected={measured} actual={len(samples[shape])}"
            )
            continue
        wall_median, wall_mad = median_mad([row["wall_ms"] for row in samples[shape]])
        cpu_median, cpu_mad = median_mad(
            [row["combined_cpu_ms"] for row in samples[shape]]
        )
        peak_rss_kib = max(row["combined_peak_rss_kib"] for row in samples[shape])
        client_peak_rss_kib = max(row["client_peak_rss_kib"] for row in samples[shape])
        conservative_component_peak_sum_rss_kib = (
            daemon_peak_rss_kib + client_peak_rss_kib
        )
        ceiling = budget_ms.get(shape) if budget_ms is not None else None
        resource_limit = (resource_baseline or {}).get(shape, {})
        cpu_limit = resource_limit.get("combined_cpu_ms")
        cpu_baseline_mad = resource_limit.get("combined_cpu_mad_ms", 0.0)
        peak_limit = resource_limit.get("combined_peak_rss_kib")
        peak_tolerance = resource_limit.get("combined_peak_rss_tolerance_kib", 0.0)
        cpu_tolerance = max(cpu_baseline_mad, cpu_mad)
        summary[shape] = {
            "wall_ms": {"median": wall_median, "mad": wall_mad},
            "combined_cpu_ms": {"median": cpu_median, "mad": cpu_mad},
            "peak_rss_kib": peak_rss_kib,
            "client_peak_rss_kib": client_peak_rss_kib,
            "daemon_peak_rss_kib": daemon_peak_rss_kib,
            "combined_peak_rss_kib": peak_rss_kib,
            "conservative_component_peak_sum_rss_kib": (
                conservative_component_peak_sum_rss_kib
            ),
            "owner_target_ms": OWNER_TARGET_MS[shape],
            "budget_ms": ceiling,
            "owner_target_pass": wall_median <= OWNER_TARGET_MS[shape],
            "budget_pass": (
                None
                if ceiling is None or not measurement_accepted
                else wall_median <= ceiling
            ),
            "resource_baseline": {
                "combined_cpu_ms": cpu_limit,
                "combined_cpu_mad_ms": cpu_baseline_mad,
                "combined_cpu_tolerance_ms": cpu_tolerance,
                "combined_peak_rss_kib": peak_limit,
                "combined_peak_rss_tolerance_kib": peak_tolerance,
            },
            "cpu_regression_pass": (
                None if cpu_limit is None else cpu_median <= cpu_limit + cpu_tolerance
            ),
            "peak_rss_regression_pass": (
                None if peak_limit is None else peak_rss_kib <= peak_limit + peak_tolerance
            ),
        }
        # Timing/resource ceilings are meaningful only after the proof pins and
        # all three load observations accept the measurement. Correctness
        # failures remain independent and are never hidden by overload.
        if measurement_accepted and ceiling is not None and wall_median > ceiling:
            budget_failures.append(
                f"{shape} wall median {wall_median:.3f}ms exceeds budget {ceiling:.3f}ms"
            )
        if (
            measurement_accepted
            and cpu_limit is not None
            and cpu_median > cpu_limit + cpu_tolerance
        ):
            budget_failures.append(
                f"{shape} CPU median {cpu_median:.3f}ms exceeds baseline+jitter "
                f"{cpu_limit:.3f}+{cpu_tolerance:.3f}ms"
            )
        if (
            measurement_accepted
            and peak_limit is not None
            and peak_rss_kib > peak_limit + peak_tolerance
        ):
            budget_failures.append(
                f"{shape} peak RSS {peak_rss_kib}KiB exceeds baseline+resolution "
                f"{peak_limit:.0f}+{peak_tolerance:.0f}KiB"
            )

    trace_text = "".join(trace_stderr) + (_daemon_trace(profile) if profile else "")
    trace_records = _trace_records(trace_text) if trace else []
    trace_stage_summary: dict[str, Any] = {}
    if trace:
        grouped: dict[tuple[str, str, int], list[dict[str, int | str]]] = {}
        for record in trace_records:
            session_id = record.get("session_id")
            run_id = record.get("run_id")
            # Client-side terminal timing is intentionally a separate trace
            # source. Only daemon records carry the locked provider tuple.
            if not isinstance(session_id, str) or not isinstance(run_id, str):
                continue
            ordinal = record.get("turn_ordinal")
            if not isinstance(ordinal, int) or ordinal <= 0:
                correctness_failures.append(f"trace record has invalid turn_ordinal={ordinal!r}")
                continue
            numeric = (
                "operation_micros",
                "request_ordinal",
                "txn_ordinal",
                "start_us_from_accept",
                "end_us_from_accept",
            )
            if any(not isinstance(record.get(field), int) for field in numeric):
                correctness_failures.append(
                    f"trace turn={ordinal} phase={record.get('phase')} lacks numeric coordinates"
                )
                continue
            if record["end_us_from_accept"] < record["start_us_from_accept"]:
                correctness_failures.append(
                    f"trace turn={ordinal} phase={record.get('phase')} has reversed timestamps"
                )
            grouped.setdefault((session_id, run_id, ordinal), []).append(record)
        seen_turns: set[tuple[str, str, int]] = set()
        for shape in ("single", "tool"):
            expected_requests = 1 if shape == "single" else 2
            for row in samples[shape]:
                ordinal = row.get("turn_ordinal")
                if not isinstance(ordinal, int) or ordinal <= 0:
                    correctness_failures.append(
                        f"{shape} sample={row['index']} has no client trace ordinal"
                    )
                    continue
                key = (str(row["session_id"]), str(row["run_id"]), ordinal)
                if key in seen_turns:
                    correctness_failures.append(f"trace turn identity reused={key!r}")
                seen_turns.add(key)
                records = grouped.get(key, [])
                phases = [record.get("phase") for record in records]
                for phase in ("accept", "terminal_commit"):
                    if phases.count(phase) != 1:
                        correctness_failures.append(
                            f"trace turn={ordinal} phase={phase} expected=1 "
                            f"actual={phases.count(phase)}"
                        )
                for phase in ("request_attempt_commit", "provider_open", "first_byte"):
                    phase_records = [record for record in records if record.get("phase") == phase]
                    ordinals = sorted(
                        record["request_ordinal"] for record in phase_records
                    )
                    if ordinals != list(range(1, expected_requests + 1)):
                        correctness_failures.append(
                            f"trace turn={ordinal} phase={phase} request_ordinals={ordinals!r}"
                        )
                    for record in phase_records:
                        request_ordinal = int(record["request_ordinal"])
                        expected_turn_id = (
                            f"{key[0]}/{key[1]}/{key[2]}/{request_ordinal}"
                        )
                        if record.get("turn_id") != expected_turn_id:
                            correctness_failures.append(
                                f"trace request identity mismatch expected={expected_turn_id!r} "
                                f"actual={record.get('turn_id')!r}"
                            )
                        if record.get("request_kind") != "primary":
                            correctness_failures.append(
                                f"trace benchmark request kind must be primary "
                                f"actual={record.get('request_kind')!r}"
                            )
                journal_txns = sorted(
                    int(record["txn_ordinal"])
                    for record in records
                    if record.get("phase") == "journal_transaction"
                )
                append_txns = sorted(
                    int(record["txn_ordinal"])
                    for record in records
                    if record.get("phase") == "journal_append_wait"
                )
                if journal_txns != append_txns or any(value <= 0 for value in journal_txns):
                    correctness_failures.append(
                        f"trace turn={ordinal} transaction join mismatch "
                        f"journal={journal_txns!r} append={append_txns!r}"
                    )
                expected_transactions = 8 if shape == "single" else 23
                if len(journal_txns) != expected_transactions:
                    correctness_failures.append(
                        f"trace turn={ordinal} complete transaction count "
                        f"expected={expected_transactions} actual={len(journal_txns)}"
                    )
        measured_trace = [
            record
            for record in trace_records
            if (
                record.get("session_id"),
                record.get("run_id"),
                record.get("turn_ordinal"),
            )
            in seen_turns
        ]
        for phase in sorted(
            {str(record["phase"]) for record in measured_trace if "phase" in record}
        ):
            phase_records = [
                record for record in measured_trace if record.get("phase") == phase
            ]
            durations = [float(record["operation_micros"]) for record in phase_records]
            duration_median, duration_mad = median_mad(durations)
            request_counts: dict[str, int] = {}
            for record in phase_records:
                key = str(record["request_ordinal"])
                request_counts[key] = request_counts.get(key, 0) + 1
            trace_stage_summary[phase] = {
                "count": len(phase_records),
                "operation_micros": {
                    "median": duration_median,
                    "mad": duration_mad,
                },
                "request_ordinal_counts": request_counts,
                "txn_ordinals": sorted(
                    {
                        int(record["txn_ordinal"])
                        for record in phase_records
                        if int(record["txn_ordinal"]) > 0
                    }
                ),
            }

    report = {
        "schema": "haider.turn-wall.v1",
        "created_at_utc": datetime.now(timezone.utc).isoformat(),
        "commit": commit_label or _git_commit(),
        "host": _host_facts(),
        "binaries": {
            "haider": {"path": str((bin_dir / "haider").resolve()), "sha256": sha256_file(bin_dir / "haider")},
            "haiderd": {"path": str((bin_dir / "haiderd").resolve()), "sha256": sha256_file(bin_dir / "haiderd")},
            "proxy_source_sha256": sha256_file(Path(__file__).with_name("turnperf_support.py")),
            "harness_source_sha256": sha256_file(Path(__file__)),
        },
        "parameters": {
            "warmups_per_shape": warmups,
            "measured_per_shape": measured,
            "order": "ABBA",
            "load_limit_one_minute": load_limit,
            "trace": trace,
        },
        "daemon": {
            "pid": identity[0] if identity else None,
            "generation": identity[1] if identity else None,
            "same_identity_whole_run": not any(
                "identity" in failure for failure in correctness_failures
            ),
            "peak_rss_kib": daemon_peak_rss_kib,
            "cleanup": stop_evidence,
        },
        "load_one_minute": load,
        "measurement_accepted": measurement_accepted,
        "measurement_reasons": measurement_reasons,
        "warmups": warmup_rows,
        "samples": samples,
        "summary": summary,
        "trace_records": trace_records,
        "trace_stage_summary": trace_stage_summary,
        "provider_ledger": provider_ledger,
        "provider_ledger_sha256": sha256_file(proxy_ledger),
        "correctness_failures": correctness_failures,
        "budget_failures": budget_failures,
        "failures": correctness_failures + budget_failures,
        "passed": not correctness_failures and not budget_failures and measurement_accepted,
    }
    if keep_root:
        report["retained_root"] = str(root)
    elif profile is not None:
        profile.dispose()
        try:
            proxy_ledger.unlink()
            root.rmdir()
        except OSError:
            pass
    return report


def _arguments(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, required=True)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--one-shot", action="store_true")
    parser.add_argument("--warmups", type=int)
    parser.add_argument("--measured", type=int)
    parser.add_argument("--load-limit", type=float, default=LOAD_LIMIT)
    parser.add_argument("--trace", action="store_true")
    parser.add_argument("--keep-root", action="store_true")
    parser.add_argument("--single-budget-ms", type=float)
    parser.add_argument("--tool-budget-ms", type=float)
    parser.add_argument("--cpu-total-21-budget-ms", type=float)
    parser.add_argument("--peak-rss-budget-kib", type=float)
    parser.add_argument("--commit-label")
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = _arguments(list(sys.argv[1:] if argv is None else argv))
    warmups = args.warmups
    if warmups is None:
        warmups = ONE_SHOT_WARMUPS if args.one_shot else WARMUPS_PER_SHAPE
    measured = args.measured
    if measured is None:
        measured = ONE_SHOT_MEASURED if args.one_shot else MEASURED_PER_SHAPE
    budgets = None
    if args.single_budget_ms is not None or args.tool_budget_ms is not None:
        budgets = {
            "single": (
                args.single_budget_ms
                if args.single_budget_ms is not None
                else OWNER_TARGET_MS["single"]
            ),
            "tool": (
                args.tool_budget_ms
                if args.tool_budget_ms is not None
                else OWNER_TARGET_MS["tool"]
            ),
        }
        if any(not math.isfinite(value) or value <= 0 for value in budgets.values()):
            print("turn-wall harness failed: budgets must be finite and positive", file=sys.stderr)
            return 2
    try:
        if args.one_shot:
            if args.tool_budget_ms is not None:
                raise ProofError("--tool-budget-ms is not valid with --one-shot")
            report = run_one_shot_harness(
                args.bin_dir,
                warmups=warmups,
                measured=measured,
                load_limit=args.load_limit,
                trace=args.trace,
                keep_root=args.keep_root,
                budget_ms=args.single_budget_ms,
                cpu_total_21_budget_ms=args.cpu_total_21_budget_ms,
                peak_rss_budget_kib=args.peak_rss_budget_kib,
                commit_label=args.commit_label,
            )
        else:
            if args.cpu_total_21_budget_ms is not None or args.peak_rss_budget_kib is not None:
                raise ProofError(
                    "--cpu-total-21-budget-ms and --peak-rss-budget-kib require --one-shot"
                )
            report = run_harness(
                args.bin_dir,
                warmups=warmups,
                measured=measured,
                load_limit=args.load_limit,
                trace=args.trace,
                keep_root=args.keep_root,
                budget_ms=budgets,
                commit_label=args.commit_label,
            )
    except Exception as error:
        print(f"turn-wall harness failed: {type(error).__name__}: {error}", file=sys.stderr)
        return 1
    rendered = json.dumps(report, indent=2, sort_keys=True) + "\n"
    if not report["measurement_accepted"]:
        print(rendered, file=sys.stderr, end="")
        if report["failures"]:
            print(
                "turn-wall: FAIL; correctness/budget failure retained while timing publication was refused",
                file=sys.stderr,
            )
            return 1
        print("turn-wall: ENV_BLOCKED; report refused by proof pins/load gate", file=sys.stderr)
        return 75
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(rendered, encoding="utf-8")
    else:
        print(rendered, end="")
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
