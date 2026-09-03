#!/usr/bin/env python3
"""Measure and guard settled `haider` client physical footprint on macOS.

Calibration is deliberately stricter than the routine CI guard: it requires
five independent samples, a 60-second settle, and host load below four both
before spawn and immediately before each reading. Every accepted sample uses
the authoritative `proc_pid_rusage` values; registry #44 records that `vmmap`
is sandbox-denied and is deliberately not retried.

The graphics TUI surface emulates only terminal capability replies.  The child
still performs the real PNG decode, ratatui-image resize/encode, Ratatui buffer
render, and terminal write, so the retained Sixel allocation is measured.
"""

from __future__ import annotations

import argparse
from collections.abc import Mapping
import ctypes
import ctypes.util
import fcntl
import importlib
import json
import math
import os
from pathlib import Path
import pty
import select
import shutil
import signal
import statistics
import struct
import subprocess
import sys
import tempfile
import termios
import time
from urllib.parse import urlsplit


RUSAGE_INFO_V4 = 4
PROC_PIDTASKINFO = 4
DEFAULT_LOAD_LIMIT = 4.0
DEFAULT_SETTLE_SECONDS = 60.0
CALIBRATION_RUNS = 5
BUDGET_HEADROOM_PERCENT = 10
HOLD_MARGIN_SECONDS = 15
SIXEL_RESPONSE = b"\x1b[?64;4c\x1b[6;20;10t\x1b[0n"
WIRE_PROVIDER = "footprint-proxy"
WIRE_MODEL = "fixture-model"
PROXY_ENV_KEYS = (
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
)
LOOPBACK_NO_PROXY = "127.0.0.1,localhost"
HEADLESS_RUN_TIMEOUT_SECONDS = 45
# The observation deadline is derived as 2 * the product timeout. This leaves
# one full product-timeout window for cold spawn, terminal publication, and CI
# scheduling after the product has committed to a typed outcome (registry #94).
HEADLESS_TERMINAL_DEADLINE_SECONDS = HEADLESS_RUN_TIMEOUT_SECONDS * 2
STUB_PROBE_IO_TIMEOUT_SECONDS = 5
# One I/O timeout plus the same allowance for interpreter startup and exit.
STUB_PROBE_DEADLINE_SECONDS = STUB_PROBE_IO_TIMEOUT_SECONDS * 2
DIAGNOSTIC_SECTION_CHARS = 750
TUI_CPU_TURNS = 20
# A TUI turn uses the same run engine as the headless surface. Reuse the
# already-derived terminal observation deadline (2 * the 45s product timeout)
# rather than inventing a second ungrounded timeout (registry #94).
TUI_TURN_TIMEOUT_SECONDS = HEADLESS_TERMINAL_DEADLINE_SECONDS
TUI_TURN_QUIESCE_SECONDS = 0.25


class RusageInfoV4(ctypes.Structure):
    _fields_ = [("ri_uuid", ctypes.c_ubyte * 16)] + [
        (name, ctypes.c_uint64)
        for name in (
            "ri_user_time",
            "ri_system_time",
            "ri_pkg_idle_wkups",
            "ri_interrupt_wkups",
            "ri_pageins",
            "ri_wired_size",
            "ri_resident_size",
            "ri_phys_footprint",
            "ri_proc_start_abstime",
            "ri_proc_exit_abstime",
            "ri_child_user_time",
            "ri_child_system_time",
            "ri_child_pkg_idle_wkups",
            "ri_child_interrupt_wkups",
            "ri_child_pageins",
            "ri_child_elapsed_abstime",
            "ri_diskio_bytesread",
            "ri_diskio_byteswritten",
            "ri_cpu_time_qos_default",
            "ri_cpu_time_qos_maintenance",
            "ri_cpu_time_qos_background",
            "ri_cpu_time_qos_utility",
            "ri_cpu_time_qos_legacy",
            "ri_cpu_time_qos_user_initiated",
            "ri_cpu_time_qos_user_interactive",
            "ri_billed_system_time",
            "ri_serviced_system_time",
            "ri_logical_writes",
            "ri_lifetime_max_phys_footprint",
            "ri_instructions",
            "ri_cycles",
            "ri_billed_energy",
            "ri_serviced_energy",
            "ri_interval_max_phys_footprint",
            "ri_runnable_time",
        )
    ]


class ProcTaskInfo(ctypes.Structure):
    _fields_ = [
        ("virtual_size", ctypes.c_uint64),
        ("resident_size", ctypes.c_uint64),
        ("total_user", ctypes.c_uint64),
        ("total_system", ctypes.c_uint64),
        ("threads_user", ctypes.c_uint64),
        ("threads_system", ctypes.c_uint64),
        ("policy", ctypes.c_int32),
        ("faults", ctypes.c_int32),
        ("pageins", ctypes.c_int32),
        ("cow_faults", ctypes.c_int32),
        ("messages_sent", ctypes.c_int32),
        ("messages_received", ctypes.c_int32),
        ("syscalls_mach", ctypes.c_int32),
        ("syscalls_unix", ctypes.c_int32),
        ("csw", ctypes.c_int32),
        ("threadnum", ctypes.c_int32),
        ("numrunning", ctypes.c_int32),
        ("priority", ctypes.c_int32),
    ]


class MachTimebaseInfo(ctypes.Structure):
    _fields_ = [("numer", ctypes.c_uint32), ("denom", ctypes.c_uint32)]


