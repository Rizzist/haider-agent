"""Shared stdlib-only support for the turn wall and SIGKILL proof harnesses."""

from __future__ import annotations

from dataclasses import dataclass
import ctypes
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import shutil
import signal
import statistics
import subprocess
import sys
import tempfile
import threading
import time
from typing import Any, Mapping, Sequence

try:
    import resource
except ImportError:  # pragma: no cover - Windows reports zero resource counters
    resource = None  # type: ignore[assignment]


PROVIDER_ID = "turnperf-proxy"
MODEL_ID = "turnperf-model"
TRACE_ENV = "HAIDER_DAEMON_TRACE"
BOUNDARY_FILE_ENV = "HAIDER_TEST_JOURNAL_BOUNDARY_FILE"
BOUNDARY_TARGET_ENV = "HAIDER_TEST_JOURNAL_KILL_AFTER"
TERMINAL_STATES = frozenset(("done", "errored", "cancelled"))


class ProofError(RuntimeError):
    """A correctness or ownership proof failed."""


@dataclass(frozen=True)
class CommandResult:
    argv: tuple[str, ...]
    returncode: int
    stdout: str
    stderr: str
    wall_ms: float
    cpu_ms: float
    child_peak_rss_kib: int
    observed_peak_rss_kib: int
    combined_peak_rss_kib: int
    timed_out: bool = False


def run_command(
    argv: Sequence[str | os.PathLike[str]],
    *,
    env: Mapping[str, str],
    cwd: Path,
    timeout: float,
    observe_pid: int | None = None,
) -> CommandResult:
    command = tuple(os.fspath(value) for value in argv)
    before_usage = resource.getrusage(resource.RUSAGE_CHILDREN) if resource else None
    started = time.monotonic_ns()
    process = subprocess.Popen(
        command,
        cwd=cwd,
        env=dict(env),
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        encoding="utf-8",
        errors="replace",
        start_new_session=os.name == "posix",
    )
    peak_lock = threading.Lock()
    sampled_peak_rss_kib = 0
    sampled_observed_peak_rss_kib = 0
    sampled_combined_peak_rss_kib = 0
    sampling_done = threading.Event()

    def sample_peak() -> None:
        nonlocal sampled_peak_rss_kib
        nonlocal sampled_observed_peak_rss_kib
        nonlocal sampled_combined_peak_rss_kib
        while not sampling_done.is_set():
            value = process_rss_kib(process.pid)
            observed = process_rss_kib(observe_pid) if observe_pid is not None else 0
            with peak_lock:
                sampled_peak_rss_kib = max(sampled_peak_rss_kib, value)
                sampled_observed_peak_rss_kib = max(
                    sampled_observed_peak_rss_kib, observed
                )
                sampled_combined_peak_rss_kib = max(
                    sampled_combined_peak_rss_kib, value + observed
                )
            sampling_done.wait(0.002)

    sampler = threading.Thread(target=sample_peak, name="turnperf-rss", daemon=True)
    sampler.start()
    timed_out = False
    try:
        stdout, stderr = process.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        timed_out = True
        if os.name == "posix":
            os.killpg(process.pid, signal.SIGKILL)
        else:
            process.kill()
        stdout, stderr = process.communicate()
    sampling_done.set()
    sampler.join(timeout=1)
    ended = time.monotonic_ns()
    after_usage = resource.getrusage(resource.RUSAGE_CHILDREN) if resource else None
    cpu_ms = 0.0
    peak_rss_kib = sampled_peak_rss_kib
    if before_usage is not None and after_usage is not None:
        cpu_ms = (
            (after_usage.ru_utime - before_usage.ru_utime)
            + (after_usage.ru_stime - before_usage.ru_stime)
        ) * 1_000
        # RUSAGE_CHILDREN's high-water mark is cumulative across every child
        # ever reaped by this harness process, so it cannot identify this
        # particular CLI sample. The live PID sampler above owns that value.
    return CommandResult(
        argv=command,
        returncode=process.returncode,
        stdout=stdout,
        stderr=stderr,
        wall_ms=(ended - started) / 1_000_000,
        cpu_ms=max(0.0, cpu_ms),
        child_peak_rss_kib=peak_rss_kib,
        observed_peak_rss_kib=sampled_observed_peak_rss_kib,
        combined_peak_rss_kib=sampled_combined_peak_rss_kib,
        timed_out=timed_out,
    )


