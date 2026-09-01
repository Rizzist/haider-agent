#!/usr/bin/env python3
"""Measure and guard settled `haider` client physical footprint on macOS.

Calibration is deliberately stricter than the routine CI guard: it requires
five independent samples, a 60-second settle, and host load below four both
before spawn and immediately before each reading.  Every accepted sample keeps
the authoritative `proc_pid_rusage` values and a `vmmap -summary` diagnostic.

The graphics TUI surface emulates only terminal capability replies.  The child
still performs the real PNG decode, ratatui-image resize/encode, Ratatui buffer
render, and terminal write, so the retained Sixel allocation is measured.
"""

from __future__ import annotations

import argparse
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


RUSAGE_INFO_V4 = 4
PROC_PIDTASKINFO = 4
DEFAULT_LOAD_LIMIT = 4.0
DEFAULT_SETTLE_SECONDS = 60.0
CALIBRATION_RUNS = 5
HOLD_MARGIN_SECONDS = 15
SIXEL_RESPONSE = b"\x1b[?64;4c\x1b[6;20;10t\x1b[0n"
WIRE_PROVIDER = "footprint-proxy"
WIRE_MODEL = "fixture-model"


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
        return {
            "phys_footprint_bytes": int(usage.ri_phys_footprint),
            "resident_bytes": int(usage.ri_resident_size),
            "lifetime_max_phys_footprint_bytes": int(
                usage.ri_lifetime_max_phys_footprint
            ),
            "user_time_us": int(usage.ri_user_time),
            "system_time_us": int(usage.ri_system_time),
            "cpu_total_us": int(usage.ri_user_time + usage.ri_system_time),
            "task_user_time_us": int(task.total_user),
            "task_system_time_us": int(task.total_system),
            "task_cpu_total_us": int(task.total_user + task.total_system),
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
        }
    )
    return env


def save_vmmap(pid: int, path: Path) -> dict[str, object]:
    result = subprocess.run(
        ["/usr/bin/vmmap", "-summary", str(pid)],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        timeout=30,
        check=False,
    )
    path.write_text(result.stdout, encoding="utf-8")
    return {"vmmap_exit": result.returncode, "vmmap_path": str(path)}


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

        load_before_read = require_load_below(load_limit, "before-read")
        sample = metrics.read(pid)
        sample.update(save_vmmap(pid, artefact_dir / "vmmap-summary.txt"))
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
    return module.OpenAIStub("client-footprint-ok", WIRE_MODEL).start()


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
        sample.update(save_vmmap(child.pid, artefact_dir / "vmmap-summary.txt"))
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
    child: subprocess.Popen[str] | None = None
    lines: list[str] = []
    try:
        ensure_profile_daemon_ready(haider, env)
        held_env = env.copy()
        held_env["HAIDER_CLIENT_FOOTPRINT_HOLD_MS"] = str(
            math.ceil((settle_seconds + HOLD_MARGIN_SECONDS) * 1000)
        )
        child = subprocess.Popen(
            [
                str(haider),
                "run",
                "client footprint fixture",
                "--output",
                "jsonl",
                "--timeout",
                "30s",
                "--provider",
                WIRE_PROVIDER,
                "--model",
                WIRE_MODEL,
            ],
            env=held_env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            start_new_session=True,
        )
        assert child.stdout is not None
        terminal: dict[str, object] | None = None
        terminal_deadline = time.monotonic() + 30.0
        while terminal is None:
            remaining = terminal_deadline - time.monotonic()
            if remaining <= 0:
                raise RuntimeError("headless run did not emit a terminal within 30 seconds")
            ready, _, _ = select.select([child.stdout], [], [], min(remaining, 0.1))
            if not ready:
                if child.poll() is not None:
                    raise RuntimeError("headless run exited before its terminal")
                continue
            line = child.stdout.readline()
            if not line:
                raise RuntimeError("headless run closed stdout before its terminal")
            lines.append(line)
            document = json.loads(line)
            payload = document.get("payload")
            if isinstance(payload, dict) and payload.get("terminal_kind") is not None:
                if payload.get("terminal_kind") != "success":
                    raise RuntimeError(f"headless fixture failed: {payload!r}")
                terminal = document
        (artefact_dir / "run.jsonl").write_text("".join(lines), encoding="utf-8")
        if stub.chat_count != 1:
            raise RuntimeError(f"headless fixture saw {stub.chat_count} provider requests")
        settle_deadline = time.monotonic() + settle_seconds
        while time.monotonic() < settle_deadline:
            returncode = child.poll()
            if returncode is not None:
                assert child.stderr is not None
                stderr = child.stderr.read()
                (artefact_dir / "run.stderr").write_text(stderr, encoding="utf-8")
                raise RuntimeError(
                    f"held headless run exited during settle exit={returncode} "
                    f"stderr={stderr!r}"
                )
            time.sleep(0.1)
        load_before_read = require_load_below(load_limit, "before-read")
        sample = metrics.read(child.pid)
        sample.update(save_vmmap(child.pid, artefact_dir / "vmmap-summary.txt"))
        sample.update(
            {
                "pid": child.pid,
                "load_before_read": load_before_read,
                "provider_requests": stub.chat_count,
                "terminal_seq": terminal.get("seq"),
            }
        )
        return sample
    finally:
        stub.close()
        if child is not None and child.poll() is None:
            os.killpg(child.pid, signal.SIGTERM)
        if child is not None:
            try:
                child.communicate(timeout=5)
            except subprocess.TimeoutExpired:
                os.killpg(child.pid, signal.SIGKILL)
                child.communicate(timeout=5)
        stop_profile_daemon(haider, env)