class DarwinProcessMetrics:
    def __init__(self) -> None:
        library = ctypes.util.find_library("proc")
        if not library:
            raise RuntimeError("libproc is unavailable; client footprint requires macOS")
        self.lib = ctypes.CDLL(library, use_errno=True)
        self.lib.proc_pid_rusage.argtypes = [
            ctypes.c_int,
            ctypes.c_int,
            ctypes.c_void_p,
        ]
        self.lib.proc_pid_rusage.restype = ctypes.c_int
        self.lib.proc_pidinfo.argtypes = [
            ctypes.c_int,
            ctypes.c_int,
            ctypes.c_uint64,
            ctypes.c_void_p,
            ctypes.c_int,
        ]
        self.lib.proc_pidinfo.restype = ctypes.c_int
        system = ctypes.CDLL(None)
        system.mach_timebase_info.argtypes = [ctypes.POINTER(MachTimebaseInfo)]
        system.mach_timebase_info.restype = ctypes.c_int
        timebase = MachTimebaseInfo()
        if system.mach_timebase_info(ctypes.byref(timebase)) != 0 or timebase.denom == 0:
            raise RuntimeError("mach_timebase_info is unavailable")
        self.timebase_numer = int(timebase.numer)
        self.timebase_denom = int(timebase.denom)

    def ticks_to_ns(self, ticks: int) -> int:
        return ticks * self.timebase_numer // self.timebase_denom

    def read(self, pid: int) -> dict[str, int]:
        usage = RusageInfoV4()
        if self.lib.proc_pid_rusage(pid, RUSAGE_INFO_V4, ctypes.byref(usage)) != 0:
            error = ctypes.get_errno()
            raise OSError(error, os.strerror(error), pid)
        task = ProcTaskInfo()
        task_size = ctypes.sizeof(task)
        task_result = self.lib.proc_pidinfo(
            pid, PROC_PIDTASKINFO, 0, ctypes.byref(task), task_size
        )
        if task_result != task_size:
            error = ctypes.get_errno()
            raise OSError(error, os.strerror(error), pid)
        user_ticks = int(usage.ri_user_time)
        system_ticks = int(usage.ri_system_time)
        user_ns = self.ticks_to_ns(user_ticks)
        system_ns = self.ticks_to_ns(system_ticks)
        return {
            "phys_footprint_bytes": int(usage.ri_phys_footprint),
            "resident_bytes": int(usage.ri_resident_size),
            "lifetime_max_phys_footprint_bytes": int(
                usage.ri_lifetime_max_phys_footprint
            ),
            "user_time_raw_ticks": user_ticks,
            "system_time_raw_ticks": system_ticks,
            "cpu_total_raw_ticks": user_ticks + system_ticks,
            "user_time_ns": user_ns,
            "system_time_ns": system_ns,
            "cpu_total_ns": user_ns + system_ns,
            "user_time_us": user_ns // 1_000,
            "system_time_us": system_ns // 1_000,
            "cpu_total_us": (user_ns + system_ns) // 1_000,
            "task_user_time_raw_ticks": int(task.total_user),
            "task_system_time_raw_ticks": int(task.total_system),
            "task_cpu_total_raw_ticks": int(task.total_user + task.total_system),
            "task_user_time_ns": self.ticks_to_ns(int(task.total_user)),
            "task_system_time_ns": self.ticks_to_ns(int(task.total_system)),
            "task_cpu_total_ns": self.ticks_to_ns(
                int(task.total_user + task.total_system)
            ),
            "task_cpu_total_us": self.ticks_to_ns(
                int(task.total_user + task.total_system)
            )
            // 1_000,
            "task_resident_bytes": int(task.resident_size),
            "threads": int(task.threadnum),
        }


def wait_for_load(limit: float, timeout: float) -> float:
    deadline = time.monotonic() + timeout
    while True:
        load = float(os.getloadavg()[0])
        if load < limit:
            return load
        if time.monotonic() >= deadline:
            raise RuntimeError(
                f"ENVIRONMENT-BLOCKED load_1m={load:.2f} limit={limit:.2f}"
            )
        time.sleep(5.0)


def require_load_below(limit: float, stage: str) -> float:
    load = float(os.getloadavg()[0])
    if load >= limit:
        raise RuntimeError(
            f"ENVIRONMENT-BLOCKED {stage} load_1m={load:.2f} limit={limit:.2f}"
        )
    return load


def hermetic_env(root: Path) -> dict[str, str]:
    env = os.environ.copy()
    for key in tuple(env):
        if key.startswith("HAIDER_") or key.endswith(("_API_KEY", "_TOKEN", "_SECRET")):
            env.pop(key, None)
    for key in PROXY_ENV_KEYS:
        env.pop(key, None)
    for key in ("NO_COLOR", "CLICOLOR", "CLICOLOR_FORCE", "FORCE_COLOR", "COLORTERM"):
        env.pop(key, None)
    profile = root / "p"
    runtime = root / "r"
    home = root / "h"
    workspace = root / "w"
    for directory in (profile, runtime, home, workspace):
        directory.mkdir(mode=0o700)
    env.update(
        {
            "HAIDER_PROFILE_DIR": str(profile),
            "HAIDER_RUNTIME_DIR": str(runtime),
            "HAIDER_DISCOVERY_DISABLED": "1",
            "HAIDER_NO_UPDATE_CHECK": "1",
            "HAIDER_TEST_DEVICE_NAME": "test-mac",
            "HOME": str(home),
            "USERPROFILE": str(home),
            "XDG_CACHE_HOME": str(home / ".cache"),
            "XDG_CONFIG_HOME": str(home / ".config"),
            "XDG_DATA_HOME": str(home / ".local" / "share"),
            "XDG_STATE_HOME": str(home / ".local" / "state"),
            "TMPDIR": str(root),
            "TERM": "xterm-256color",
            "NO_PROXY": LOOPBACK_NO_PROXY,
            "no_proxy": LOOPBACK_NO_PROXY,
        }
    )
    return env