def parse_json_lines(text: str, label: str) -> list[dict[str, Any]]:
    values: list[dict[str, Any]] = []
    for index, line in enumerate(text.splitlines(), start=1):
        if not line.strip():
            continue
        try:
            value = json.loads(line)
        except json.JSONDecodeError as error:
            raise ProofError(f"{label} line {index} invalid JSON: {error}") from error
        if not isinstance(value, dict):
            raise ProofError(f"{label} line {index} is not an object")
        values.append(value)
    return values


def parse_single_json(text: str, label: str) -> dict[str, Any]:
    values = parse_json_lines(text, label)
    if len(values) != 1:
        raise ProofError(f"{label} expected one JSON object, actual={len(values)}")
    return values[0]


def median_mad(samples: Sequence[float]) -> tuple[float, float]:
    if not samples:
        raise ProofError("median/MAD requires at least one sample")
    median = float(statistics.median(samples))
    mad = float(statistics.median(abs(sample - median) for sample in samples))
    return median, mad


def load_one_minute() -> float:
    try:
        return float(os.getloadavg()[0])
    except (AttributeError, OSError) as error:
        raise ProofError(f"one-minute load unavailable: {error}") from error


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def _tool_specifications(body: Mapping[str, Any]) -> list[Mapping[str, Any]]:
    tools = body.get("tools")
    if not isinstance(tools, list):
        return []
    result: list[Mapping[str, Any]] = []
    for value in tools:
        if not isinstance(value, Mapping):
            continue
        function = value.get("function")
        result.append(function if isinstance(function, Mapping) else value)
    return result


def _select_exec_tool(
    body: Mapping[str, Any], effect_token: str
) -> tuple[str, dict[str, Any]]:
    specifications = _tool_specifications(body)
    selected = next(
        (
            spec
            for spec in specifications
            if str(spec.get("name", "")).casefold() == "process_exec"
        ),
        None,
    )
    # Fall back only for older tool catalogs that do not expose the canonical
    # local process tool. Do not match generic ``shell``: current catalogs also
    # contain ``ssh_shell``, whose remote profile requirement cannot serve as
    # the harness's local monotonic effect ledger.
    for token in ("process", "exec", "execute", "command", "bash"):
        if selected is not None:
            break
        selected = next(
            (
                spec
                for spec in specifications
                if token in str(spec.get("name", "")).casefold()
            ),
            None,
        )
        if selected is not None:
            break
    if selected is None:
        raise ProofError("tool shape requires a monotonic process-exec tool")
    name = str(selected.get("name") or "bash")
    schema = selected.get("parameters")
    if not isinstance(schema, Mapping):
        schema = selected.get("input_schema")
    properties = schema.get("properties", {}) if isinstance(schema, Mapping) else {}
    if not isinstance(properties, Mapping):
        properties = {}
    command_key = next(
        (
            key
            for key in ("command", "cmd", "script", "shell_command", "code")
            if key in properties
        ),
        None,
    )
    command_key = command_key or (next(iter(properties), None)) or "command"
    arguments: dict[str, Any] = {
        command_key: (
            "printf '%s\\n' '" + effect_token + "' >> turnperf-tool-effects.log"
        )
    }
    required = schema.get("required", []) if isinstance(schema, Mapping) else []
    if isinstance(required, list):
        for key in required:
            if isinstance(key, str) and key not in arguments:
                arguments[key] = "." if key in ("cwd", "workdir", "directory") else "turnperf"
    return name, arguments


