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
    FakeProvider,
    ProofError,
    TRACE_ENV,
    ThrowawayProfile,
    assert_provider_ledger,
    assert_tool_effect,
    load_one_minute,
    median_mad,
    process_cpu_ms,
    process_peak_rss_kib,
    process_rss_kib,
    run_arguments,
    sha256_file,
    validate_jsonl,
    wait_session_idle,
)


WARMUPS_PER_SHAPE = 5
MEASURED_PER_SHAPE = 25
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
                "start_us_from_exec",
                "end_us_from_exec",
            }:
                try:
                    record[key] = int(value)
                except ValueError:
                    continue
            elif key in {"target", "phase", "level", "side", "clock"}:
                record[key] = value
        if record.get("target") == "haider.turn" and isinstance(record.get("phase"), str):
            if "start_us_from_exec" in record or "end_us_from_exec" in record:
                record.setdefault("side", "client")
                record.setdefault("clock", "client_exec")
            else:
                record.setdefault("side", "daemon")
                record.setdefault("clock", "daemon_accept")
            records.append(record)
    return records


def _contains_turn_trace(text: str) -> bool:
    return any("target=haider.turn" in line for line in text.splitlines())


def _record_interval(record: dict[str, int | str]) -> tuple[int, int] | None:
    clock = record.get("clock")
    fields = (
        ("start_us_from_exec", "end_us_from_exec")
        if clock == "client_exec"
        else ("start_us_from_accept", "end_us_from_accept")
    )
    start, end = (record.get(field) for field in fields)
    if isinstance(start, int) and isinstance(end, int):
        return start, end
    return None


def _trace_record_error(record: dict[str, int | str]) -> str | None:
    ordinal = record.get("turn_ordinal")
    phase = record.get("phase")
    if not isinstance(ordinal, int) or ordinal <= 0:
        return f"trace record has invalid turn_ordinal={ordinal!r}"
    for field in ("operation_micros", "request_ordinal", "txn_ordinal"):
        value = record.get(field)
        if not isinstance(value, int) or value < 0:
            return f"trace turn={ordinal} phase={phase} has invalid {field}={value!r}"
    side_clock = (record.get("side"), record.get("clock"))
    coordinate_fields = {
        ("daemon", "daemon_accept"): (
            "start_us_from_accept",
            "end_us_from_accept",
            "start_us_from_exec",
            "end_us_from_exec",
        ),
        ("client", "client_exec"): (
            "start_us_from_exec",
            "end_us_from_exec",
            "start_us_from_accept",
            "end_us_from_accept",
        ),
    }
    if side_clock not in coordinate_fields:
        return f"trace turn={ordinal} phase={phase} has invalid side/clock={side_clock!r}"
    start_field, end_field, foreign_start, foreign_end = coordinate_fields[side_clock]
    start, end = record.get(start_field), record.get(end_field)
    if not isinstance(start, int) or not isinstance(end, int):
        return f"trace turn={ordinal} phase={phase} lacks its clock coordinates"
    if foreign_start in record or foreign_end in record:
        return f"trace turn={ordinal} phase={phase} mixes clock coordinates"
    if start < 0 or end < start:
        return f"trace turn={ordinal} phase={phase} has invalid interval=({start},{end})"
    duration = record["operation_micros"]
    if duration != end - start:
        return (
            f"trace turn={ordinal} phase={phase} duration={duration} "
            f"does not match coordinates={end - start}"
        )
    return None


def _client_process_residual_micros(wall_ms: float, exit_end_us: int) -> float:
    residual = wall_ms * 1_000.0 - exit_end_us
    if residual < 0:
        raise ValueError(
            f"client exit coordinate {exit_end_us}us exceeds process wall {wall_ms * 1_000.0:.1f}us"
        )
    return residual


def _interval_union_micros(intervals: list[tuple[int, int]], cutoff: int) -> int:
    clipped = sorted(
        (max(0, start), min(cutoff, end))
        for start, end in intervals
        if end >= 0 and start <= cutoff and min(cutoff, end) >= max(0, start)
    )
    covered = 0
    cursor_start: int | None = None
    cursor_end = 0
    for start, end in clipped:
        if cursor_start is None:
            cursor_start, cursor_end = start, end
        elif start <= cursor_end:
            cursor_end = max(cursor_end, end)
        else:
            covered += cursor_end - cursor_start
            cursor_start, cursor_end = start, end
    if cursor_start is not None:
        covered += cursor_end - cursor_start
    return covered


