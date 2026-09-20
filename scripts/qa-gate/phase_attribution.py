"""Content-free phase accounting. Integer nanoseconds are authoritative.

CPU is exclusive active-poll thread time, not process CPU across an await.
Wall is a partition of observed intervals, not a causal critical-path claim:
active work wins over waits, then the most specific named phase wins. All
uncovered wall/CPU remains an explicit residual. Missing probes are not zero.
"""
from __future__ import annotations

import heapq
import json
from pathlib import Path
from typing import Any

ENV = "HAIDER_PHASE_TRACE_DIR"
# Specific work overrides enclosing transport/tool waits. Schema v2 added the
# CAS phases/counters; v3 adds the store residual split and content-free page
# counters. The reader remains compatible with both earlier trace schemas.
PHASES = (
    "client_control", "turn_control", "turn_setup", "lockdown_bind_activate", "store_access",
    "submit", "rpc", "tool_dispatch", "completion_render", "spawn",
    "runtime_init", "socket_handshake", "directory_prep", "store_open",
    "capability_catalog", "provider_assembly", "stream_decode",
    "store_other", "store_journal", "projection_digest", "store_query_point",
    "store_receipt_attempt", "store_provider_view", "store_query_replay",
    "store_query_reducer", "store_event_decode",
    "store_owner_lock_wait", "store_connection_lock_wait",
    "cas_read_hash", "cas_reverify",
)
COLD_PHASES = (
    "spawn", "dynamic_link", "runtime_init", "store_open", "directory_prep",
    "socket_handshake", "capability_catalog", "lockdown_bind_activate",
    "first_request", "teardown",
)
ALL_PHASES = (*PHASES, "teardown", "daemon_reaped_children")
COUNTERS = (
    "bytes_read", "blocks_hashed", "reverify_calls", "rows_read",
    "payload_bytes", "events_decoded",
)


def read_records(directory: Path) -> tuple[list[dict[str, Any]], list[int]]:
    records: list[dict[str, Any]] = []
    pids: list[int] = []
    for path in sorted(directory.glob("phase-*.jsonl")):
        lines = path.read_text(encoding="utf-8").splitlines()
        if not lines:
            raise ValueError(f"empty phase trace: {path.name}")
        header = json.loads(lines[0])
        schema = header.get("schema")
        if (schema not in (1, 2, 3) or header.get("dropped") != 0
                or type(header.get("records")) is not int
                or header["records"] != len(lines) - 1
                or header.get("clock") != "CLOCK_MONOTONIC"
                or header.get("cpu_clock") != "CLOCK_THREAD_CPUTIME_ID"):
            raise ValueError(f"unsupported or truncated phase trace: {path.name}")
        pid = header["pid"]
        if type(pid) is not int or pid <= 0 or path.name != f"phase-{pid}.jsonl":
            raise ValueError("invalid phase process identity")
        pids.append(pid)
        for line in lines[1:]:
            row = json.loads(line)
            if row.get("phase") not in ALL_PHASES:
                raise ValueError("unknown phase")
            if any(type(row.get(key)) is not int or row[key] < 0
                   for key in ("start_ns", "end_ns", "cpu_ns")):
                raise ValueError("invalid phase clocks")
            if row["end_ns"] < row["start_ns"] or type(row.get("waiting")) is not bool:
                raise ValueError("invalid phase interval")
            if row["waiting"] and row["cpu_ns"]:
                raise ValueError("wait interval charged CPU")
            if schema in (2, 3) and "counters" not in row:
                raise ValueError(f"phase trace v{schema} record has no counters")
            counters = row.get("counters", {})
            if not isinstance(counters, dict) or any(
                type(counters.get(key, 0)) is not int or counters.get(key, 0) < 0
                for key in COUNTERS
            ):
                raise ValueError("invalid phase counters")
            row["counters"] = {key: counters.get(key, 0) for key in COUNTERS}
            records.append({**row, "pid": pid})
    return records, pids