def _chat_chunk(model: str, delta: Mapping[str, Any], finish: str | None = None) -> bytes:
    value = {
        "id": "chatcmpl-turnperf",
        "object": "chat.completion.chunk",
        "created": 1,
        "model": model,
        "choices": [{"index": 0, "delta": dict(delta), "finish_reason": finish}],
    }
    return b"data: " + json.dumps(value, separators=(",", ":")).encode() + b"\n\n"


def _text_response(model: str) -> list[bytes]:
    return [
        _chat_chunk(model, {"role": "assistant"}),
        _chat_chunk(model, {"content": "turnperf complete"}),
        _chat_chunk(model, {}, "stop"),
        b"data: [DONE]\n\n",
    ]


def _tool_response(model: str, body: Mapping[str, Any], effect_token: str) -> list[bytes]:
    name, arguments = _select_exec_tool(body, effect_token)
    call = {
        "index": 0,
        "id": "turnperf-call-1",
        "type": "function",
        "function": {
            "name": name,
            "arguments": json.dumps(arguments, separators=(",", ":")),
        },
    }
    return [
        _chat_chunk(model, {"role": "assistant"}),
        _chat_chunk(model, {"tool_calls": [call]}),
        _chat_chunk(model, {}, "tool_calls"),
        b"data: [DONE]\n\n",
    ]


class ProxyState:
    """Case-resettable provider state with a proxy-owned immutable ledger."""

    def __init__(self, ledger_path: Path):
        self.ledger_path = ledger_path
        self._condition = threading.Condition()
        self._case_id = 0
        self._shape = "single"
        self._case_requests: list[dict[str, Any]] = []
        self._all_requests: list[dict[str, Any]] = []
        self._active_handlers = 0
        self._gate: tuple[int, str] | None = None
        self._gate_reached = False
        self._gate_released = False

    def begin_case(self, shape: str, gate: tuple[int, str] | None = None) -> int:
        if shape not in ("single", "tool"):
            raise ProofError(f"unknown provider shape {shape!r}")
        with self._condition:
            if self._active_handlers != 0:
                raise ProofError(
                    f"provider begin_case requires zero active handlers, actual={self._active_handlers}"
                )
            self._case_id += 1
            self._shape = shape
            self._case_requests = []
            self._gate = gate
            self._gate_reached = False
            self._gate_released = False
            return self._case_id

    def enter(self) -> None:
        with self._condition:
            self._active_handlers += 1

    def leave(self) -> None:
        with self._condition:
            self._active_handlers -= 1
            self._condition.notify_all()

    def record(self, body: Mapping[str, Any], path: str) -> tuple[int, str, int]:
        with self._condition:
            request_number = len(self._case_requests) + 1
            messages = body.get("messages", [])
            logical_ordinal = 2 if isinstance(messages, list) and any(
                isinstance(message, Mapping) and message.get("role") == "tool"
                for message in messages
            ) else 1
            entry = {
                "case_id": self._case_id,
                "shape": self._shape,
                "request_number": request_number,
                "logical_ordinal": logical_ordinal,
                "path": path,
                "model": body.get("model"),
                "tool_names": [str(spec.get("name", "")) for spec in _tool_specifications(body)],
                "recorded_monotonic_ns": time.monotonic_ns(),
            }
            self._case_requests.append(entry)
            self._all_requests.append(entry)
            with self.ledger_path.open("a", encoding="utf-8") as ledger:
                ledger.write(json.dumps(entry, separators=(",", ":")) + "\n")
                ledger.flush()
                os.fsync(ledger.fileno())
            return request_number, self._shape, self._case_id

    def gate(self, request_number: int, phase: str) -> None:
        with self._condition:
            if self._gate != (request_number, phase):
                return
            self._gate_reached = True
            self._condition.notify_all()
            while not self._gate_released:
                self._condition.wait(timeout=0.1)

    def wait_gate(self, timeout: float) -> bool:
        deadline = time.monotonic() + timeout
        with self._condition:
            while not self._gate_reached and time.monotonic() < deadline:
                self._condition.wait(timeout=min(0.1, deadline - time.monotonic()))
            return self._gate_reached

    def release_gate(self) -> None:
        with self._condition:
            self._gate_released = True
            self._condition.notify_all()

    def snapshot_case(self) -> list[dict[str, Any]]:
        with self._condition:
            return [dict(entry) for entry in self._case_requests]

    def snapshot_all(self) -> list[dict[str, Any]]:
        with self._condition:
            return [dict(entry) for entry in self._all_requests]

    def read_disk_ledger(self) -> list[dict[str, Any]]:
        try:
            return parse_json_lines(self.ledger_path.read_text(encoding="utf-8"), "provider ledger")
        except FileNotFoundError:
            # Before the first physical request, an absent append-only ledger
            # is the exact on-disk representation of an empty external log.
            return []
        except OSError as error:
            raise ProofError(f"provider ledger unavailable: {error}") from error

    def wait_idle(self, timeout: float) -> bool:
        deadline = time.monotonic() + timeout
        with self._condition:
            while self._active_handlers and time.monotonic() < deadline:
                self._condition.wait(timeout=min(0.1, deadline - time.monotonic()))
            return self._active_handlers == 0