def require_ipv4_loopback_base_url(base_url: str) -> None:
    parsed = urlsplit(base_url)
    if (
        parsed.scheme != "http"
        or parsed.hostname != "127.0.0.1"
        or parsed.port is None
    ):
        raise RuntimeError(
            "client footprint stub must advertise an exact IPv4 loopback URL; "
            f"got {base_url!r}"
        )


def verify_stub_reachable(
    base_url: str, env: dict[str, str], artefact_dir: Path
) -> None:
    require_ipv4_loopback_base_url(base_url)
    probe_url = f"{base_url.rstrip('/')}/models"
    probe = subprocess.run(
        [
            sys.executable,
            "-I",
            "-c",
            (
                "import sys, urllib.request; "
                "response = urllib.request.urlopen("
                f"sys.argv[1], timeout={STUB_PROBE_IO_TIMEOUT_SECONDS}); "
                "print(response.status); response.close()"
            ),
            probe_url,
        ],
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        timeout=STUB_PROBE_DEADLINE_SECONDS,
        check=False,
    )
    evidence = {
        "url": probe_url,
        "exit": probe.returncode,
        "stdout": probe.stdout,
        "stderr": probe.stderr,
    }
    (artefact_dir / "stub-reachability.json").write_text(
        json.dumps(evidence, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    if probe.returncode != 0 or probe.stdout.strip() != "200":
        raise RuntimeError(
            "loopback stub subprocess probe failed "
            f"exit={probe.returncode} stdout={probe.stdout!r} stderr={probe.stderr!r}"
        )


def wait_pid(pid: int, timeout: float) -> int | None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        done, status = os.waitpid(pid, os.WNOHANG)
        if done:
            return status
        time.sleep(0.05)
    return None


def terminate_pty_child(pid: int, fd: int) -> None:
    try:
        os.write(fd, b"\x03\x03\x03")
    except OSError:
        pass
    if wait_pid(pid, 3.0) is not None:
        return
    for requested_signal in (signal.SIGTERM, signal.SIGKILL):
        try:
            os.kill(pid, requested_signal)
        except ProcessLookupError:
            return
        if wait_pid(pid, 2.0) is not None:
            return


def read_cpu_at_calibrated_load(
    metrics: DarwinProcessMetrics,
    pid: int,
    load_limit: float,
    stage: str,
) -> dict[str, int]:
    """Gate load immediately before a process CPU clock reading."""
    require_load_below(load_limit, stage)
    return metrics.read(pid)


def drive_tui_turns(
    fd: int,
    pid: int,
    turns: int,
    metrics: DarwinProcessMetrics,
    tail: bytearray,
    load_limit: float,
) -> tuple[dict[str, int], int]:
    """Drive the fixed demo workload and return exact process CPU deltas."""
    if turns == 0:
        return {}, 0
    before = read_cpu_at_calibrated_load(
        metrics, pid, load_limit, "before-tui-turn-cpu"
    )
    output_bytes = 0
    for turn in range(1, turns + 1):
        os.write(fd, f"memclient workload {turn}\r".encode("ascii"))
        turn_tail = bytearray()
        deadline = time.monotonic() + TUI_TURN_TIMEOUT_SECONDS
        thinking_at: int | None = None
        while True:
            if time.monotonic() >= deadline:
                raise RuntimeError(
                    f"TUI turn {turn}/{turns} missed its derived "
                    f"{TUI_TURN_TIMEOUT_SECONDS}s terminal deadline "
                    f"thinking_seen={thinking_at is not None}"
                )
            ready, _, _ = select.select([fd], [], [], 0.05)
            if not ready:
                continue
            chunk = os.read(fd, 65536)
            if not chunk:
                raise RuntimeError(f"TUI exited during turn {turn}/{turns}")
            output_bytes += len(chunk)
            turn_tail.extend(chunk)
            tail.extend(chunk)
            if len(turn_tail) > 1_000_000:
                del turn_tail[:-1_000_000]
            if len(tail) > 1_000_000:
                del tail[:-1_000_000]
            if thinking_at is None:
                marker = turn_tail.find(b"THINKING")
                if marker >= 0:
                    thinking_at = marker
            if thinking_at is not None and turn_tail.find(b"IDLE", thinking_at + 8) >= 0:
                break

        # One quarter-second is longer than seven 33ms frame cadences. Drain
        # those bytes so the next turn cannot match this turn's terminal.
        quiet_deadline = time.monotonic() + TUI_TURN_QUIESCE_SECONDS
        while time.monotonic() < quiet_deadline:
            ready, _, _ = select.select([fd], [], [], 0.01)
            if not ready:
                continue
            chunk = os.read(fd, 65536)
            if not chunk:
                raise RuntimeError(f"TUI exited while quiescing turn {turn}/{turns}")
            output_bytes += len(chunk)
            tail.extend(chunk)
            if len(tail) > 1_000_000:
                del tail[:-1_000_000]

    after = read_cpu_at_calibrated_load(
        metrics, pid, load_limit, "after-tui-turn-cpu"
    )
    return (
        {
            "turn_20_cpu_raw_ticks": int(after["cpu_total_raw_ticks"])
            - int(before["cpu_total_raw_ticks"]),
            "turn_20_cpu_ns": int(after["cpu_total_ns"]) - int(before["cpu_total_ns"]),
            "turn_20_cpu_us": int(after["cpu_total_us"])
            - int(before["cpu_total_us"]),
            "turns_completed": turns,
        },
        output_bytes,
    )


def measure_tui(
    *,
    haider: Path,
    env: dict[str, str],
    graphics: bool,
    settle_seconds: float,
    load_limit: float,
    load_wait_seconds: float,
    metrics: DarwinProcessMetrics,
    artefact_dir: Path,
    tui_turns: int = 0,
) -> dict[str, object]:
    pid, fd = pty.fork()
    if pid == 0:
        os.environ.clear()
        os.environ.update(env)
        if graphics:
            os.environ["TERM_PROGRAM"] = "rio"
        os.execve(str(haider), [str(haider), "tui", "--demo"], os.environ)

    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 36, 118, 0, 0))
    os.kill(pid, signal.SIGWINCH)

    response_sent = False
    query_seen = False
    alt_screen_seen = False
    bytes_seen = 0
    tail = bytearray()
    settle_start: float | None = None
    startup_deadline = time.monotonic() + 30.0
    try:
        while settle_start is None:
            if time.monotonic() >= startup_deadline:
                raise RuntimeError(
                    "TUI did not render a complete first frame before startup deadline "
                    f"bytes={bytes_seen} alt={alt_screen_seen} query={query_seen} "
                    f"response={response_sent}"
                )
            ready, _, _ = select.select([fd], [], [], 0.05)
            if not ready:
                continue
            chunk = os.read(fd, 65536)
            if not chunk:
                raise RuntimeError("TUI exited before first frame")
            bytes_seen += len(chunk)
            tail.extend(chunk)
            if len(tail) > 1_000_000:
                del tail[:-1_000_000]
            alt_screen_seen = alt_screen_seen or b"\x1b[?1049h" in chunk
            query_seen = query_seen or b"\x1b[16t" in chunk
            if graphics and query_seen and not response_sent:
                os.write(fd, SIXEL_RESPONSE)
                response_sent = True
            if not graphics and alt_screen_seen and bytes_seen >= 16_384:
                settle_start = time.monotonic()
            # icy_sixel begins its real payload with a DCS (`ESC P`).  Wait
            # for that marker rather than guessing encoded byte size: the
            # retained allocator capacity can be MiB even when this sparse
            # wordmark compresses to only a few KiB on the wire.
            if graphics and response_sent and b"\x1bP" in tail:
                settle_start = time.monotonic()

        settle_deadline = settle_start + settle_seconds
        while time.monotonic() < settle_deadline:
            ready, _, _ = select.select([fd], [], [], 0.05)
            if not ready:
                continue
            chunk = os.read(fd, 65536)
            if not chunk:
                raise RuntimeError("TUI exited during settle")
            bytes_seen += len(chunk)
            tail.extend(chunk)
            if len(tail) > 1_000_000:
                del tail[:-1_000_000]

        turn_metrics, turn_output_bytes = drive_tui_turns(
            fd, pid, tui_turns, metrics, tail, load_limit
        )
        bytes_seen += turn_output_bytes
        load_before_read = require_load_below(load_limit, "before-read")
        sample = metrics.read(pid)
        sample.update(turn_metrics)
        sample.update(
            {
                "pid": pid,
                "load_before_read": load_before_read,
                "output_bytes": bytes_seen,
                "graphics_query_seen": query_seen,
                "graphics_response_sent": response_sent,
                "alt_screen_seen": alt_screen_seen,
            }
        )
        (artefact_dir / "pty-tail.bin").write_bytes(bytes(tail))
        return sample
    finally:
        (artefact_dir / "pty-tail.bin").write_bytes(bytes(tail))
        terminate_pty_child(pid, fd)
        os.close(fd)