def partition(records: list[dict[str, Any]], *, start_ns: int, end_ns: int,
              cpu_ns: int, expected_pids: list[int], available_pids: list[int],
              cold: bool = False, reaped_children_cpu_ns: int | None = None) -> dict[str, Any]:
    if end_ns <= start_ns or cpu_ns < 0:
        raise ValueError("invalid independent measurement boundary")
    rows = {
        name: {"wall_ns": 0, "cpu_ns": 0, "records": 0, **dict.fromkeys(COUNTERS, 0)}
        for name in ALL_PHASES
    }
    events: dict[int, list[tuple[bool, int]]] = {start_ns: [], end_ns: []}
    selected: list[dict[str, Any]] = []
    observed_records: list[dict[str, Any]] = []
    client_rpc_starts = [row["start_ns"] for row in records
                         if row["pid"] == expected_pids[0] and row["phase"] == "rpc"
                         and start_ns <= row["start_ns"] < end_ns]
    request_start = min(client_rpc_starts, default=end_ns)
    request_end = min((row["start_ns"] for row in records
                       if row["pid"] == expected_pids[0] and row["phase"] == "teardown"
                       and request_start <= row["start_ns"] < end_ns), default=end_ns)
    boundary_cpu_records = 0
    boundary_counter_records = 0
    excluded_cold_request_records = 0
    for record in records:
        if record["pid"] not in expected_pids:
            continue
        a, b = max(start_ns, record["start_ns"]), min(end_ns, record["end_ns"])
        if a >= b:
            continue
        observed_records.append(record)
        if cold and record["phase"] not in COLD_PHASES:
            # Startup mutations and post-teardown output are not first-request
            # work. Keep them in residual, with their raw records retained.
            a, b = max(a, request_start), min(b, request_end)
            if a >= b:
                excluded_cold_request_records += 1
                continue
        index = len(selected)
        selected.append(record)
        events.setdefault(a, []).append((True, index))
        events.setdefault(b, []).append((False, index))
        phase = rows[record["phase"]]
        phase["records"] += 1
        counters = record.get("counters", {})
        # Never prorate CPU across a sample boundary: unknown distribution.
        if record["start_ns"] >= a and record["end_ns"] <= b:
            phase["cpu_ns"] += record["cpu_ns"]
            for key in COUNTERS:
                phase[key] += counters.get(key, 0)
        else:
            if record["cpu_ns"]:
                boundary_cpu_records += 1
            if any(counters.values()):
                boundary_counter_records += 1
    active: set[int] = set()
    heap: list[tuple[int, int, int]] = []
    prior = start_ns
    wall_residual = 0
    for at, changes in sorted(events.items()):
        while heap and heap[0][2] not in active:
            heapq.heappop(heap)
        elapsed = at - prior
        if heap:
            rows[selected[heap[0][2]]["phase"]]["wall_ns"] += elapsed
        else:
            wall_residual += elapsed
        for entering, index in changes:
            if entering:
                active.add(index)
                row = selected[index]
                heapq.heappush(heap, (int(row["waiting"]), -ALL_PHASES.index(row["phase"]), index))
            else:
                active.remove(index)
        prior = at
    if reaped_children_cpu_ns is not None:
        if reaped_children_cpu_ns < 0:
            raise ValueError("negative reaped child CPU")
        rows["daemon_reaped_children"].update(cpu_ns=reaped_children_cpu_ns, records=1)
    accounted_cpu = sum(row["cpu_ns"] for row in rows.values())
    if accounted_cpu > cpu_ns:
        raise ValueError(f"phase CPU exceeds independent total: {accounted_cpu} > {cpu_ns}")
    residual = {"wall_ns": wall_residual, "cpu_ns": cpu_ns - accounted_cpu}
    assert sum(row["wall_ns"] for row in rows.values()) + residual["wall_ns"] == end_ns - start_ns
    detail = {name: ({**row, "coverage": "observed_scopes"} if row["records"] else
                    {"wall_ns": None, "cpu_ns": None, "records": 0,
                     **dict.fromkeys(COUNTERS, 0), "coverage": "unobserved"})
              for name, row in rows.items()}
    if reaped_children_cpu_ns is not None:
        detail["daemon_reaped_children"]["coverage"] = "cpu_counter_only; no wall allocation"
    phases = detail
    if cold:
        phases = {name: detail.get(name, {"wall_ns": None, "cpu_ns": None, "records": 0,
                                        **dict.fromkeys(COUNTERS, 0),
                                        "coverage": "unavailable_before_main"})
                  for name in COLD_PHASES if name != "first_request"}
        request = [row for name, row in rows.items() if name not in COLD_PHASES]
        phases["first_request"] = {key: sum(row[key] for row in request)
                                   for key in ("wall_ns", "cpu_ns", "records", *COUNTERS)}
        if not phases["first_request"]["records"]:
            phases["first_request"].update(wall_ns=None, cpu_ns=None)
        phases["first_request"]["coverage"] = "observed_constituents_between_first_client_rpc_and_teardown"
    for key, total in (("wall_ns", end_ns - start_ns), ("cpu_ns", cpu_ns)):
        if sum(row[key] or 0 for row in phases.values()) + residual[key] != total:
            raise ValueError(f"phase rollup does not reconcile: {key}")
    missing = sorted(set(expected_pids) - set(available_pids))
    return {
        "schema": "haider.phase_attribution.v2",
        "status": "missing_process_traces" if missing else "partial_attribution",
        "wall_policy": "active_before_wait_then_phase_priority_v2",
        "wall_priority_low_to_high": list(ALL_PHASES),
        "cpu_policy": "exclusive_thread_cpu_per_active_poll; separate_daemon_reaped_child_counter",
        "total": {"wall_ns": end_ns - start_ns, "cpu_ns": cpu_ns},
        "phases": phases, "detail": detail, "residual": residual,
        "residual_includes": ["pre-main and dynamic link", "uninstrumented work",
                              "trace bookkeeping and flush", "child CPU when no independent counter is available"],
        "missing_pids": missing, "boundary_cpu_records": boundary_cpu_records,
        "boundary_counter_records": boundary_counter_records,
        "cold_request_boundary_ns": None if not cold or not client_rpc_starts else
            {"start": request_start, "end": request_end},
        "excluded_cold_request_records": excluded_cold_request_records,
        "records": observed_records,
    }


def summarize(maps: list[dict[str, Any]]) -> dict[str, Any]:
    """Use pooled sums/means, since independent phase medians do not add up."""
    if not maps:
        return {"count": 0}
    names = maps[0]["phases"]
    def sum_fields(rows: list[dict[str, Any]], fields: tuple[str, ...]) -> dict[str, int]:
        return {key: sum(row.get(key, 0) or 0 for row in rows) for key in fields}
    return {
        "count": len(maps), "aggregation": "pooled_ns; divide by count for additive mean",
        "total": sum_fields([row["total"] for row in maps], ("wall_ns", "cpu_ns")),
        "phases": {name: {
            **(sum_fields([row["phases"][name] for row in maps],
                          ("wall_ns", "cpu_ns", *COUNTERS))
               if any(row["phases"][name]["records"] for row in maps)
               else {"wall_ns": None, "cpu_ns": None, **dict.fromkeys(COUNTERS, 0)}),
            "observed_samples": sum(row["phases"][name]["records"] > 0 for row in maps)}
            for name in names},
        "residual": sum_fields([row["residual"] for row in maps], ("wall_ns", "cpu_ns")),
    }