class _Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    state: ProxyState

    def log_message(self, _format: str, *_args: object) -> None:
        return

    def do_POST(self) -> None:  # noqa: N802
        self.state.enter()
        try:
            length = int(self.headers.get("Content-Length", "0"))
            raw = self.rfile.read(max(0, length))
            try:
                decoded = json.loads(raw)
            except (UnicodeDecodeError, json.JSONDecodeError):
                decoded = {}
            body = decoded if isinstance(decoded, Mapping) else {}
            request_number, shape, case_id = self.state.record(body, self.path)
            self.state.gate(request_number, "after_post")
            chunks = (
                _tool_response(MODEL_ID, body, f"turnperf-effect-{case_id}")
                if shape == "tool" and request_number == 1
                else _text_response(MODEL_ID)
            )
            payload = b"".join(chunks)
            self.state.gate(request_number, "before_headers")
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Cache-Control", "no-cache")
            self.send_header("Content-Length", str(len(payload)))
            self.send_header("Connection", "keep-alive")
            self.end_headers()
            split = max(1, len(chunks) // 2)
            self.wfile.write(b"".join(chunks[:split]))
            self.wfile.flush()
            self.state.gate(request_number, "between_chunks")
            self.wfile.write(b"".join(chunks[split:]))
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            return
        finally:
            self.state.leave()

    def do_GET(self) -> None:  # noqa: N802
        if self.path.rstrip("/").endswith("/models"):
            payload = json.dumps(
                {
                    "object": "list",
                    "data": [{"id": MODEL_ID, "object": "model", "created": 1}],
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


class _QuietThreadingHTTPServer(ThreadingHTTPServer):
    def handle_error(self, _request: object, _client_address: object) -> None:
        # SIGKILL deliberately resets keep-alive sockets. Those resets are
        # expected proof stimuli, not fake-provider failures worth dumping to
        # the matrix runner's stderr.
        return


class FakeProvider:
    def __init__(self, ledger_path: Path):
        self.state = ProxyState(ledger_path)
        handler = type("TurnperfHandler", (_Handler,), {"state": self.state})
        self.server = _QuietThreadingHTTPServer(("127.0.0.1", 0), handler)
        self.server.daemon_threads = True
        self.server.block_on_close = False
        self.thread = threading.Thread(
            target=self.server.serve_forever, name="turnperf-provider", daemon=True
        )

    @property
    def base_url(self) -> str:
        host, port = self.server.server_address[:2]
        return f"http://{host}:{port}/v1"

    def __enter__(self) -> "FakeProvider":
        self.thread.start()
        return self

    def __exit__(self, *_args: object) -> None:
        self.state.release_gate()
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=2)


class ThrowawayProfile:
    """One exact profile/runtime and its installed binary pair."""

    def __init__(self, bin_dir: Path, proxy_url: str, root: Path | None = None):
        self.bin_dir = bin_dir.resolve()
        self.haider = self.bin_dir / ("haider.exe" if os.name == "nt" else "haider")
        self.haiderd = self.bin_dir / ("haiderd.exe" if os.name == "nt" else "haiderd")
        if not self.haider.is_file() or not self.haiderd.is_file():
            raise ProofError(f"installed haider/haiderd pair missing under {self.bin_dir}")
        self.root = root or Path(tempfile.mkdtemp(prefix="htp-", dir="/tmp"))
        self.profile = self.root / "p"
        self.runtime = self.root / "r"
        self.home = self.root / "h"
        self.workspace = self.root / "w"
        for path in (self.profile, self.runtime, self.home, self.workspace):
            path.mkdir(parents=True, mode=0o700, exist_ok=True)
        providers = {
            "providers": [
                {
                    "provider_id": PROVIDER_ID,
                    "display_name": "turn performance proxy",
                    "api_family": "openai_chat_completions",
                    "base_url": proxy_url,
                    "enabled": True,
                    "auth_requirement": "none",
                    "configured_models": [MODEL_ID],
                    "default_model": MODEL_ID,
                    "provenance": "custom",
                }
            ]
        }
        (self.profile / "providers.json").write_text(
            json.dumps(providers, separators=(",", ":")) + "\n", encoding="utf-8"
        )
        os.chmod(self.profile / "providers.json", 0o600)
        environment = os.environ.copy()
        for key in tuple(environment):
            if key.startswith("HAIDER_") or key.endswith(("_API_KEY", "_TOKEN", "_SECRET")):
                environment.pop(key, None)
        environment.update(
            {
                "HAIDER_PROFILE_DIR": str(self.profile),
                "HAIDER_RUNTIME_DIR": str(self.runtime),
                "HAIDER_DISCOVERY_DISABLED": "1",
                "HAIDER_NO_UPDATE_CHECK": "1",
                "HAIDER_TEST_DEVICE_NAME": "test-mac",
                # The custom keyless-provider default intentionally excludes
                # process execution. This hermetic throwaway profile needs the
                # append-only process effect below so a duplicate execution
                # produces a second externally countable token.
                "HAIDER_AUTO_HERMETIC": "0",
                "HOME": str(self.home),
                "USERPROFILE": str(self.home),
                "XDG_CACHE_HOME": str(self.home / ".cache"),
                "XDG_CONFIG_HOME": str(self.home / ".config"),
                "XDG_DATA_HOME": str(self.home / ".local" / "share"),
                "XDG_STATE_HOME": str(self.home / ".local" / "state"),
                "TMPDIR": str(self.root),
                "NO_COLOR": "1",
                "TERM": "xterm-256color",
            }
        )
        self.env = environment

    def command(
        self,
        args: Sequence[str],
        *,
        timeout: float = 30,
        overrides: Mapping[str, str | None] | None = None,
        observe_pid: int | None = None,
    ) -> CommandResult:
        environment = self.env.copy()
        for key, value in (overrides or {}).items():
            if value is None:
                environment.pop(key, None)
            else:
                environment[key] = value
        return run_command(
            (self.haider, *args),
            env=environment,
            cwd=self.workspace,
            timeout=timeout,
            observe_pid=observe_pid,
        )

    def ready(self, overrides: Mapping[str, str | None] | None = None) -> None:
        result = self.command(["--ready"], timeout=60, overrides=overrides)
        if result.timed_out or result.returncode != 0:
            raise ProofError(
                f"daemon readiness failed exit={result.returncode} stderr={result.stderr!r}"
            )

    def status(self) -> tuple[int, int, dict[str, Any]]:
        result = self.command(["status", "--json", "--no-spawn"], timeout=10)
        if result.returncode != 0:
            raise ProofError(f"daemon status failed exit={result.returncode}")
        document = parse_single_json(result.stdout, "daemon status")
        daemon = document.get("daemon")
        if not isinstance(daemon, dict):
            raise ProofError("daemon status has no daemon object")
        pid = daemon.get("pid")
        generation = daemon.get("generation")
        if (
            isinstance(pid, bool)
            or not isinstance(pid, int)
            or pid <= 0
            or isinstance(generation, bool)
            or not isinstance(generation, int)
            or generation <= 0
        ):
            raise ProofError(f"invalid daemon identity pid={pid!r} generation={generation!r}")
        if Path(str(document.get("profile_path", ""))).resolve() != self.profile.resolve():
            raise ProofError("daemon status profile ownership mismatch")
        return pid, generation, document

    def stop(self) -> CommandResult:
        return self.command(["daemon", "stop", "--json"], timeout=30)

    def dispose(self) -> None:
        shutil.rmtree(self.root, ignore_errors=True)


def run_arguments(shape: str) -> list[str]:
    arguments = [
        "run",
        "-p",
        f"turnperf {shape}",
        "--provider",
        PROVIDER_ID,
        "--model",
        MODEL_ID,
        "--output",
        "jsonl",
        "--timeout",
        "20s",
    ]
    if shape == "tool":
        arguments.extend(("--auto-allow", "--allow-writes", "--allow-exec"))
    return arguments


def validate_jsonl(stdout: str, shape: str) -> dict[str, Any]:
    documents = parse_json_lines(stdout, f"{shape} JSONL")
    if not documents or documents[0].get("event") != "accepted":
        raise ProofError(f"{shape} JSONL first record is not accepted")
    accepted = documents[0]
    events = documents[1:]
    if not events:
        raise ProofError(f"{shape} JSONL has no envelopes")
    session_id = accepted.get("session_id")
    sequences = [event.get("seq") for event in events]
    if sequences[0] != accepted.get("head_seq"):
        raise ProofError(f"{shape} JSONL head_seq does not match first envelope")
    if any(
        isinstance(before, bool)
        or not isinstance(before, int)
        or after != before + 1
        for before, after in zip(sequences, sequences[1:])
    ):
        raise ProofError(f"{shape} JSONL sequence is not contiguous")
    terminals = [
        event
        for event in events
        if isinstance(event.get("payload"), dict)
        and event["payload"].get("terminal_kind") is not None
    ]
    if len(terminals) != 1:
        raise ProofError(f"{shape} typed terminal count expected=1 actual={len(terminals)}")
    if terminals[0] is not events[-1]:
        raise ProofError(f"{shape} typed terminal is not the final live envelope")
    run_ids = {event.get("run_id") for event in events if isinstance(event.get("run_id"), str)}
    if len(run_ids) != 1 or not isinstance(session_id, str):
        raise ProofError(f"{shape} JSONL run/session identity is not singular")
    return {
        "accepted": accepted,
        "events": events,
        "session_id": session_id,
        "run_id": next(iter(run_ids)),
        "terminal_seq": terminals[0].get("seq"),
        "terminal_kind": terminals[0]["payload"].get("terminal_kind"),
    }


def wait_session_idle(profile: ThrowawayProfile, session_id: str, timeout: float = 5) -> None:
    deadline = time.monotonic() + timeout
    actual: Any = None
    while time.monotonic() < deadline:
        result = profile.command(
            ["session", session_id, "--json", "--no-spawn"], timeout=min(2, timeout)
        )
        if result.returncode == 0:
            document = parse_single_json(result.stdout, "session idle")
            session = document.get("session")
            if isinstance(session, dict):
                summary = session.get("summary")
                actual = (
                    summary.get("run_state")
                    if isinstance(summary, dict)
                    else session.get("run_state")
                )
                if actual == "idle":
                    return
        time.sleep(0.01)
    raise ProofError(f"session did not settle durably Idle, actual={actual!r}")


def wait_session_settled(profile: ThrowawayProfile, session_id: str, timeout: float = 5) -> str:
    """Wait for a durable non-running session projection after recovery.

    A normal completed turn reaches ``idle``. A matrix case parked behind the
    typed recovery door is explicitly abandoned to project ``errored``; an
    independently recovered cancellation may project ``cancelled``. The
    matrix validates the typed terminal and replay separately.
    """

    deadline = time.monotonic() + timeout
    actual: Any = None
    while time.monotonic() < deadline:
        result = profile.command(
            ["session", session_id, "--json", "--no-spawn"], timeout=min(2, timeout)
        )
        if result.returncode == 0:
            document = parse_single_json(result.stdout, "session settled")
            session = document.get("session")
            if isinstance(session, dict):
                summary = session.get("summary")
                actual = (
                    summary.get("run_state")
                    if isinstance(summary, dict)
                    else session.get("run_state")
                )
                if actual in {"idle", "errored", "cancelled"}:
                    return actual
        time.sleep(0.01)
    raise ProofError(f"session did not settle durably, actual={actual!r}")


def process_rss_kib(pid: int) -> int:
    native = _process_usage(pid)
    if native is not None:
        return native[1]
    try:
        result = subprocess.run(
            ["ps", "-o", "rss=", "-p", str(pid)],
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=2,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired):
        return 0
    try:
        return int(result.stdout.strip())
    except ValueError:
        return 0


def process_cpu_ms(pid: int) -> float:
    native = _process_usage(pid)
    if native is not None:
        return native[0]
    try:
        result = subprocess.run(
            ["ps", "-o", "time=", "-p", str(pid)],
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=2,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired):
        return 0.0
    value = result.stdout.strip()
    try:
        day_split = value.split("-", 1)
        days = int(day_split[0]) if len(day_split) == 2 else 0
        clock = day_split[-1].split(":")
        if len(clock) == 3:
            hours, minutes, seconds = int(clock[0]), int(clock[1]), float(clock[2])
        elif len(clock) == 2:
            hours, minutes, seconds = 0, int(clock[0]), float(clock[1])
        else:
            return 0.0
        return ((days * 24 + hours) * 3_600 + minutes * 60 + seconds) * 1_000
    except ValueError:
        return 0.0


def process_peak_rss_kib(pid: int) -> int:
    native = _process_usage(pid)
    return native[2] if native is not None else 0


class _DarwinRusageInfoV2(ctypes.Structure):
    _fields_ = [
        ("ri_uuid", ctypes.c_ubyte * 16),
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
        ("ri_child_user_time", ctypes.c_uint64),
        ("ri_child_system_time", ctypes.c_uint64),
        ("ri_child_pkg_idle_wkups", ctypes.c_uint64),
        ("ri_child_interrupt_wkups", ctypes.c_uint64),
        ("ri_child_pageins", ctypes.c_uint64),
        ("ri_child_elapsed_abstime", ctypes.c_uint64),
        ("ri_diskio_bytesread", ctypes.c_uint64),
        ("ri_diskio_byteswritten", ctypes.c_uint64),
    ]


class _DarwinRusageInfoV4(ctypes.Structure):
    _fields_ = _DarwinRusageInfoV2._fields_ + [
        ("ri_cpu_time_qos_default", ctypes.c_uint64),
        ("ri_cpu_time_qos_maintenance", ctypes.c_uint64),
        ("ri_cpu_time_qos_background", ctypes.c_uint64),
        ("ri_cpu_time_qos_utility", ctypes.c_uint64),
        ("ri_cpu_time_qos_legacy", ctypes.c_uint64),
        ("ri_cpu_time_qos_user_initiated", ctypes.c_uint64),
        ("ri_cpu_time_qos_user_interactive", ctypes.c_uint64),
        ("ri_billed_system_time", ctypes.c_uint64),
        ("ri_serviced_system_time", ctypes.c_uint64),
        ("ri_logical_writes", ctypes.c_uint64),
        ("ri_lifetime_max_phys_footprint", ctypes.c_uint64),
        ("ri_instructions", ctypes.c_uint64),
        ("ri_cycles", ctypes.c_uint64),
        ("ri_billed_energy", ctypes.c_uint64),
        ("ri_serviced_energy", ctypes.c_uint64),
        ("ri_interval_max_phys_footprint", ctypes.c_uint64),
        ("ri_runnable_time", ctypes.c_uint64),
    ]


def _process_usage(pid: int) -> tuple[float, int, int] | None:
    if sys.platform == "darwin":
        try:
            library = ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True)
            library.proc_pid_rusage.argtypes = [
                ctypes.c_int,
                ctypes.c_int,
                ctypes.c_void_p,
            ]
            library.proc_pid_rusage.restype = ctypes.c_int
            info = _DarwinRusageInfoV4()
            if library.proc_pid_rusage(pid, 4, ctypes.byref(info)) != 0:
                return None
            cpu_ms = (info.ri_user_time + info.ri_system_time) / 1_000_000
            # `phys_footprint` and `lifetime_max_phys_footprint` are accounting
            # metrics, not resident-set size. Return current resident bytes in
            # both slots on Darwin; the 2 ms live sampler owns the per-command
            # RSS high-water mark and samples client+daemon simultaneously.
            rss_kib = int(info.ri_resident_size // 1_024)
            peak_rss_kib = rss_kib
            return cpu_ms, rss_kib, peak_rss_kib
        except (AttributeError, OSError):
            return None
    stat = Path(f"/proc/{pid}/stat")
    statm = Path(f"/proc/{pid}/statm")
    try:
        fields = stat.read_text(encoding="ascii").split()
        ticks = os.sysconf("SC_CLK_TCK")
        cpu_ms = (int(fields[13]) + int(fields[14])) * 1_000 / ticks
        resident_pages = int(statm.read_text(encoding="ascii").split()[1])
        rss_kib = resident_pages * os.sysconf("SC_PAGE_SIZE") // 1_024
        status = Path(f"/proc/{pid}/status").read_text(encoding="ascii")
        peak_line = next(
            (line for line in status.splitlines() if line.startswith("VmHWM:")), ""
        )
        peak_rss_kib = int(peak_line.split()[1]) if peak_line else rss_kib
        return cpu_ms, rss_kib, peak_rss_kib
    except (OSError, ValueError, IndexError):
        return None


def assert_provider_ledger(entries: Sequence[Mapping[str, Any]], shape: str) -> None:
    expected = 1 if shape == "single" else 2
    if len(entries) != expected:
        raise ProofError(f"{shape} provider requests expected={expected} actual={len(entries)}")
    ordinals = [entry.get("logical_ordinal") for entry in entries]
    if ordinals != list(range(1, expected + 1)):
        raise ProofError(f"{shape} physical request ordinals expected=1..{expected} actual={ordinals}")


def tool_effect_count(effect_root: Path, case_id: int) -> int:
    logs = list(effect_root.rglob("turnperf-tool-effects.log"))
    path = logs[0] if logs else effect_root / "turnperf-tool-effects.log"
    token = f"turnperf-effect-{case_id}"
    try:
        tokens = path.read_text(encoding="utf-8").splitlines()
    except FileNotFoundError:
        token_files = list(effect_root.rglob(f"{token}.txt"))
        if not token_files:
            return 0
        raise ProofError(
            "non-monotonic fs_write effect observed; process-exec append ledger required"
        )
    except OSError as error:
        raise ProofError(f"tool effect ledger unavailable: {error}") from error
    return tokens.count(token)


def assert_tool_effect(effect_root: Path, case_id: int) -> None:
    token = f"turnperf-effect-{case_id}"
    actual = tool_effect_count(effect_root, case_id)
    if actual != 1:
        raise ProofError(f"tool effect token={token} expected=1 actual={actual}")