def stop_profile_daemon(haider: Path, env: dict[str, str]) -> None:
    subprocess.run(
        [str(haider), "daemon", "stop", "--json", "--timeout", "15s"],
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=25,
        check=False,
    )


def ensure_profile_daemon_ready(haider: Path, env: dict[str, str]) -> None:
    ready = subprocess.run(
        [str(haider), "--ready"],
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        timeout=45,
        check=False,
    )
    if ready.returncode != 0:
        raise RuntimeError(
            f"daemon readiness failed exit={ready.returncode} stderr={ready.stderr!r}"
        )


def configure_wire_fixture(env: dict[str, str], base_url: str) -> None:
    profile = Path(env["HAIDER_PROFILE_DIR"])
    providers = {
        "providers": [
            {
                "provider_id": WIRE_PROVIDER,
                "display_name": "client footprint proxy",
                "api_family": "openai_chat_completions",
                "base_url": base_url,
                "enabled": True,
                "auth_requirement": "api_key",
                "configured_models": [WIRE_MODEL],
                "default_model": WIRE_MODEL,
                "provenance": "custom",
            }
        ]
    }
    accounts = [
        {
            "alias": WIRE_PROVIDER,
            "provider": WIRE_PROVIDER,
            "auth_method": "api_key",
            "identity": "client footprint proxy",
            "status": {"status": "ok"},
            "active": True,
        }
    ]
    providers_path = profile / "providers.json"
    accounts_path = profile / "accounts.json"
    vault_path = profile / "vault" / f"{WIRE_PROVIDER.encode().hex()}.vault"
    vault_path.parent.mkdir(mode=0o700)
    providers_path.write_text(
        json.dumps(providers, separators=(",", ":")) + "\n", encoding="utf-8"
    )
    accounts_path.write_text(
        json.dumps(accounts, separators=(",", ":")) + "\n", encoding="utf-8"
    )
    vault_path.write_text("client-footprint-secret", encoding="utf-8")
    for path in (providers_path, accounts_path, vault_path):
        path.chmod(0o600)


