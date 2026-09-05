"""Real-daemon PTY primitives shared by the TUI QA-gate checks.

This module deliberately loads ``scripts/tui-probes/probelib.py`` lazily:
checks declare ``pty`` as a need, so a non-POSIX host becomes ENV_BLOCKED
before importing POSIX-only modules.
"""

from __future__ import annotations

from dataclasses import dataclass
import importlib.util
import json
import os
from pathlib import Path
import queue
import socket
import sqlite3
import struct
import threading
import time
from typing import Any, Callable

from .context import parse_single_json
from .contract import DAEMON_STARTUP, STATUS_REQUEST, BudgetPart, budget_seconds


TUI_BOOT = BudgetPart(
    "live TUI alternate-screen boot",
    25.0,
    "scripts/tui-probes/pty-probe-live.py:177-184 boot deadline",
)
TUI_ACTION = BudgetPart(
    "palette action observation",
    12.0,
    "scripts/tui-probes/pty-probe-live.py:186-191 bounded visible-state wait",
)
TUI_REPAINT = BudgetPart(
    "two pinned full repaints",
    4.0,
    "scripts/tui-probes/pty-probe-live.py:221-237 resize repaint (2s each) × 2 sizes",
)
TUI_EXIT = BudgetPart(
    "clean PTY child exit",
    2.5,
    "scripts/tui-probes/probelib.py:154-178 reap deadline",
)
RPC_KEEPALIVE = BudgetPart(
    "QA RPC keepalive cadence",
    15.0,
    "crates/haider-daemon/src/connection.rs:97-105 READ_IDLE_DEADLINE=45s; cadence=deadline/3",
)
RPC_THREAD_EXIT = BudgetPart(
    "QA RPC helper thread exit",
    1.0,
    "local socket shutdown makes both helper threads immediately runnable",
)