def _phase_row(
    *,
    shape: str,
    side: str,
    clock: str,
    phase: str,
    values: list[float],
    spans: list[float] | None,
    records: int,
    turns: int,
) -> dict[str, Any]:
    median, mad = median_mad(values)
    span_median, span_mad = median_mad(spans or values)
    return {
        "shape": shape,
        "side": side,
        "clock": clock,
        "phase": phase,
        "present_turns": len(values),
        "measured_turns": turns,
        "records": records,
        "records_per_present_turn": records / len(values),
        "operation_micros_per_turn": {"median": median, "mad": mad},
        "coordinate_span_micros_per_turn": {
            "median": span_median,
            "mad": span_mad,
        },
    }


def _phase_table_key(record: dict[str, int | str]) -> tuple[str, str, str]:
    phase = str(record.get("phase"))
    if phase == "prompt_assembly":
        scope = "setup" if record.get("request_ordinal") == 0 else "request"
        phase = f"{phase}.{scope}"
    return str(record.get("side")), str(record.get("clock")), phase


def _build_phase_table(
    samples: dict[str, list[dict[str, Any]]],
    grouped: dict[int, list[dict[str, int | str]]],
    correctness_failures: list[str] | None = None,
) -> dict[str, list[dict[str, Any]]]:
    table: dict[str, list[dict[str, Any]]] = {"single": [], "tool": []}
    for shape in ("single", "tool"):
        turns: list[tuple[dict[str, Any], list[dict[str, int | str]]]] = []
        for sample in samples[shape]:
            ordinal = sample.get("turn_ordinal")
            if isinstance(ordinal, int):
                turns.append((sample, grouped.get(ordinal, [])))
        keys = sorted(
            {
                _phase_table_key(record)
                for _, records in turns
                for record in records
            }
        )
        for side, clock, phase in keys:
            values: list[float] = []
            spans: list[float] = []
            record_count = 0
            for _, records in turns:
                matching = [
                    record
                    for record in records
                    if _phase_table_key(record) == (side, clock, phase)
                ]
                if not matching:
                    continue
                values.append(sum(float(record["operation_micros"]) for record in matching))
                intervals = [
                    interval
                    for record in matching
                    if (interval := _record_interval(record)) is not None
                ]
                if intervals:
                    spans.append(
                        float(
                            max(end for _, end in intervals)
                            - min(start for start, _ in intervals)
                        )
                    )
                record_count += len(matching)
            table[shape].append(
                _phase_row(
                    shape=shape,
                    side=side,
                    clock=clock,
                    phase=phase,
                    values=values,
                    spans=spans,
                    records=record_count,
                    turns=len(turns),
                )
            )

        daemon_request_starts: list[float] = []
        daemon_open_ends: list[float] = []
        daemon_residuals: list[float] = []
        client_walls: list[float] = []
        client_residuals: list[float] = []
        for sample, records in turns:
            first_open = next(
                (
                    record
                    for record in records
                    if record.get("phase") == "provider_open"
                    and record.get("request_ordinal") == 1
                ),
                None,
            )
            if first_open is not None:
                interval = _record_interval(first_open)
                if interval is not None:
                    cutoff = interval[0]
                    covered = _interval_union_micros(
                        [
                            candidate_interval
                            for record in records
                            if record.get("clock") == "daemon_accept"
                            and record.get("phase") != "hooks_discovery"
                            and (candidate_interval := _record_interval(record)) is not None
                        ],
                        cutoff,
                    )
                    daemon_request_starts.append(float(cutoff))
                    daemon_open_ends.append(float(interval[1]))
                    daemon_residuals.append(float(max(0, cutoff - covered)))
            exit_record = next(
                (record for record in records if record.get("phase") == "exit"), None
            )
            if exit_record is not None:
                interval = _record_interval(exit_record)
                if interval is not None:
                    client_walls.append(float(interval[1]))
                    try:
                        client_residuals.append(
                            _client_process_residual_micros(
                                float(sample["wall_ms"]), interval[1]
                            )
                        )
                    except ValueError as error:
                        if correctness_failures is None:
                            raise
                        correctness_failures.append(
                            f"{shape} sample={sample.get('index')} {error}"
                        )
        for phase, side, clock, values in [
            (
                "daemon_accept_to_provider_open_start",
                "daemon",
                "daemon_accept",
                daemon_request_starts,
            ),
            (
                "daemon_accept_to_first_provider_open_end",
                "daemon",
                "daemon_accept",
                daemon_open_ends,
            ),
            (
                "daemon_unattributed_to_provider_open_start",
                "derived",
                "daemon_accept",
                daemon_residuals,
            ),
            ("client_exec_to_exit", "client", "client_exec", client_walls),
            ("client_process_residual", "derived", "process_wall", client_residuals),
        ]:
            if values:
                table[shape].append(
                    _phase_row(
                        shape=shape,
                        side=side,
                        clock=clock,
                        phase=phase,
                        values=values,
                        spans=None,
                        records=len(values),
                        turns=len(turns),
                    )
                )
        table[shape].sort(key=lambda row: (row["side"], row["clock"], row["phase"]))
    return table