def openai_stub() -> object:
    qa_gate = Path(__file__).resolve().parents[1] / "qa-gate"
    sys.path.insert(0, str(qa_gate))
    module = importlib.import_module("gate.openai_stub")
    stub = module.OpenAIStub("client-footprint-ok", WIRE_MODEL).start()
    try:
        require_ipv4_loopback_base_url(stub.base_url)
    except Exception:
        stub.close()
        raise
    return stub


def read_headless_terminal(
    child: subprocess.Popen[bytes], stdout_parts: list[bytes]
) -> dict[str, object]:
    assert child.stdout is not None
    pending = bytearray()
    deadline = time.monotonic() + HEADLESS_TERMINAL_DEADLINE_SECONDS
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise RuntimeError(
                "headless run did not emit a terminal within "
                f"{HEADLESS_TERMINAL_DEADLINE_SECONDS} seconds"
            )
        ready, _, _ = select.select([child.stdout], [], [], min(remaining, 0.1))
        if not ready:
            continue
        chunk = os.read(child.stdout.fileno(), 65_536)
        if not chunk:
            raise RuntimeError(
                "headless run closed stdout before its terminal "
                f"exit={child.poll()}"
            )
        stdout_parts.append(chunk)
        pending.extend(chunk)
        while b"\n" in pending:
            raw_line, _, remainder = pending.partition(b"\n")
            pending = bytearray(remainder)
            if not raw_line.strip():
                continue
            document = json.loads(raw_line.decode("utf-8"))
            payload = document.get("payload")
            if isinstance(payload, dict) and payload.get("terminal_kind") is not None:
                return document


def require_successful_headless_terminal(terminal: dict[str, object]) -> None:
    terminal_payload = terminal.get("payload")
    if not isinstance(terminal_payload, dict):
        raise RuntimeError(f"headless terminal has no payload: {terminal!r}")
    if terminal_payload.get("terminal_kind") != "success":
        raise RuntimeError(
            "headless fixture emitted a typed terminal but the footprint "
            f"surface requires success: {terminal_payload!r}"
        )


