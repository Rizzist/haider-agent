#!/usr/bin/env python3
"""Deterministic 40-turn resident-session performance fixture.

The fixture grows one native session through text and process-tool turns.  It
uses only a loopback provider, verifies every external tool effect, fragments
both text and tool-call streams, and records exact phase attribution at the
depths that gate the v0.0.972 wall-time lanes.
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass
from datetime import datetime, timezone
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import math
import os
from pathlib import Path
import shutil
import statistics
import subprocess
import sys
import tempfile
import threading
import time
from typing import Any, Mapping, Sequence

import phase_attribution as attribution
from turnperf_support import (
    MODEL_ID,
    PROVIDER_ID,
    ProofError,
    ThrowawayProfile,
    chat_chunk,
    cpu_accounting_self_check,
    load_one_minute,
    median_mad,
    parse_single_json,
    process_cpu_times,
    process_peak_rss_kib,
    process_rss_kib,
    profile_daemon_pid,
    profile_lock_is_free,
    runtime_socket_paths,
    select_exec_tool,
    sha256_file,
    validate_jsonl,
    wait_pid_gone,
    wait_session_idle,
)


TURNS = 40
CHECKPOINTS = (1, 5, 10, 20, 30, 40)
TOOL_TURNS = tuple(range(4, TURNS + 1, 4))
# Keep the gate's depth-40 turn a small process call so WALL-3/4 are not
# conflated with the deliberately large capture.  Three earlier calls still
# exercise and retain real CAS-backed transcript spills in the grown history.
SPILL_TURNS = (8, 24, 36)
PROVIDER_COUNT = 24
MODELS_PER_PROVIDER = 8
ABBA = ("a", "b", "b", "a")
QUIET_REQUEST = Path("/Users/rizzist/Developer/haiderharness/state/quiet-request")
QUIET_GRANTED = Path("/Users/rizzist/Developer/haiderharness/state/quiet-granted")
STRICT_LOAD_LIMIT = 2.0
T95 = {
    1: 12.706,
    2: 4.303,
    3: 3.182,
    4: 2.776,
    5: 2.571,
    6: 2.447,
    7: 2.365,
    8: 2.306,
    9: 2.262,
    10: 2.228,
}


def abba_order(rounds: int) -> list[tuple[int, int, str]]:
    if rounds < 1:
        raise ProofError("ABBA rounds must be positive")
    return [
        (block, position, arm)
        for block in range(1, rounds + 1)
        for position, arm in enumerate(ABBA, start=1)
    ]


def turn_is_tool(turn: int) -> bool:
    return turn in TOOL_TURNS


def turn_is_spill(turn: int) -> bool:
    return turn in SPILL_TURNS


def turn_prompt(turn: int) -> str:
    marker = " ".join(f"context-{index:02d}" for index in range(48))
    return (
        f"Deep fixture turn {turn:02d}. Preserve all prior constraints, inspect the "
        f"accumulated transcript, and return the deterministic response. {marker}"
    )


def effect_token(turn: int) -> str:
    return f"deep-effect-turn-{turn:02d}"


def process_command(turn: int) -> str:
    prefix = f"printf '%s\\n' '{effect_token(turn)}' >> deep-effects.log; "
    if turn_is_spill(turn):
        return prefix + (
            "awk 'BEGIN { for (i=0; i<6000; i++) "
            "printf \"deep-output-%05d-xxxxxxxx\\n\", i }'"
        )
    return prefix + f"printf '%s\\n' 'deep-tool-output-turn-{turn:02d}'"


def fragment_bytes(payload: bytes) -> list[bytes]:
    """Split every SSE payload across a deterministic repeating byte pattern."""
    widths = (1, 2, 3, 5, 8, 13, 21)
    fragments: list[bytes] = []
    offset = 0
    index = 0
    while offset < len(payload):
        width = widths[index % len(widths)]
        fragments.append(payload[offset : offset + width])
        offset += width
        index += 1
    return fragments


def _argument_fragments(arguments: str) -> list[str]:
    widths = (7, 11, 5, 13)
    fragments: list[str] = []
    offset = 0
    index = 0
    while offset < len(arguments):
        width = widths[index % len(widths)]
        fragments.append(arguments[offset : offset + width])
        offset += width
        index += 1
    return fragments


def text_response(turn: int) -> list[bytes]:
    pieces = [
        f"deep-{turn:02d}-{index:02d}: deterministic transcript growth; "
        for index in range(24)
    ]
    return [
        chat_chunk(MODEL_ID, {"role": "assistant"}),
        *(chat_chunk(MODEL_ID, {"content": piece}) for piece in pieces),
        chat_chunk(MODEL_ID, {}, "stop"),
        b"data: [DONE]\n\n",
    ]


def tool_response(turn: int, body: Mapping[str, Any]) -> list[bytes]:
    name, arguments = select_exec_tool(body, process_command(turn))
    encoded = json.dumps(arguments, separators=(",", ":"))
    fragments = _argument_fragments(encoded)
    call_id = f"deep-call-{turn:02d}"
    first = {
        "index": 0,
        "id": call_id,
        "type": "function",
        "function": {"name": name, "arguments": fragments[0]},
    }
    continuation = [
        {
            "index": 0,
            "function": {"arguments": fragment},
        }
        for fragment in fragments[1:]
    ]
    return [
        chat_chunk(MODEL_ID, {"role": "assistant"}),
        chat_chunk(MODEL_ID, {"tool_calls": [first]}),
        *(chat_chunk(MODEL_ID, {"tool_calls": [part]}) for part in continuation),
        chat_chunk(MODEL_ID, {}, "tool_calls"),
        b"data: [DONE]\n\n",
    ]


def provider_catalog(base_url: str) -> dict[str, Any]:
    providers = []
    for provider_index in range(PROVIDER_COUNT):
        provider_id = PROVIDER_ID if provider_index == 0 else f"deep-provider-{provider_index:02d}"
        models = [
            MODEL_ID if provider_index == 0 and model_index == 0
            else f"deep-model-{provider_index:02d}-{model_index:02d}"
            for model_index in range(MODELS_PER_PROVIDER)
        ]
        providers.append(
            {
                "provider_id": provider_id,
                "display_name": f"deep fixture provider {provider_index:02d}",
                "api_family": "openai_chat_completions",
                "base_url": base_url,
                "enabled": True,
                "auth_requirement": "none",
                "configured_models": models,
                "default_model": models[0],
                "provenance": "custom",
            }
        )
    return {"providers": providers}


def parse_turn_header(value: str | None) -> tuple[str, str, int, int]:
    if not value:
        raise ProofError("deep provider request is missing X-Haider-Turn")
    parts = value.split("/")
    if len(parts) != 4 or not parts[0] or not parts[1]:
        raise ProofError(f"invalid X-Haider-Turn value {value!r}")
    try:
        turn, request = int(parts[2]), int(parts[3])
    except ValueError as error:
        raise ProofError(f"invalid X-Haider-Turn ordinals {value!r}") from error
    if turn < 1 or request < 1:
        raise ProofError(f"non-positive X-Haider-Turn ordinals {value!r}")
    return parts[0], parts[1], turn, request


class DeepProviderState:
    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._entries: list[dict[str, Any]] = []
        self._active = 0
        self._condition = threading.Condition(self._lock)

    def enter(self) -> None:
        with self._condition:
            self._active += 1

    def leave(self) -> None:
        with self._condition:
            self._active -= 1
            self._condition.notify_all()

    def record(
        self,
        *,
        raw: bytes,
        body: Mapping[str, Any],
        turn_header: str | None,
        decode_micros: int,
    ) -> int:
        session_id, run_id, turn, request = parse_turn_header(turn_header)
        messages = body.get("messages")
        tools = body.get("tools")
        entry = {
            "session_id": session_id,
            "run_id": run_id,
            "turn_ordinal": turn,
            "request_ordinal": request,
            "request_kind": "primary",
            "body_bytes": len(raw),
            "body_sha256": hashlib.sha256(raw).hexdigest(),
            "messages": len(messages) if isinstance(messages, list) else 0,
            "tools": len(tools) if isinstance(tools, list) else 0,
            "decode_micros": decode_micros,
            "handler_micros": None,
        }
        with self._condition:
            self._entries.append(entry)
            return len(self._entries) - 1

    def finish(self, index: int, handler_micros: int) -> None:
        with self._condition:
            self._entries[index]["handler_micros"] = handler_micros

    def turn(self, turn: int) -> list[dict[str, Any]]:
        with self._condition:
            return [dict(row) for row in self._entries if row["turn_ordinal"] == turn]

    def snapshot(self) -> list[dict[str, Any]]:
        with self._condition:
            return [dict(row) for row in self._entries]

    def wait_idle(self, timeout: float) -> bool:
        deadline = time.monotonic() + timeout
        with self._condition:
            while self._active and time.monotonic() < deadline:
                self._condition.wait(timeout=min(0.1, deadline - time.monotonic()))
            return self._active == 0


class _DeepHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    state: DeepProviderState

    def log_message(self, _format: str, *_args: object) -> None:
        return

    def do_POST(self) -> None:  # noqa: N802
        self.state.enter()
        started = time.perf_counter_ns()
        entry_index: int | None = None
        try:
            length = int(self.headers.get("Content-Length", "0"))
            raw = self.rfile.read(max(0, length))
            decode_started = time.perf_counter_ns()
            decoded = json.loads(raw)
            decode_micros = (time.perf_counter_ns() - decode_started) // 1_000
            body = decoded if isinstance(decoded, Mapping) else {}
            entry_index = self.state.record(
                raw=raw,
                body=body,
                turn_header=self.headers.get("X-Haider-Turn"),
                decode_micros=decode_micros,
            )
            _session, _run, turn, request = parse_turn_header(
                self.headers.get("X-Haider-Turn")
            )
            chunks = (
                tool_response(turn, body)
                if turn_is_tool(turn) and request == 1
                else text_response(turn)
            )
            payload = b"".join(chunks)
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Cache-Control", "no-cache")
            self.send_header("Content-Length", str(len(payload)))
            self.send_header("Connection", "keep-alive")
            self.end_headers()
            for fragment in fragment_bytes(payload):
                self.wfile.write(fragment)
                self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            return
        finally:
            if entry_index is not None:
                self.state.finish(
                    entry_index, (time.perf_counter_ns() - started) // 1_000
                )
            self.state.leave()

    def do_GET(self) -> None:  # noqa: N802
        if self.path.rstrip("/").endswith("/models"):
            payload = json.dumps(
                {
                    "object": "list",
                    "data": [
                        {"id": f"deep-model-00-{index:02d}", "object": "model"}
                        for index in range(MODELS_PER_PROVIDER)
                    ],
                },
                separators=(",", ":"),
            ).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
            return
        self.send_error(404)


class _DeepServer(ThreadingHTTPServer):
    def handle_error(self, _request: object, _client_address: object) -> None:
        return


class DeepProvider:
    def __init__(self) -> None:
        self.state = DeepProviderState()
        handler = type("DeepTurnHandler", (_DeepHandler,), {"state": self.state})
        self.server = _DeepServer(("127.0.0.1", 0), handler)
        self.server.daemon_threads = True
        self.server.block_on_close = False
        self.thread = threading.Thread(
            target=self.server.serve_forever,
            name="deep-turn-provider",
            daemon=True,
        )

    @property
    def base_url(self) -> str:
        host, port = self.server.server_address[:2]
        return f"http://{host}:{port}/v1"

    def __enter__(self) -> "DeepProvider":
        self.thread.start()
        return self

    def __exit__(self, *_args: object) -> None:
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=2)


def parse_store_records(text: str) -> list[dict[str, int]]:
    records: list[dict[str, int]] = []
    for line in text.splitlines():
        if "target=haider.store" not in line:
            continue
        values: dict[str, int] = {}
        for token in line.split():
            if token.startswith("queue_wait_micros=") or token.startswith(
                "operation_micros="
            ):
                key, value = token.split("=", 1)
                try:
                    values[key] = int(value)
                except ValueError:
                    pass
        if set(values) == {"queue_wait_micros", "operation_micros"}:
            records.append(values)
    return records


def _daemon_log_from(profile: ThrowawayProfile, offset: int) -> tuple[str, int]:
    path = profile.profile / "daemon.log"
    try:
        with path.open("rb") as handle:
            handle.seek(offset)
            value = handle.read()
            return value.decode("utf-8", errors="replace"), handle.tell()
    except FileNotFoundError:
        return "", offset


def _assert_quiet(label: str, required: bool) -> float:
    load = load_one_minute()
    if not required:
        return load
    if not QUIET_REQUEST.exists():
        raise ProofError(f"{label}: quiet request is absent")
    if not QUIET_GRANTED.exists():
        raise ProofError(f"{label}: quiet grant is absent")
    if load >= STRICT_LOAD_LIMIT:
        raise ProofError(
            f"{label}: load1={load:.3f} is not below {STRICT_LOAD_LIMIT:.1f}"
        )
    return load


def _git_commit() -> str | None:
    result = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        timeout=2,
        check=False,
    )
    value = result.stdout.strip()
    return value if result.returncode == 0 and value else None


def _initial_arguments() -> list[str]:
    return [
        "run",
        "-p",
        turn_prompt(1),
        "--provider",
        PROVIDER_ID,
        "--model",
        MODEL_ID,
        "--output",
        "jsonl",
        "--timeout",
        "30s",
    ]


def _continuation_arguments(session_id: str, turn: int) -> list[str]:
    return [
        "run",
        "--session",
        session_id,
        "-p",
        turn_prompt(turn),
        "--output",
        "jsonl",
        "--timeout",
        "30s",
    ]


def _effect_count(root: Path, token: str) -> int:
    logs = list(root.rglob("deep-effects.log"))
    if len(logs) != 1:
        return 0
    return logs[0].read_text(encoding="utf-8").splitlines().count(token)


def _pre_stop(profile: ThrowawayProfile) -> dict[str, Any]:
    stop = profile.stop()
    initialized_for_stop = False
    if stop.returncode == 70:
        # A never-opened profile has no durable identity for the read-only
        # stop resolver.  Initialize this isolated profile once, then exercise
        # the real profile-scoped stop before starting the measured daemon.
        profile.ready()
        owner = profile_daemon_pid(profile.profile)
        if owner is None:
            raise ProofError("isolated pre-stop initialization produced no owner")
        stop = profile.stop()
        if not wait_pid_gone(owner, 5):
            raise ProofError(f"isolated pre-stop daemon pid {owner} survived stop")
        initialized_for_stop = True
    try:
        document = parse_single_json(stop.stdout, "deep fixture pre-stop")
    except ProofError:
        document = {}
    expected = "stopped_cleanly" if initialized_for_stop else "not_running"
    expected_code = 0 if initialized_for_stop else 69
    if stop.returncode != expected_code or document.get("outcome") != expected:
        raise ProofError(
            f"isolated pre-stop expected {expected}/{expected_code}, "
            f"actual={stop.returncode}/{document}; stderr={stop.stderr[-300:]!r}"
        )
    if profile_daemon_pid(profile.profile) is not None:
        raise ProofError("isolated pre-stop left a daemon owner")
    if not profile_lock_is_free(profile.profile):
        raise ProofError("isolated pre-stop found the profile lock held")
    sockets = runtime_socket_paths(profile.runtime)
    if sockets:
        raise ProofError(f"isolated pre-stop found runtime sockets: {sockets}")
    return {
        "returncode": stop.returncode,
        "outcome": document.get("outcome"),
        "initialized_for_stop": initialized_for_stop,
    }


def _provider_catalog_check(
    profile: ThrowawayProfile, overrides: Mapping[str, str | None]
) -> dict[str, Any]:
    result = profile.command(
        ["provider", "list", "--json"], timeout=20, overrides=overrides
    )
    if result.returncode != 0:
        raise ProofError(
            f"provider catalog check failed exit={result.returncode}: {result.stderr[-300:]}"
        )
    try:
        document = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise ProofError(f"provider catalog is invalid JSON: {error}") from error
    if not isinstance(document, Mapping):
        raise ProofError("provider catalog JSON is not an object")
    providers = document.get("providers")
    if not isinstance(providers, list):
        raise ProofError("provider catalog has no providers array")
    names = {
        row.get("provider", row.get("provider_id", row.get("name")))
        for row in providers
        if isinstance(row, Mapping)
    }
    expected = {
        PROVIDER_ID,
        *(f"deep-provider-{index:02d}" for index in range(1, PROVIDER_COUNT)),
    }
    missing = sorted(expected - names)
    if missing:
        raise ProofError(f"daemon catalog omitted deep providers: {missing}")
    return {"reported_count": len(providers), "deep_count": len(expected)}


def _phase_file_evidence(directory: Path) -> list[dict[str, Any]]:
    evidence = []
    for path in sorted(directory.glob("phase-*.jsonl")):
        header = json.loads(path.read_text(encoding="utf-8").splitlines()[0])
        evidence.append(
            {
                "name": path.name,
                "bytes": path.stat().st_size,
                "sha256": sha256_file(path),
                "records": header.get("records"),
                "dropped": header.get("dropped"),
            }
        )
    return evidence


@dataclass(frozen=True)
class Arm:
    name: str
    bin_dir: Path
    environment: dict[str, str]
    description: str


def run_fixture(
    arm: Arm,
    *,
    block: int,
    position: int,
    require_quiet: bool,
    keep_root: bool,
) -> dict[str, Any]:
    label = f"block-{block:02d}-position-{position}-{arm.name}"
    load_start = _assert_quiet(label, require_quiet)
    # Unix-domain endpoints have a 103-byte macOS ceiling.  Keep the physical
    # root short; the descriptive label remains in the report.
    root = Path(tempfile.mkdtemp(prefix="hdp-", dir="/tmp"))
    phase_dir = root / "phase-traces"
    phase_dir.mkdir()
    profile: ThrowawayProfile | None = None
    stopped = False
    try:
        with DeepProvider() as provider:
            profile = ThrowawayProfile(
                arm.bin_dir, provider.base_url, root=root / "x"
            )
            catalog = provider_catalog(provider.base_url)
            (profile.profile / "providers.json").write_text(
                json.dumps(catalog, separators=(",", ":")) + "\n",
                encoding="utf-8",
            )
            os.chmod(profile.profile / "providers.json", 0o600)
            pre_stop = _pre_stop(profile)
            overrides: dict[str, str | None] = {
                attribution.ENV: str(phase_dir),
                "HAIDER_DAEMON_TRACE": arm.environment.get("HAIDER_DAEMON_TRACE"),
                "HAIDER_WIRE_MSGPACK": arm.environment.get("HAIDER_WIRE_MSGPACK"),
            }
            profile.ready(overrides)
            pid, generation, _status = profile.status()
            catalog_check = _provider_catalog_check(profile, overrides)
            daemon_peak = process_peak_rss_kib(pid)
            session_id: str | None = None
            _startup_log, log_offset = _daemon_log_from(profile, 0)
            rows: list[dict[str, Any]] = []
            turn_loads: list[float] = []
            for turn in range(1, TURNS + 1):
                turn_loads.append(_assert_quiet(f"{label}/turn-{turn:02d}", require_quiet))
                # Status/idle checks are correctness work outside the measured
                # command boundary.  Discard their store events before taking
                # this turn's trace slice.
                _between_turn_log, log_offset = _daemon_log_from(profile, log_offset)
                daemon_before = process_cpu_times(pid)
                arguments = (
                    _initial_arguments()
                    if session_id is None
                    else _continuation_arguments(session_id, turn)
                )
                result = profile.command(
                    arguments,
                    timeout=90,
                    overrides=overrides,
                    observe_pid=pid,
                )
                daemon_after = process_cpu_times(pid)
                trace_text, log_offset = _daemon_log_from(profile, log_offset)
                if result.timed_out or result.returncode != 0:
                    raise ProofError(
                        f"{label}/turn-{turn:02d} failed exit={result.returncode} "
                        f"timeout={result.timed_out}: {result.stderr[-500:]}"
                    )
                try:
                    parsed = validate_jsonl(
                        result.stdout,
                        "deep-tool" if turn_is_tool(turn) else "deep-text",
                        continuation=turn > 1,
                    )
                except ProofError as error:
                    raise ProofError(
                        f"{error}; first JSONL records={result.stdout.splitlines()[:3]!r}"
                    ) from error
                if session_id is None:
                    session_id = parsed["session_id"]
                elif parsed["session_id"] != session_id:
                    raise ProofError(
                        f"session identity changed {session_id!r} -> {parsed['session_id']!r}"
                    )
                wait_session_idle(profile, session_id)
                if not provider.state.wait_idle(2):
                    raise ProofError(f"{label}/turn-{turn:02d} provider did not settle")
                requests = provider.state.turn(turn)
                expected_requests = 2 if turn_is_tool(turn) else 1
                if len(requests) != expected_requests:
                    raise ProofError(
                        f"turn {turn} provider requests expected={expected_requests} "
                        f"actual={len(requests)}"
                    )
                if [row["request_ordinal"] for row in requests] != list(
                    range(1, expected_requests + 1)
                ):
                    raise ProofError(f"turn {turn} provider request ordinals are not contiguous")
                if any(row["session_id"] != session_id for row in requests):
                    raise ProofError(f"turn {turn} provider session identity differs")
                if turn_is_tool(turn):
                    actual = _effect_count(profile.root, effect_token(turn))
                    if actual != 1:
                        raise ProofError(
                            f"turn {turn} tool effect expected=1 actual={actual}"
                        )
                daemon_cpu = daemon_after.delta(daemon_before)
                daemon_peak = max(daemon_peak, result.observed_peak_rss_kib)
                store_records = parse_store_records(trace_text)
                row: dict[str, Any] = {
                    "turn": turn,
                    "shape": "spill_tool"
                    if turn_is_spill(turn)
                    else ("tool" if turn_is_tool(turn) else "text"),
                    "wall_ms": result.wall_ms,
                    "client_cpu_ms": result.cpu_ms,
                    "daemon_cpu_ms": daemon_cpu.self_ms,
                    "daemon_reaped_children_cpu_ms": daemon_cpu.reaped_children_ms,
                    "combined_cpu_ms": (
                        result.cpu_ms
                        + daemon_cpu.self_ms
                        + daemon_cpu.reaped_children_ms
                    ),
                    "client_peak_rss_kib": result.child_peak_rss_kib,
                    "daemon_rss_kib": process_rss_kib(pid),
                    "daemon_peak_rss_kib": result.observed_peak_rss_kib,
                    "provider_requests": requests,
                    "terminal_kind": parsed["terminal_kind"],
                    "terminal_seq": parsed["terminal_seq"],
                    "run_id": parsed["run_id"],
                    "store_trace": {
                        "count": len(store_records),
                        "queue_wait_micros": [
                            value["queue_wait_micros"] for value in store_records
                        ],
                        "operation_micros": [
                            value["operation_micros"] for value in store_records
                        ],
                    },
                    "phase_boundary": {
                        "start_ns": result.started_clock_ns,
                        "end_ns": result.ended_clock_ns,
                        "cpu_ns": round(
                            (
                                result.cpu_ms
                                + daemon_cpu.self_ms
                                + daemon_cpu.reaped_children_ms
                            )
                            * 1_000_000
                        ),
                        "expected_pids": [result.client_pid, pid],
                        "reaped_children_cpu_ns": round(
                            daemon_cpu.reaped_children_ms * 1_000_000
                        ),
                    },
                }
                rows.append(row)
                if turn in CHECKPOINTS:
                    actual_pid, actual_generation, _ = profile.status()
                    if (actual_pid, actual_generation) != (pid, generation):
                        raise ProofError(
                            f"daemon identity changed {(pid, generation)} -> "
                            f"{(actual_pid, actual_generation)}"
                        )

            checkpoint_sizes = [
                rows[depth - 1]["provider_requests"][0]["body_bytes"]
                for depth in CHECKPOINTS
            ]
            if any(after <= before for before, after in zip(checkpoint_sizes, checkpoint_sizes[1:])):
                raise ProofError(
                    f"provider transcript did not grow at every checkpoint: {checkpoint_sizes}"
                )
            if checkpoint_sizes[-1] < checkpoint_sizes[0] * 2:
                raise ProofError(
                    f"depth-40 provider body did not materially grow: {checkpoint_sizes}"
                )
            stop = profile.stop()
            try:
                stop_document = parse_single_json(stop.stdout, "deep fixture stop")
            except ProofError:
                stop_document = {}
            if stop.returncode != 0 or stop_document.get("outcome") != "stopped_cleanly":
                raise ProofError(
                    f"owned daemon stop failed exit={stop.returncode}: {stop_document}"
                )
            if not wait_pid_gone(pid, 5):
                raise ProofError(f"owned daemon pid {pid} survived stop")
            stopped = True
            phase_records, phase_pids = attribution.read_records(phase_dir)
            for row in rows:
                phase_map = attribution.partition(
                    phase_records,
                    **row.pop("phase_boundary"),
                    available_pids=phase_pids,
                )
                for key in ("wall_ns", "cpu_ns"):
                    reconciled = sum(
                        phase[key] or 0 for phase in phase_map["phases"].values()
                    ) + phase_map["residual"][key]
                    if reconciled != phase_map["total"][key]:
                        raise ProofError(
                            f"turn {row['turn']} phase {key} does not reconcile"
                        )
                phase_map["selected_record_count"] = len(phase_map.pop("records"))
                row["phase_attribution"] = phase_map
            load_end = _assert_quiet(label + "/end", require_quiet)
            provider_entries = provider.state.snapshot()
            if len(provider_entries) != TURNS + len(TOOL_TURNS):
                raise ProofError(
                    f"provider ledger count expected={TURNS + len(TOOL_TURNS)} "
                    f"actual={len(provider_entries)}"
                )
            report = {
                "label": label,
                "arm": arm.name,
                "block": block,
                "position": position,
                "description": arm.description,
                "environment": arm.environment,
                "binary_dir": str(arm.bin_dir.resolve()),
                "binaries": {
                    name: sha256_file(arm.bin_dir / name) for name in ("haider", "haiderd")
                },
                "pre_stop": pre_stop,
                "daemon": {
                    "pid": pid,
                    "generation": generation,
                    "peak_rss_kib": daemon_peak,
                    "stop": {
                        "returncode": stop.returncode,
                        "outcome": stop_document.get("outcome"),
                    },
                },
                "catalog": catalog_check,
                "session_id": session_id,
                "load_one_minute": {
                    "start": load_start,
                    "turn_max": max(turn_loads),
                    "end": load_end,
                },
                "turns": rows,
                "provider_ledger": provider_entries,
                "checkpoint_request_body_bytes": dict(zip(CHECKPOINTS, checkpoint_sizes)),
                "phase_files": _phase_file_evidence(phase_dir),
                "phase_reconciliation": "exact integer nanoseconds for every turn",
            }
            if keep_root:
                report["retained_root"] = str(root)
            return report
    finally:
        if profile is not None:
            if not stopped:
                try:
                    profile.stop()
                except (OSError, ProofError):
                    pass
            profile.dispose(remove_root=not keep_root)
        if not keep_root:
            shutil.rmtree(root, ignore_errors=True)


def _phase_summary(rows: Sequence[dict[str, Any]]) -> dict[str, Any]:
    summary = attribution.summarize([row["phase_attribution"] for row in rows])
    count = summary["count"]
    return {
        "count": count,
        "aggregation": summary["aggregation"],
        "total_mean_ms": {
            key.removesuffix("_ns"): value / count / 1_000_000
            for key, value in summary["total"].items()
        },
        "phases_mean_ms": {
            name: {
                key.removesuffix("_ns"): (
                    None if value is None else value / count / 1_000_000
                )
                for key, value in phase.items()
                if key in ("wall_ns", "cpu_ns")
            }
            for name, phase in summary["phases"].items()
        },
        "residual_mean_ms": {
            key.removesuffix("_ns"): value / count / 1_000_000
            for key, value in summary["residual"].items()
        },
    }


def _metric_summary(values: Sequence[float]) -> dict[str, float | int]:
    median, mad = median_mad(values)
    return {"count": len(values), "median": median, "mad": mad}


def summarize_arms(runs: Sequence[dict[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for arm in sorted({str(run["arm"]) for run in runs}):
        arm_runs = [run for run in runs if run["arm"] == arm]
        depths: dict[str, Any] = {}
        for depth in CHECKPOINTS:
            rows = [run["turns"][depth - 1] for run in arm_runs]
            store_queue = [
                value
                for row in rows
                for value in row["store_trace"]["queue_wait_micros"]
            ]
            store_operation = [
                value
                for row in rows
                for value in row["store_trace"]["operation_micros"]
            ]
            provider_first = [row["provider_requests"][0] for row in rows]
            depths[str(depth)] = {
                "wall_ms": _metric_summary([row["wall_ms"] for row in rows]),
                "combined_cpu_ms": _metric_summary(
                    [row["combined_cpu_ms"] for row in rows]
                ),
                "request_body_bytes": _metric_summary(
                    [float(row["body_bytes"]) for row in provider_first]
                ),
                "provider_decode_micros": _metric_summary(
                    [float(row["decode_micros"]) for row in provider_first]
                ),
                "provider_handler_micros": _metric_summary(
                    [float(row["handler_micros"]) for row in provider_first]
                ),
                "store_trace": {
                    "records": len(store_queue),
                    "queue_wait_micros": _metric_summary(store_queue)
                    if store_queue
                    else None,
                    "operation_micros": _metric_summary(store_operation)
                    if store_operation
                    else None,
                    "queue_wait_sum_micros_per_turn": (
                        sum(store_queue) / len(rows) if store_queue else None
                    ),
                    "operation_sum_micros_per_turn": (
                        sum(store_operation) / len(rows) if store_operation else None
                    ),
                },
                "phase_attribution": _phase_summary(rows),
            }
        result[arm] = {"runs": len(arm_runs), "depths": depths}
    return result


def _student_interval(values: Sequence[float]) -> dict[str, Any]:
    if not values:
        return {"count": 0, "mean": None, "interval_95": None}
    mean = statistics.mean(values)
    if len(values) < 2 or len(values) - 1 not in T95:
        return {"count": len(values), "mean": mean, "interval_95": None}
    half = T95[len(values) - 1] * statistics.stdev(values) / math.sqrt(len(values))
    return {
        "count": len(values),
        "mean": mean,
        "interval_95": [mean - half, mean + half],
    }


def summarize_contrasts(runs: Sequence[dict[str, Any]]) -> dict[str, Any] | None:
    if {run["arm"] for run in runs} != {"a", "b"}:
        return None
    result: dict[str, Any] = {}
    blocks = sorted({int(run["block"]) for run in runs})
    for depth in CHECKPOINTS:
        metrics: dict[str, Any] = {}
        for metric in ("wall_ms", "combined_cpu_ms"):
            arm_values = {
                arm: [
                    run["turns"][depth - 1][metric]
                    for run in runs
                    if run["arm"] == arm
                ]
                for arm in ("a", "b")
            }
            medians = {
                arm: statistics.median(values) for arm, values in arm_values.items()
            }
            block_contrasts = []
            for block in blocks:
                block_values = {
                    arm: [
                        run["turns"][depth - 1][metric]
                        for run in runs
                        if run["block"] == block and run["arm"] == arm
                    ]
                    for arm in ("a", "b")
                }
                if not all(block_values.values()):
                    continue
                block_contrasts.append(
                    statistics.median(block_values["b"])
                    - statistics.median(block_values["a"])
                )
            delta = medians["b"] - medians["a"]
            metrics[metric] = {
                "a_median": medians["a"],
                "b_median": medians["b"],
                "b_minus_a": delta,
                "percent": delta / medians["a"] * 100,
                "block_contrasts": block_contrasts,
                "block_mean_interval": _student_interval(block_contrasts),
            }
        result[str(depth)] = metrics
    return result


def arms_for(args: argparse.Namespace) -> tuple[Arm, Arm | None]:
    base = args.bin_dir.resolve()
    if args.experiment == "baseline":
        return Arm("a", base, {}, "instrumented deep baseline"), None
    if args.experiment == "free1-store-trace":
        return (
            Arm("a", base, {}, "safe timing trace disabled"),
            Arm(
                "b",
                base,
                {"HAIDER_DAEMON_TRACE": "1"},
                "safe timing trace enabled",
            ),
        )
    if args.experiment == "free2-msgpack":
        return (
            Arm("a", base, {}, "JSON wire"),
            Arm(
                "b",
                base,
                {"HAIDER_WIRE_MSGPACK": "1"},
                "negotiated MessagePack wire",
            ),
        )
    if args.experiment == "free3-mimalloc":
        if args.variant_bin_dir is None:
            raise ProofError("free3-mimalloc requires --variant-bin-dir")
        return (
            Arm("a", base, {}, "system allocator"),
            Arm("b", args.variant_bin_dir.resolve(), {}, "haider-daemond/mimalloc"),
        )
    raise ProofError(f"unknown experiment {args.experiment!r}")


def self_check() -> dict[str, Any]:
    payload = b"".join(text_response(1))
    fragments = fragment_bytes(payload)
    catalog = provider_catalog("http://127.0.0.1:1/v1")
    checks = {
        "turns": TURNS,
        "checkpoints": list(CHECKPOINTS),
        "tool_turns": list(TOOL_TURNS),
        "spill_turns": list(SPILL_TURNS),
        "provider_count": len(catalog["providers"]),
        "models_per_provider": len(catalog["providers"][0]["configured_models"]),
        "fragment_reassembly": b"".join(fragments) == payload,
        "max_fragment_bytes": max(map(len, fragments)),
        "abba_n_per_arm_at_three_rounds": [
            arm for _block, _position, arm in abba_order(3)
        ].count("a"),
    }
    if checks != {
        "turns": 40,
        "checkpoints": [1, 5, 10, 20, 30, 40],
        "tool_turns": [4, 8, 12, 16, 20, 24, 28, 32, 36, 40],
        "spill_turns": [8, 24, 36],
        "provider_count": 24,
        "models_per_provider": 8,
        "fragment_reassembly": True,
        "max_fragment_bytes": 21,
        "abba_n_per_arm_at_three_rounds": 6,
    }:
        raise ProofError(f"deep fixture self-check failed: {checks}")
    return checks


def _arguments(argv: Sequence[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path)
    parser.add_argument("--variant-bin-dir", type=Path)
    parser.add_argument(
        "--experiment",
        choices=(
            "baseline",
            "free1-store-trace",
            "free2-msgpack",
            "free3-mimalloc",
        ),
        default="baseline",
    )
    parser.add_argument("--rounds", type=int, default=3)
    parser.add_argument("--runs", type=int, default=5)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--require-quiet", action="store_true")
    parser.add_argument("--keep-root", action="store_true")
    parser.add_argument("--commit-label")
    parser.add_argument("--self-check", action="store_true")
    return parser.parse_args(argv)


def main(argv: Sequence[str] | None = None) -> int:
    args = _arguments(sys.argv[1:] if argv is None else argv)
    try:
        if args.self_check:
            print(json.dumps(self_check(), indent=2, sort_keys=True))
            return 0
        if args.bin_dir is None:
            raise ProofError("--bin-dir is required")
        arm_a, arm_b = arms_for(args)
        if arm_b is None:
            if args.runs < 5:
                raise ProofError("baseline requires at least five independent runs")
            order = [(index, 1, "a") for index in range(1, args.runs + 1)]
            arms = {"a": arm_a}
        else:
            if args.rounds < 3:
                raise ProofError("A/B experiments require at least three ABBA rounds")
            order = abba_order(args.rounds)
            arms = {"a": arm_a, "b": arm_b}
        cpu_accounting = cpu_accounting_self_check()
        runs = []
        for block, position, arm_name in order:
            run = run_fixture(
                arms[arm_name],
                block=block,
                position=position,
                require_quiet=args.require_quiet,
                keep_root=args.keep_root,
            )
            runs.append(run)
            print(
                f"deep-turn {run['label']} complete "
                f"depth40={run['turns'][39]['wall_ms']:.3f}ms",
                file=sys.stderr,
                flush=True,
            )
        report = {
            "schema": "haider.deep-turn.v1",
            "created_at_utc": datetime.now(timezone.utc).isoformat(),
            "commit": args.commit_label or _git_commit(),
            "experiment": args.experiment,
            "parameters": {
                "turns": TURNS,
                "checkpoints": CHECKPOINTS,
                "tool_turns": TOOL_TURNS,
                "spill_turns": SPILL_TURNS,
                "providers": PROVIDER_COUNT,
                "models_per_provider": MODELS_PER_PROVIDER,
                "order": "single-arm"
                if arm_b is None
                else "ABBA repeated by block",
                "rounds": None if arm_b is None else args.rounds,
                "require_quiet": args.require_quiet,
                "strict_load_limit": STRICT_LOAD_LIMIT if args.require_quiet else None,
            },
            "cpu_accounting": cpu_accounting,
            "arms": {
                name: {
                    "binary_dir": str(arm.bin_dir),
                    "environment": arm.environment,
                    "description": arm.description,
                }
                for name, arm in arms.items()
            },
            "runs": runs,
            "summary": summarize_arms(runs),
            "contrasts": summarize_contrasts(runs),
            "failures": [],
            "passed": True,
        }
        rendered = json.dumps(report, indent=2, sort_keys=True) + "\n"
        if args.output:
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(rendered, encoding="utf-8")
        else:
            print(rendered, end="")
        return 0
    except Exception as error:
        print(
            f"deep-turn harness failed: {type(error).__name__}: {error}",
            file=sys.stderr,
        )
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