def median_absolute_deviation(values: list[int]) -> float:
    median = statistics.median(values)
    return float(statistics.median(abs(value - median) for value in values))


def summarize(surface: str, samples: list[dict[str, object]]) -> dict[str, object]:
    footprint = [int(sample["phys_footprint_bytes"]) for sample in samples]
    cpu = [int(sample["cpu_total_us"]) for sample in samples]
    return {
        "surface": surface,
        "runs": len(samples),
        "measurement_accepted": all(int(sample["vmmap_exit"]) == 0 for sample in samples),
        "phys_footprint_bytes": {
            "min": min(footprint),
            "median": statistics.median(footprint),
            "max": max(footprint),
            "mad": median_absolute_deviation(footprint),
        },
        "cpu_total_us": {
            "min": min(cpu),
            "median": statistics.median(cpu),
            "max": max(cpu),
            "mad": median_absolute_deviation(cpu),
        },
        "derived_budget_bytes": math.ceil(max(footprint) * 1.10),
        "samples": samples,
    }


def self_test() -> int:
    samples: list[dict[str, object]] = [
        {"phys_footprint_bytes": 900, "cpu_total_us": 10, "vmmap_exit": 0},
        {"phys_footprint_bytes": 1_000, "cpu_total_us": 20, "vmmap_exit": 0},
        {"phys_footprint_bytes": 1_100, "cpu_total_us": 30, "vmmap_exit": 0},
    ]
    summary = summarize("self-test", samples)
    if summary["phys_footprint_bytes"] != {
        "min": 900,
        "median": 1_000,
        "max": 1_100,
        "mad": 100.0,
    }:
        raise RuntimeError(f"summary arithmetic drifted: {summary!r}")
    if summary["derived_budget_bytes"] != 1_210:
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


def parse_args() -> argparse.Namespace:
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
    parser.add_argument("--settle-seconds", type=float, default=DEFAULT_SETTLE_SECONDS)
    parser.add_argument("--load-limit", type=float, default=DEFAULT_LOAD_LIMIT)
    parser.add_argument("--load-wait-seconds", type=float, default=15 * 60)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--budget-bytes", type=int)
    parser.add_argument("--calibrate", action="store_true")
    parser.add_argument(
        "--diagnostic-allow-missing-vmmap",
        action="store_true",
        help="retain rejected proc_pid_rusage diagnostics when sandboxing denies vmmap",
    )
    parser.add_argument("--keep-profiles", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        incompatible = (
            args.haider is not None
            or args.surface is not None
            or args.output is not None
            or args.budget_bytes is not None
            or args.calibrate
            or args.diagnostic_allow_missing_vmmap
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
    if args.settle_seconds <= 0:
        parser.error("--settle-seconds must be positive")
    if args.calibrate and args.runs < CALIBRATION_RUNS:
        parser.error(f"--calibrate requires --runs >= {CALIBRATION_RUNS}")
    if args.calibrate and args.runs > CALIBRATION_RUNS:
        parser.error(f"--calibrate is bounded to --runs {CALIBRATION_RUNS}")
    if args.calibrate and args.settle_seconds < DEFAULT_SETTLE_SECONDS:
        parser.error(
            f"--calibrate requires --settle-seconds >= {DEFAULT_SETTLE_SECONDS:g}"
        )
    if args.calibrate and args.load_limit > DEFAULT_LOAD_LIMIT:
        parser.error(f"--calibrate requires --load-limit <= {DEFAULT_LOAD_LIMIT:g}")
    if args.calibrate and args.diagnostic_allow_missing_vmmap:
        parser.error("--calibrate refuses --diagnostic-allow-missing-vmmap")
    if args.budget_bytes is not None and args.diagnostic_allow_missing_vmmap:
        parser.error("budget guards refuse --diagnostic-allow-missing-vmmap")
    if (
        not args.calibrate
        and args.budget_bytes is None
        and not args.diagnostic_allow_missing_vmmap
    ):
        parser.error("guard mode requires --budget-bytes")
    return args


def main() -> int:
    args = parse_args()
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
            if (
                int(sample["vmmap_exit"]) != 0
                and not args.diagnostic_allow_missing_vmmap
            ):
                raise RuntimeError(
                    "vmmap -summary failed "
                    f"exit={sample['vmmap_exit']}; diagnostic={sample['vmmap_path']}"
                )
        finally:
            if not args.keep_profiles:
                shutil.rmtree(root, ignore_errors=True)

    summary = summarize(args.surface, samples)
    summary_path = args.output / "summary.json"
    summary_path.write_text(
        json.dumps(summary, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    print(json.dumps(summary, indent=2, sort_keys=True))
    if not summary["measurement_accepted"]:
        print(
            "REJECTED diagnostic only: required vmmap evidence is unavailable",
            file=sys.stderr,
        )
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