def _probelib():
    path = Path(__file__).resolve().parents[2] / "tui-probes" / "probelib.py"
    spec = importlib.util.spec_from_file_location("haider_qa_tui_probelib", path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot import shared probe harness at {path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def start_daemon(ctx) -> dict[str, Any]:
    """Start the one owned daemon and record its status-derived identity."""

    result = ctx.run_haider(["status", "--json"], timeout=DAEMON_STARTUP + STATUS_REQUEST)
    if result.timed_out or result.returncode != 0:
        raise RuntimeError(
            f"daemon status exit={result.returncode} timed_out={str(result.timed_out).lower()}"
        )
    status = parse_single_json(result.stdout, "TUI fixture status")
    problems = ctx.observe_status(status)
    if problems:
        raise RuntimeError("; ".join(problems))
    return status


class RpcClient:
    """Small JSON-framed UDS client for independent durable oracles."""

    def __init__(
        self,
        socket_path: str,
        timeout: BudgetPart = STATUS_REQUEST,
        keepalive: BudgetPart = RPC_KEEPALIVE,
    ):
        self._timeout = budget_seconds(timeout)
        self._keepalive_interval = budget_seconds(keepalive)
        self._write_lock = threading.Lock()
        self._request_lock = threading.Lock()
        self._frames: queue.Queue[dict[str, Any] | BaseException] = queue.Queue()
        self._closed = threading.Event()
        self.socket = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.socket.settimeout(self._timeout)
        self.socket.connect(socket_path)
        self.counter = 0
        self._send(
            {
                "v": 1,
                "kind": "hello",
                "protocol_min": 1,
                "protocol_max": 1,
                "client_name": "qa-gate-tui",
                "client_version": "1",
                "client_instance_id": f"qa-{os.getpid()}-{id(self)}",
                "client_kind": "tui",
                "capabilities_requested": ["control", "view"],
                "max_receive_frame": 16 * 1024 * 1024,
            }
        )
        welcome = self._recv()
        if welcome.get("kind") != "welcome":
            raise RuntimeError(f"RPC welcome expected=welcome actual={welcome!r}")
        # Registry #95: the fixture may retain this negotiated connection while
        # a real TUI process is being driven. A dedicated reader services every
        # Ping immediately instead of letting an external-state observation
        # outlive the daemon's idle bound.
        self.socket.settimeout(None)
        self._reader = threading.Thread(
            target=self._read_loop,
            name="qa-gate-rpc-keepalive",
            daemon=True,
        )
        self._reader.start()
        self._ping_counter = 0
        self._keepalive = threading.Thread(
            target=self._keepalive_loop,
            name="qa-gate-rpc-keepalive-writer",
            daemon=True,
        )
        self._keepalive.start()

    def close(self) -> None:
        if self._closed.is_set():
            return
        self._closed.set()
        try:
            self.socket.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
        try:
            self.socket.close()
        except OSError:
            pass
        self._reader.join(timeout=budget_seconds(RPC_THREAD_EXIT))
        self._keepalive.join(timeout=budget_seconds(RPC_THREAD_EXIT))

    def _read_exact(self, size: int) -> bytes:
        chunks: list[bytes] = []
        remaining = size
        while remaining:
            chunk = self.socket.recv(remaining)
            if not chunk:
                raise RuntimeError("RPC peer closed mid-frame")
            chunks.append(chunk)
            remaining -= len(chunk)
        return b"".join(chunks)

    def _send(self, frame: dict[str, Any]) -> None:
        body = json.dumps(frame, separators=(",", ":")).encode()
        with self._write_lock:
            self.socket.sendall(struct.pack(">I", len(body)) + body)

    def _recv(self) -> dict[str, Any]:
        length = struct.unpack(">I", self._read_exact(4))[0]
        value = json.loads(self._read_exact(length))
        if not isinstance(value, dict):
            raise RuntimeError(f"RPC frame expected=object actual={type(value).__name__}")
        return value

    def _read_loop(self) -> None:
        try:
            while not self._closed.is_set():
                frame = self._recv()
                if frame.get("kind") == "ping":
                    self._send({"v": 1, "kind": "pong", "nonce": frame.get("nonce", 0)})
                elif frame.get("kind") == "pong":
                    continue
                else:
                    self._frames.put(frame)
        except BaseException as error:
            if not self._closed.is_set():
                self._frames.put(error)

    def _keepalive_loop(self) -> None:
        """Send client-owned Ping traffic before the daemon's read-idle deadline."""

        try:
            while not self._closed.wait(timeout=self._keepalive_interval):
                self._ping_counter += 1
                self._send({"v": 1, "kind": "ping", "nonce": self._ping_counter})
        except BaseException as error:
            if not self._closed.is_set():
                self._frames.put(error)

    def request(self, body: dict[str, Any]) -> dict[str, Any]:
        # One continuous STATUS_REQUEST deadline contains the send and response
        # wait. The background reader keeps Ping/Pong alive inside that bound.
        deadline = time.monotonic() + self._timeout
        with self._request_lock:
            self.counter += 1
            request_id = f"qa-{self.counter}"
            self._send({"v": 1, "kind": "request", "request_id": request_id, "body": body})
            while True:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise RuntimeError(
                        f"RPC request {body.get('method')} deadline={self._timeout}s exhausted"
                    )
                try:
                    received = self._frames.get(timeout=remaining)
                except queue.Empty as error:
                    raise RuntimeError(
                        f"RPC request {body.get('method')} deadline={self._timeout}s exhausted"
                    ) from error
                if isinstance(received, BaseException):
                    raise RuntimeError(
                        f"RPC request {body.get('method')} transport={received!r}"
                    ) from received
                frame = received
                if frame.get("kind") == "response" and frame.get("request_id") == request_id:
                    answer = frame.get("body")
                    if not isinstance(answer, dict):
                        raise RuntimeError(f"RPC response body actual={answer!r}")
                    return answer
                if frame.get("kind") == "error" and frame.get("request_id") == request_id:
                    raise RuntimeError(f"RPC request {body.get('method')} error={frame!r}")

    def command_list(
        self, query: str = "", *, in_session: bool = True, slots: dict[str, Any] | None = None
    ) -> list[dict[str, Any]]:
        body: dict[str, Any] = {
            "method": "command.list",
            "query": query,
            "in_session": in_session,
        }
        if slots:
            body["slots"] = slots
        response = self.request(body)
        items = response.get("items")
        if not isinstance(items, list):
            raise RuntimeError(f"command.list items actual={items!r}")
        return items

    def create_session(
        self,
        cwd: Path,
        *,
        provider: str = "fake",
        model: str = "fake-model",
        effort: str | None = None,
        fast: bool | None = None,
    ) -> tuple[str, int]:
        self.counter += 1
        body: dict[str, Any] = {
            "method": "session.create",
            "command_id": f"qa-create-{self.counter}",
            "cwd": str(cwd),
            "provider": provider,
            "model": model,
            "max_tokens": 4096,
        }
        if effort is not None:
            body["effort"] = effort
        if fast is not None:
            body["fast"] = fast
        response = self.request(body)
        session_id = response.get("session_id")
        generation = response.get("worker_generation")
        if not isinstance(session_id, str) or not session_id:
            raise RuntimeError(f"session.create session_id actual={session_id!r}")
        if isinstance(generation, bool) or not isinstance(generation, int):
            raise RuntimeError(f"session.create worker_generation actual={generation!r}")
        return session_id, generation

    def attach_control(self, session_id: str) -> dict[str, Any]:
        return self.request(
            {
                "method": "session.attach",
                "session_id": session_id,
                "after_seq": 0,
                "mode": "control",
                "sealed_replay": False,
            }
        )


@dataclass(frozen=True)
class Frame:
    cols: int
    rows_count: int
    raw: bytes
    rows: dict[int, str]

    @property
    def text(self) -> str:
        return "\n".join(self.rows.get(row, "") for row in range(1, self.rows_count + 1))

    @property
    def body_cells(self) -> tuple[str, ...]:
        return tuple(self.rows.get(row, "") for row in range(1, self.rows_count))


class TuiProcess:
    """One independently audited TUI child, live or demo."""

    def __init__(self, ctx, *, session_id: str | None = None, demo: bool = False):
        import pty
        import signal

        self.ctx = ctx
        self.probe = _probelib()
        self.probe.require_throwaway_profile(ctx.profile_dir)
        self.sink = [b""]
        self.closed = False
        self.clean = False
        self.pid = -1
        self.fd = -1
        self.pump: Callable[[float], None] = lambda _seconds: None
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.chdir(ctx.workspace_dir)
            os.environ.clear()
            os.environ.update(ctx.env)
            argv = [str(ctx.haider_bin), "tui"]
            if demo:
                argv.append("--demo")
            if session_id is not None:
                argv.extend(("--session", session_id))
            os.execv(str(ctx.haider_bin), argv)
        ctx.spawn_possible = True
        try:
            self.probe.set_size(self.fd, 118, 36)
            os.kill(self.pid, signal.SIGWINCH)
            self.pump = self.probe.make_pump(self.fd, self.sink)
            # Registry #94: alternate-screen boot and an optional direct
            # session handoff share ONE TUI_BOOT deadline; they are phases of
            # one launch, not two independently budgeted waits.
            deadline = time.monotonic() + budget_seconds(TUI_BOOT)
            while time.monotonic() < deadline and b"\x1b[?1049h" not in self.sink[0]:
                self.pump(0.15)
            if b"\x1b[?1049h" not in self.sink[0]:
                raise RuntimeError("TUI boot expected=alternate_screen actual=absent")
            if session_id is not None:
                if not self._wait_for_session_composer(deadline):
                    raise RuntimeError(
                        f"TUI session attach expected=message_composer actual=absent session={session_id}"
                    )
            self.pump(0.5)
        except BaseException:
            # The clean-exit law also owns constructor failures: a child that
            # fails before `with TuiProcess(...)` is entered must still be
            # dismissed, reaped, and have its PTY descriptor closed.
            self.close()
            raise

    def _wait_for_session_composer(self, deadline: float) -> bool:
        # Incremental paint history does not guarantee a complete current
        # frame after handoff.
        # Force the same full repaint used by the action probes, and require
        # the actual composer in that new frame. Both resize phases consume
        # the original TUI_BOOT deadline; neither starts another allowance.
        while time.monotonic() < deadline:
            self.probe.set_size(self.fd, 118, 35)
            self.pump(min(0.35, max(0.0, deadline - time.monotonic())))
            if time.monotonic() >= deadline:
                return False
            mark = len(self.sink[0])
            self.probe.set_size(self.fd, 118, 36)
            self.pump(min(0.8, max(0.0, deadline - time.monotonic())))
            if time.monotonic() >= deadline:
                return False
            raw = self.sink[0][mark:]
            frame = Frame(118, 36, raw, self.probe.screen_rows(raw))
            if "message haider" in frame.text:
                return True
        return False

    def write(self, data: bytes) -> None:
        view = memoryview(data)
        while view:
            written = os.write(self.fd, view)
            if written <= 0:
                raise RuntimeError("PTY write made no progress")
            view = view[written:]

    def type(self, text: str) -> None:
        self.write(text.encode())

    def type_slow(self, text: str) -> None:
        """Type through repainting palettes without racing cursor rewrites."""

        for character in text:
            self.write(character.encode())
            # The real palette repaints after every key and can move the PTY
            # cursor while a burst is still queued. One 50ms pump per cell is
            # below the 12s action allowance and makes the observed composer
            # the same text the probe supplied on loaded builders.
            self.pump(0.05)

    def enter(self) -> None:
        self.write(b"\r")

    def esc(self) -> None:
        self.write(b"\x1b")

    def down(self, count: int = 1) -> None:
        self.write(b"\x1b[B" * count)

    def up(self, count: int = 1) -> None:
        self.write(b"\x1b[A" * count)

    def tab(self) -> None:
        self.write(b"\t")

    def paste(self, text: str) -> None:
        self.write(b"\x1b[200~" + text.encode() + b"\x1b[201~")

    def wait_for(self, predicate: Callable[[bytes], bool], budget: BudgetPart = TUI_ACTION) -> bool:
        mark = len(self.sink[0])
        deadline = time.monotonic() + budget_seconds(budget)
        while time.monotonic() < deadline:
            if predicate(self.sink[0][mark:]):
                return True
            self.pump(0.15)
        return predicate(self.sink[0][mark:])

    def settle(self, seconds: float = 0.35) -> None:
        self.pump(seconds)

    def repaint(self, cols: int, rows: int) -> Frame:
        self.probe.set_size(self.fd, cols, rows - 1)
        self.pump(0.35)
        mark = len(self.sink[0])
        self.probe.set_size(self.fd, cols, rows)
        self.pump(0.8)
        raw = self.sink[0][mark:]
        return Frame(cols, rows, raw, self.probe.screen_rows(raw))

    def repaint_both(self) -> tuple[Frame, Frame]:
        return self.repaint(118, 36), self.repaint(80, 24)

    def close(self) -> tuple[bool, str]:
        if self.closed:
            return self.clean, self.audit_line()
        self.closed = True
        try:
            # Dismiss nested picker/card surfaces before requesting process
            # exit. Escape is local UI navigation and cannot cancel daemon
            # work; Ctrl-C then owns session -> launcher -> process.
            for _ in range(3):
                self.write(b"\x1b")
                self.pump(0.1)
            # A command may be two layers deep (card -> session -> launcher).
            # Ctrl-C owns one layer at a time, so drive all layers before the
            # shared reap law judges the child. Stop as soon as writes fail.
            for _ in range(6):
                self.write(b"\x03")
                self.pump(0.2)
                if self.sink[0].count(b"\x1b[?1049l") >= self.sink[0].count(b"\x1b[?1049h"):
                    break
            self.probe.drain_quiet(self.fd, self.sink, quiet_s=0.2, hard_s=0.8)
        except OSError:
            pass
        if self.pid > 0:
            self.clean = self.probe.reap(self.pid, timeout=budget_seconds(TUI_EXIT))
        try:
            if self.fd >= 0:
                os.close(self.fd)
        except OSError:
            pass
        return self.clean, self.audit_line()

    def audit_line(self) -> str:
        output = self.sink[0]
        enters = output.count(b"\x1b[?1049h")
        leaves = output.count(b"\x1b[?1049l")
        decoded = output.decode("utf-8", "replace")
        return (
            f"alt_enter={enters} alt_leave={leaves} panic_free="
            f"{str('panicked' not in decoded and 'RUST_BACKTRACE' not in decoded).lower()} "
            f"exit_clean={str(self.clean).lower()}"
        )

    def __enter__(self) -> "TuiProcess":
        return self

    def __exit__(self, _type, _value, _traceback) -> None:
        self.close()


@dataclass(frozen=True)
class DurableSnapshot:
    events: int
    receipts: int
    sessions: int
    event_rows: tuple[tuple[Any, ...], ...]
    receipt_rows: tuple[tuple[Any, ...], ...]


def durable_snapshot(profile_dir: Path) -> DurableSnapshot:
    path = profile_dir / "store.sqlite"
    connection = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
    try:
        events = connection.execute("select count(*) from events").fetchone()[0]
        receipts = connection.execute("select count(*) from command_receipts").fetchone()[0]
        sessions = connection.execute("select count(*) from sessions").fetchone()[0]
        event_rows = tuple(
            connection.execute(
                "select session_id, seq, payload_kind from events order by session_id, seq"
            )
        )
        rows = tuple(
            connection.execute(
                "select command_id, method, state, session_id, accepted_seq, final_revision "
                "from command_receipts order by rowid"
            )
        )
        return DurableSnapshot(events, receipts, sessions, event_rows, rows)
    finally:
        connection.close()


def snapshot_delta(before: DurableSnapshot, after: DurableSnapshot) -> str:
    return (
        f"journal_delta(events={after.events-before.events},"
        f"receipts={after.receipts-before.receipts},sessions={after.sessions-before.sessions})"
    )


def action_rows(before: DurableSnapshot, after: DurableSnapshot) -> tuple[tuple[str, ...], tuple[str, ...]]:
    """Return newly durable event kinds and committed/failed receipt methods.

    Stable `(session_id, seq)` and command-id identities make set difference
    correct even when a newly created session sorts before an older session.
    """

    old_events = set(before.event_rows)
    events = tuple(str(row[2]) for row in after.event_rows if row not in old_events)
    old_receipts = set(before.receipt_rows)
    receipts = tuple(
        f"{row[1]}:{row[2]}" for row in after.receipt_rows if row not in old_receipts
    )
    return events, receipts


def changed_body(before: Frame, after: Frame) -> tuple[int, ...]:
    limit = min(before.rows_count, after.rows_count) - 1
    return tuple(
        row
        for row in range(1, limit + 1)
        if before.rows.get(row, "") != after.rows.get(row, "")
    )


def scan_tree_for_bytes(root: Path, needle: bytes) -> list[str]:
    matches: list[str] = []
    for path in sorted(root.rglob("*")):
        if not path.is_file():
            continue
        try:
            if needle in path.read_bytes():
                matches.append(str(path))
        except OSError:
            continue
    return matches


def daemon_transport_diagnosis(profile_dir: Path) -> str:
    """Name the last connection-retirement fact behind a PTY transport loss."""

    path = profile_dir / "daemon.log"
    if not path.exists():
        return "daemon_transport=log_absent"
    try:
        lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
    except OSError as error:
        return f"daemon_transport=log_read_error:{error}"
    for line in reversed(lines):
        if "connection_retired" in line:
            return "daemon_transport=" + line.strip()
    return "daemon_transport=no_connection_retired_row"


def session_json(ctx, session_id: str) -> dict[str, Any]:
    result = ctx.run_haider(
        ["session", session_id, "--json", "--no-spawn"], timeout=STATUS_REQUEST
    )
    if result.timed_out or result.returncode != 0:
        raise RuntimeError(
            f"session JSON exit={result.returncode} timed_out={str(result.timed_out).lower()} "
            f"stderr={result.stderr.strip()!r}"
        )
    return parse_single_json(result.stdout, f"session {session_id}")