def collect_child_output(
    child: subprocess.Popen[bytes], stdout_parts: list[bytes], stderr_path: Path
) -> tuple[str, str, int | None]:
    if child.poll() is None:
        try:
            os.killpg(child.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
    try:
        remaining_stdout, _ = child.communicate(timeout=5)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(child.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        remaining_stdout, _ = child.communicate(timeout=5)
    if remaining_stdout:
        stdout_parts.append(remaining_stdout)
    stdout = b"".join(stdout_parts).decode("utf-8", "replace")
    try:
        stderr = stderr_path.read_text(encoding="utf-8", errors="replace")
    except FileNotFoundError:
        stderr = ""
    return stdout, stderr, child.returncode


def snapshot_daemon_logs(
    env: dict[str, str], artefact_dir: Path
) -> list[Path]:
    profile = Path(env["HAIDER_PROFILE_DIR"])
    candidates = [profile / "daemon.log"]
    daemon_log_dir = profile / "daemon-logs"
    if daemon_log_dir.is_dir():
        candidates.extend(sorted(daemon_log_dir.glob("*.log")))
    destination = artefact_dir / "daemon-logs"
    copied: list[Path] = []
    errors: list[str] = []
    for source in candidates:
        if not source.is_file():
            continue
        target = destination / source.name
        try:
            destination.mkdir(parents=True, exist_ok=True)
            target.write_bytes(source.read_bytes())
            copied.append(target)
        except OSError as error:
            errors.append(f"{source}: {error}")
    if errors:
        destination.mkdir(parents=True, exist_ok=True)
        (destination / "copy-errors.txt").write_text(
            "\n".join(errors) + "\n", encoding="utf-8"
        )
    return copied


def diagnostic_tail(text: str) -> str:
    stripped = text.strip()
    if not stripped:
        return "(empty)"
    return stripped[-DIAGNOSTIC_SECTION_CHARS:]


def persist_run_failure_diagnostics(
    *,
    artefact_dir: Path,
    env: dict[str, str],
    stub: object,
    failure: BaseException,
    child_stdout: str,
    child_stderr: str,
    child_exit: int | None,
    terminal: dict[str, object] | None,
    cleanup_errors: list[str],
) -> None:
    (artefact_dir / "run.stdout").write_text(child_stdout, encoding="utf-8")
    (artefact_dir / "run.stderr").write_text(child_stderr, encoding="utf-8")
    requests = list(getattr(stub, "requests", getattr(stub, "chat_requests", [])))
    stub_evidence = {
        "request_count": len(requests),
        "chat_request_count": int(getattr(stub, "chat_count", 0)),
        "requests": requests,
    }
    (artefact_dir / "stub-requests.json").write_text(
        json.dumps(stub_evidence, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    copied_logs = snapshot_daemon_logs(env, artefact_dir)
    terminal_payload = terminal.get("payload") if terminal is not None else None
    terminal_kind = (
        terminal_payload.get("terminal_kind")
        if isinstance(terminal_payload, dict)
        else None
    )
    failure_evidence = {
        "error_type": type(failure).__name__,
        "error": str(failure),
        "child_exit": child_exit,
        "terminal_seen": terminal is not None,
        "terminal_kind": terminal_kind,
        "daemon_logs": [str(path.relative_to(artefact_dir)) for path in copied_logs],
        "cleanup_errors": cleanup_errors,
    }
    (artefact_dir / "failure.json").write_text(
        json.dumps(failure_evidence, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    daemon_excerpt = "\n".join(
        f"[{path.name}]\n{diagnostic_tail(path.read_text(encoding='utf-8', errors='replace'))}"
        for path in copied_logs
    )
    request_excerpt = "\n".join(
        f"{request.get('method', '?')} {request.get('path', '?')}"
        for request in requests
        if isinstance(request, dict)
    )
    excerpt = "\n".join(
        (
            f"failure={type(failure).__name__}: {failure}",
            f"child_exit={child_exit} terminal_seen={terminal is not None} "
            f"terminal_kind={terminal_kind}",
            f"stub_requests={len(requests)} chat_requests={stub_evidence['chat_request_count']}",
            "stub request log:\n" + diagnostic_tail(request_excerpt),
            "child stdout tail:\n" + diagnostic_tail(child_stdout),
            "child stderr tail:\n" + diagnostic_tail(child_stderr),
            "daemon log tail:\n" + diagnostic_tail(daemon_excerpt),
        )
    )
    (artefact_dir / "diagnostic-excerpt.txt").write_text(
        excerpt + "\n", encoding="utf-8"
    )
    print(
        f"client-footprint diagnostics ({artefact_dir}):\n{excerpt}",
        file=sys.stderr,
        flush=True,
    )


def measure_status(
    *,
    haider: Path,
    env: dict[str, str],
    settle_seconds: float,
    load_limit: float,
    load_wait_seconds: float,
    metrics: DarwinProcessMetrics,
    artefact_dir: Path,
) -> dict[str, object]:
    ensure_profile_daemon_ready(haider, env)
    held_env = env.copy()
    held_env["HAIDER_CLIENT_FOOTPRINT_HOLD_MS"] = str(
        math.ceil((settle_seconds + HOLD_MARGIN_SECONDS) * 1000)
    )
    child = subprocess.Popen(
        [str(haider), "status", "--json", "--no-spawn"],
        env=held_env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        start_new_session=True,
    )
    try:
        assert child.stdout is not None
        line = child.stdout.readline()
        document = json.loads(line)
        if document.get("schema") != "haider.observe.v1" or document.get("kind") != "status":
            raise RuntimeError(f"unexpected status document: {document!r}")
        settle_deadline = time.monotonic() + settle_seconds
        while time.monotonic() < settle_deadline:
            if child.poll() is not None:
                raise RuntimeError("held status process exited during settle")
            time.sleep(0.1)
        load_before_read = require_load_below(load_limit, "before-read")
        sample = metrics.read(child.pid)
        sample.update(
            {
                "pid": child.pid,
                "load_before_read": load_before_read,
                "status_daemon_pid": document.get("daemon", {}).get("pid"),
            }
        )
        (artefact_dir / "status.json").write_text(line, encoding="utf-8")
        return sample
    finally:
        if child.poll() is None:
            os.killpg(child.pid, signal.SIGTERM)
        try:
            child.communicate(timeout=5)
        except subprocess.TimeoutExpired:
            os.killpg(child.pid, signal.SIGKILL)
            child.communicate(timeout=5)
        stop_profile_daemon(haider, env)


def measure_run(
    *,
    haider: Path,
    env: dict[str, str],
    settle_seconds: float,
    load_limit: float,
    load_wait_seconds: float,
    metrics: DarwinProcessMetrics,
    artefact_dir: Path,
) -> dict[str, object]:
    del load_wait_seconds
    stub = openai_stub()
    configure_wire_fixture(env, stub.base_url)
    child: subprocess.Popen[bytes] | None = None
    stdout_parts: list[bytes] = []
    stderr_path = artefact_dir / "run.stderr"
    terminal: dict[str, object] | None = None
    failure: BaseException | None = None
    child_stdout = ""
    child_stderr = ""
    child_exit: int | None = None
    daemon_stopped = False
    cleanup_errors: list[str] = []
    try:
        verify_stub_reachable(stub.base_url, env, artefact_dir)
        ensure_profile_daemon_ready(haider, env)
        held_env = env.copy()
        held_env["HAIDER_CLIENT_FOOTPRINT_HOLD_MS"] = str(
            math.ceil((settle_seconds + HOLD_MARGIN_SECONDS) * 1000)
        )
        stderr_handle = stderr_path.open("wb")
        try:
            child = subprocess.Popen(
                [
                    str(haider),
                    "run",
                    "client footprint fixture",
                    "--output",
                    "jsonl",
                    "--timeout",
                    f"{HEADLESS_RUN_TIMEOUT_SECONDS}s",
                    "--provider",
                    WIRE_PROVIDER,
                    "--model",
                    WIRE_MODEL,
                ],
                env=held_env,
                stdout=subprocess.PIPE,
                stderr=stderr_handle,
                bufsize=0,
                start_new_session=True,
            )
        finally:
            stderr_handle.close()
        terminal = read_headless_terminal(child, stdout_parts)
        (artefact_dir / "run.jsonl").write_bytes(b"".join(stdout_parts))
        require_successful_headless_terminal(terminal)
        if stub.chat_count != 1:
            raise RuntimeError(f"headless fixture saw {stub.chat_count} provider requests")
        settle_deadline = time.monotonic() + settle_seconds
        while time.monotonic() < settle_deadline:
            returncode = child.poll()
            if returncode is not None:
                stderr = stderr_path.read_text(encoding="utf-8", errors="replace")
                raise RuntimeError(
                    f"held headless run exited during settle exit={returncode} "
                    f"stderr={stderr!r}"
                )
            time.sleep(0.1)
        load_before_read = require_load_below(load_limit, "before-read")
        sample = metrics.read(child.pid)
        sample.update(
            {
                "pid": child.pid,
                "load_before_read": load_before_read,
                "provider_requests": stub.chat_count,
                "terminal_seq": terminal.get("seq"),
            }
        )
        return sample
    except Exception as error:
        failure = error
        raise
    finally:
        if child is not None:
            try:
                child_stdout, child_stderr, child_exit = collect_child_output(
                    child, stdout_parts, stderr_path
                )
            except (OSError, subprocess.TimeoutExpired) as error:
                cleanup_errors.append(f"collect child output: {error}")
        try:
            stop_profile_daemon(haider, env)
            daemon_stopped = True
        except (OSError, subprocess.TimeoutExpired) as error:
            cleanup_errors.append(f"stop profile daemon: {error}")
        if failure is not None:
            try:
                persist_run_failure_diagnostics(
                    artefact_dir=artefact_dir,
                    env=env,
                    stub=stub,
                    failure=failure,
                    child_stdout=child_stdout,
                    child_stderr=child_stderr,
                    child_exit=child_exit,
                    terminal=terminal,
                    cleanup_errors=cleanup_errors,
                )
            except (OSError, TypeError, ValueError) as error:
                print(
                    f"client-footprint: could not persist complete diagnostics: {error}",
                    file=sys.stderr,
                    flush=True,
                )
        try:
            stub.close()
        except Exception as error:
            if failure is None:
                raise
            print(
                f"client-footprint: stub cleanup after failure also failed: {error}",
                file=sys.stderr,
                flush=True,
            )
        if not daemon_stopped and failure is None:
            stop_profile_daemon(haider, env)


def median_absolute_deviation(values: list[int]) -> float:
    median = statistics.median(values)
    return float(statistics.median(abs(value - median) for value in values))


def runner_context(environment: Mapping[str, str]) -> dict[str, str]:
    fields = (
        ("github_run_id", "GITHUB_RUN_ID"),
        ("github_run_attempt", "GITHUB_RUN_ATTEMPT"),
        ("github_sha", "GITHUB_SHA"),
        ("runner_os", "RUNNER_OS"),
        ("runner_arch", "RUNNER_ARCH"),
        ("runner_name", "RUNNER_NAME"),
        ("image_os", "ImageOS"),
        ("image_version", "ImageVersion"),
    )
    return {
        field: value
        for field, variable in fields
        if (value := environment.get(variable)) is not None
    }


def budget_from_median(median_bytes: int | float) -> int:
    return math.ceil(median_bytes * (100 + BUDGET_HEADROOM_PERCENT) / 100)


def summarize(
    surface: str,
    samples: list[dict[str, object]],
    *,
    calibration: bool = False,
    environment: Mapping[str, str] = os.environ,
) -> dict[str, object]:
    footprint = [int(sample["phys_footprint_bytes"]) for sample in samples]
    cpu = [int(sample["cpu_total_us"]) for sample in samples]
    footprint_median = statistics.median(footprint)
    summary: dict[str, object] = {
        "surface": surface,
        "runs": len(samples),
        "mode": "calibration" if calibration else "guard",
        "run_context": runner_context(environment),
        "measurement_accepted": True,
        "phys_footprint_bytes": {
            "min": min(footprint),
            "median": footprint_median,
            "max": max(footprint),
            "mad": median_absolute_deviation(footprint),
        },
        "cpu_total_us": {
            "min": min(cpu),
            "median": statistics.median(cpu),
            "max": max(cpu),
            "mad": median_absolute_deviation(cpu),
        },
        "budget_basis": {
            "metric": "phys_footprint_bytes.median",
            "headroom_percent": BUDGET_HEADROOM_PERCENT,
            "formula": "ceil(median * 1.10)",
        },
        "derived_budget_bytes": budget_from_median(footprint_median),
        "samples": samples,
    }
    if all("turn_20_cpu_ns" in sample for sample in samples):
        turn_cpu = [int(sample["turn_20_cpu_ns"]) for sample in samples]
        summary["turn_20_cpu_ns"] = {
            "min": min(turn_cpu),
            "median": statistics.median(turn_cpu),
            "max": max(turn_cpu),
            "mad": median_absolute_deviation(turn_cpu),
        }
    return summary


def self_test() -> int:
    samples: list[dict[str, object]] = [
        {"phys_footprint_bytes": 900, "cpu_total_us": 10},
        {"phys_footprint_bytes": 1_000, "cpu_total_us": 20},
        {"phys_footprint_bytes": 1_100, "cpu_total_us": 30},
    ]
    summary = summarize("self-test", samples)
    if summary["phys_footprint_bytes"] != {
        "min": 900,
        "median": 1_000,
        "max": 1_100,
        "mad": 100.0,
    }:
        raise RuntimeError(f"summary arithmetic drifted: {summary!r}")
    if summary["derived_budget_bytes"] != 1_100:
        raise RuntimeError(f"budget arithmetic drifted: {summary!r}")

    metrics = DarwinProcessMetrics()
    with subprocess.Popen(["/bin/sleep", "2"]) as child:
        sample = metrics.read(child.pid)
        child.terminate()
    if sample["phys_footprint_bytes"] <= 0 or sample["threads"] < 1:
        raise RuntimeError(f"libproc returned implausible process metrics: {sample!r}")
    print(
        "client-footprint self-test: PASS "
        f"footprint={sample['phys_footprint_bytes']} threads={sample['threads']}"
    )
    return 0


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--haider", type=Path)
    parser.add_argument(
        "--surface",
        choices=(
            "run-post-command",
            "status-post-command",
            "tui-demo-no-graphics",
            "tui-demo-sixel",
        ),
    )
    parser.add_argument("--runs", type=int, default=1)
    parser.add_argument("--tui-turns", type=int, default=0)
    parser.add_argument("--settle-seconds", type=float, default=DEFAULT_SETTLE_SECONDS)
    parser.add_argument("--load-limit", type=float, default=DEFAULT_LOAD_LIMIT)
    parser.add_argument("--load-wait-seconds", type=float, default=15 * 60)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--budget-bytes", type=int)
    parser.add_argument("--calibrate", action="store_true")
    parser.add_argument("--keep-profiles", action="store_true")
    args = parser.parse_args(argv)
    if args.self_test:
        incompatible = (
            args.haider is not None
            or args.surface is not None
            or args.output is not None
            or args.budget_bytes is not None
            or args.calibrate
            or args.tui_turns != 0
            or args.keep_profiles
        )
        if incompatible:
            parser.error("--self-test cannot be combined with measurement options")
        return args
    for option, value in (
        ("--haider", args.haider),
        ("--surface", args.surface),
        ("--output", args.output),
    ):
        if value is None:
            parser.error(f"the following arguments are required: {option}")
    if args.runs <= 0:
        parser.error("--runs must be positive")
    if args.tui_turns not in (0, TUI_CPU_TURNS):
        parser.error(f"--tui-turns must be 0 or exactly {TUI_CPU_TURNS}")
    if args.tui_turns and not str(args.surface).startswith("tui-demo-"):
        parser.error("--tui-turns is valid only for TUI surfaces")
    if args.settle_seconds <= 0:
        parser.error("--settle-seconds must be positive")
    if args.calibrate and args.runs != CALIBRATION_RUNS:
        parser.error(f"--calibrate requires exactly --runs {CALIBRATION_RUNS}")
    if args.calibrate and args.settle_seconds < DEFAULT_SETTLE_SECONDS:
        parser.error(
            f"--calibrate requires --settle-seconds >= {DEFAULT_SETTLE_SECONDS:g}"
        )
    if args.calibrate and args.load_limit > DEFAULT_LOAD_LIMIT:
        parser.error(f"--calibrate requires --load-limit <= {DEFAULT_LOAD_LIMIT:g}")
    if not args.calibrate and args.budget_bytes is None:
        parser.error("guard mode requires --budget-bytes")
    return args


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    if args.self_test:
        return self_test()
    assert args.haider is not None
    assert args.output is not None
    assert args.surface is not None
    haider = args.haider.resolve(strict=True)
    sibling = haider.with_name("haiderd")
    if args.surface in {"run-post-command", "status-post-command"} and not sibling.exists():
        raise RuntimeError(f"wire surface requires sibling daemon: {sibling}")
    args.output.mkdir(parents=True, exist_ok=True)
    metrics = DarwinProcessMetrics()
    samples: list[dict[str, object]] = []
    for index in range(1, args.runs + 1):
        load_before_spawn = wait_for_load(args.load_limit, args.load_wait_seconds)
        root = Path(tempfile.mkdtemp(prefix="hm969-", dir="/private/tmp"))
        os.chmod(root, 0o700)
        artefact_dir = args.output / f"run-{index}"
        artefact_dir.mkdir(parents=True, exist_ok=True)
        env = hermetic_env(root)
        try:
            common = {
                "haider": haider,
                "env": env,
                "settle_seconds": args.settle_seconds,
                "load_limit": args.load_limit,
                "load_wait_seconds": args.load_wait_seconds,
                "metrics": metrics,
                "artefact_dir": artefact_dir,
            }
            if args.surface == "status-post-command":
                sample = measure_status(**common)
            elif args.surface == "run-post-command":
                sample = measure_run(**common)
            else:
                sample = measure_tui(
                    **common,
                    graphics=args.surface == "tui-demo-sixel",
                    tui_turns=args.tui_turns,
                )
            sample["run"] = index
            sample["load_before_spawn"] = load_before_spawn
            samples.append(sample)
            (artefact_dir / "sample.json").write_text(
                json.dumps(sample, indent=2, sort_keys=True) + "\n", encoding="utf-8"
            )
            print(
                f"{args.surface} run={index} "
                f"footprint={sample['phys_footprint_bytes']} "
                f"cpu_us={sample['cpu_total_us']} threads={sample['threads']} "
                f"load={load_before_spawn:.2f}/{sample['load_before_read']:.2f}",
                flush=True,
            )
        finally:
            if not args.keep_profiles:
                shutil.rmtree(root, ignore_errors=True)

    summary = summarize(args.surface, samples, calibration=args.calibrate)
    summary_path = args.output / "summary.json"
    summary_path.write_text(
        json.dumps(summary, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    print(json.dumps(summary, indent=2, sort_keys=True))
    if args.budget_bytes is not None:
        actual = max(int(sample["phys_footprint_bytes"]) for sample in samples)
        if actual >= args.budget_bytes:
            print(
                f"FAIL {args.surface}: footprint {actual} >= budget {args.budget_bytes}",
                file=sys.stderr,
            )
            return 1
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, RuntimeError, subprocess.TimeoutExpired) as error:
        print(f"client-footprint: {error}", file=sys.stderr)
        raise SystemExit(2) from error