def _render_phase_table(table: dict[str, list[dict[str, Any]]]) -> str:
    lines = [
        "turn-wall phase attribution (per-turn sums; microseconds)",
        "shape   side     clock          phase                                      turns  rec/turn    median       MAD",
    ]
    for shape in ("single", "tool"):
        for row in table.get(shape, []):
            operation = row["operation_micros_per_turn"]
            lines.append(
                f"{shape:<7} {row['side']:<8} {row['clock']:<14} {row['phase']:<42} "
                f"{row['present_turns']:>2}/{row['measured_turns']:<2} "
                f"{row['records_per_present_turn']:>8.2f} "
                f"{operation['median']:>9.1f} {operation['mad']:>9.1f}"
            )
    return "\n".join(lines)


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
                if not trace and _contains_turn_trace(result.stderr):
                    correctness_failures.append(
                        f"{shape} case={case_id} emitted client trace while {TRACE_ENV} was off"
                    )
                client_terminal = [
                    record
                    for record in client_trace
                    if record.get("phase") == "terminal_seen"
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
                    "turn_ordinal": (
                        client_terminal[0].get("turn_ordinal") if client_terminal else None
                    ),
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

    daemon_trace_text = _daemon_trace(profile) if profile else ""
    if not trace and _contains_turn_trace(daemon_trace_text):
        correctness_failures.append(
            f"daemon emitted trace while {TRACE_ENV} was off"
        )
    trace_text = "".join(trace_stderr) + daemon_trace_text
    trace_records = _trace_records(trace_text) if trace else []
    trace_stage_summary: dict[str, Any] = {}
    trace_phase_table: dict[str, list[dict[str, Any]]] = {"single": [], "tool": []}
    if trace:
        grouped: dict[int, list[dict[str, int | str]]] = {}
        for record in trace_records:
            if error := _trace_record_error(record):
                correctness_failures.append(error)
                continue
            ordinal = int(record["turn_ordinal"])
            grouped.setdefault(ordinal, []).append(record)
        seen_ordinals: set[int] = set()
        for shape in ("single", "tool"):
            expected_requests = 1 if shape == "single" else 2
            for row in samples[shape]:
                ordinal = row.get("turn_ordinal")
                if not isinstance(ordinal, int) or ordinal <= 0:
                    correctness_failures.append(
                        f"{shape} sample={row['index']} has no client trace ordinal"
                    )
                    continue
                if ordinal in seen_ordinals:
                    correctness_failures.append(f"trace turn ordinal reused={ordinal}")
                seen_ordinals.add(ordinal)
                records = grouped.get(ordinal, [])

                def matching(
                    phase: str,
                    side: str,
                    clock: str,
                    request_ordinal: int | None = None,
                    txn_ordinal: int | None = None,
                ) -> list[dict[str, int | str]]:
                    return [
                        record
                        for record in records
                        if record.get("phase") == phase
                        and record.get("side") == side
                        and record.get("clock") == clock
                        and (
                            request_ordinal is None
                            or record.get("request_ordinal") == request_ordinal
                        )
                        and (
                            txn_ordinal is None
                            or record.get("txn_ordinal") == txn_ordinal
                        )
                    ]

                for phase in (
                    "accept",
                    "worker_dispatch",
                    "worker_spawn",
                    "provider_resolution",
                    "lockdown",
                    "instructions",
                    "delegation_context",
                    "graph_context",
                    "tool_catalog",
                    "setup_finalize",
                ):
                    actual = len(matching(phase, "daemon", "daemon_accept", 0, 0))
                    if actual != 1:
                        correctness_failures.append(
                            f"trace turn={ordinal} daemon phase={phase} expected=1 actual={actual}"
                        )
                for phase in (
                    "exec_start",
                    "profile_resolved",
                    "connected",
                    "hello_done",
                    "submitted",
                    "terminal_seen",
                    "exit",
                ):
                    actual = len(matching(phase, "client", "client_exec", 0, 0))
                    if actual != 1:
                        correctness_failures.append(
                            f"trace turn={ordinal} client phase={phase} expected=1 actual={actual}"
                        )
                read_bundle_count = len(
                    matching("read_bundle", "daemon", "daemon_accept", 0, 0)
                )
                if read_bundle_count != 2:
                    correctness_failures.append(
                        f"trace turn={ordinal} phase=read_bundle expected=2 "
                        f"actual={read_bundle_count}"
                    )
                hook_count = len(
                    matching("hooks_discovery", "daemon", "daemon_accept", 0, 0)
                )
                if hook_count > 1:
                    correctness_failures.append(
                        f"trace turn={ordinal} phase=hooks_discovery expected<=1 "
                        f"actual={hook_count}"
                    )
                setup_prompt_count = len(
                    matching("prompt_assembly", "daemon", "daemon_accept", 0, 0)
                )
                if setup_prompt_count != 1:
                    correctness_failures.append(
                        f"trace turn={ordinal} setup prompt_assembly expected=1 "
                        f"actual={setup_prompt_count}"
                    )
                for phase in (
                    "prompt_assembly",
                    "budget_estimate",
                    "request_prepare",
                    "budget_enforcement",
                    "request_attempt_commit",
                    "provider_open",
                    "first_byte",
                ):
                    phase_records = [
                        record
                        for record in matching(phase, "daemon", "daemon_accept", None, 0)
                        if int(record["request_ordinal"]) > 0
                    ]
                    ordinals = sorted(
                        record["request_ordinal"] for record in phase_records
                    )
                    if ordinals != list(range(1, expected_requests + 1)):
                        correctness_failures.append(
                            f"trace turn={ordinal} phase={phase} request_ordinals={ordinals!r}"
                        )
                journal_txns = sorted(
                    int(record["txn_ordinal"])
                    for record in matching(
                        "journal_transaction", "daemon", "daemon_accept", 0, None
                    )
                    if int(record["txn_ordinal"]) > 0
                )
                append_txns = sorted(
                    int(record["txn_ordinal"])
                    for record in matching(
                        "journal_append_wait", "daemon", "daemon_accept", 0, None
                    )
                    if int(record["txn_ordinal"]) > 0
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
                terminal_records = [
                    record
                    for record in matching(
                        "terminal_commit", "daemon", "daemon_accept", 0, None
                    )
                    if int(record["txn_ordinal"]) > 0
                ]
                if len(terminal_records) != 1:
                    correctness_failures.append(
                        f"trace turn={ordinal} phase=terminal_commit expected=1 "
                        f"actual={len(terminal_records)}"
                    )
        trace_phase_table = _build_phase_table(
            samples, grouped, correctness_failures
        )
        trace_stage_summary = {
            "aggregation": "per-turn phase sums separated by shape",
            "replacement_field": "trace_phase_table",
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
        "trace_phase_table": trace_phase_table,
        "trace_off_silent": (
            None
            if trace
            else not any("trace" in failure and "off" in failure for failure in correctness_failures)
        ),
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
    parser.add_argument("--warmups", type=int, default=WARMUPS_PER_SHAPE)
    parser.add_argument("--measured", type=int, default=MEASURED_PER_SHAPE)
    parser.add_argument("--load-limit", type=float, default=LOAD_LIMIT)
    parser.add_argument("--trace", action="store_true")
    parser.add_argument("--keep-root", action="store_true")
    parser.add_argument("--single-budget-ms", type=float)
    parser.add_argument("--tool-budget-ms", type=float)
    parser.add_argument("--commit-label")
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = _arguments(list(sys.argv[1:] if argv is None else argv))
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
        report = run_harness(
            args.bin_dir,
            warmups=args.warmups,
            measured=args.measured,
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
    if args.trace:
        print(_render_phase_table(report["trace_phase_table"]), file=sys.stderr)
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
