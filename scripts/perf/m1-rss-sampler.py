#!/usr/bin/env python3
"""Fork-free 1 ms Darwin RSS sampler for the M1 peak case.

The sampler discovers a named root PID from a tiny pid file, follows only its
descendants with libproc, and samples task RSS with
proc_pidinfo(PROC_PIDTASKINFO). It never invokes ps, pgrep, or another child.
"""

from __future__ import annotations

import argparse
import ctypes
import ctypes.util
import os
from pathlib import Path
import sys
import time


PROC_PIDTBSDINFO = 3
PROC_PIDTASKINFO = 4
MAX_PID_PATH = 4096
SAMPLE_INTERVAL_NS = 1_000_000
DISCOVERY_INTERVAL_NS = 5_000_000


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


class ProcBsdInfo(ctypes.Structure):
    _fields_ = [
        ("flags", ctypes.c_uint32),
        ("status", ctypes.c_uint32),
        ("xstatus", ctypes.c_uint32),
        ("pid", ctypes.c_uint32),
        ("ppid", ctypes.c_uint32),
        ("uid", ctypes.c_uint32),
        ("gid", ctypes.c_uint32),
        ("ruid", ctypes.c_uint32),
        ("rgid", ctypes.c_uint32),
        ("svuid", ctypes.c_uint32),
        ("svgid", ctypes.c_uint32),
        ("rfu_1", ctypes.c_uint32),
        ("comm", ctypes.c_char * 16),
        ("name", ctypes.c_char * 32),
        ("nfiles", ctypes.c_uint32),
        ("pgid", ctypes.c_uint32),
        ("pjobc", ctypes.c_uint32),
        ("e_tdev", ctypes.c_uint32),
        ("e_tpgid", ctypes.c_uint32),
        ("nice", ctypes.c_int32),
        ("start_tvsec", ctypes.c_uint64),
        ("start_tvusec", ctypes.c_uint64),
    ]


class LibProc:
    def __init__(self) -> None:
        library = ctypes.util.find_library("proc")
        if not library:
            raise RuntimeError("libproc is unavailable; M1 requires macOS")
        self.lib = ctypes.CDLL(library, use_errno=True)
        self.lib.proc_pidinfo.argtypes = [
            ctypes.c_int,
            ctypes.c_int,
            ctypes.c_uint64,
            ctypes.c_void_p,
            ctypes.c_int,
        ]
        self.lib.proc_pidinfo.restype = ctypes.c_int
        self.lib.proc_listallpids.argtypes = [ctypes.c_void_p, ctypes.c_int]
        self.lib.proc_listallpids.restype = ctypes.c_int
        self.lib.proc_pidpath.argtypes = [ctypes.c_int, ctypes.c_void_p, ctypes.c_uint32]
        self.lib.proc_pidpath.restype = ctypes.c_int

    def task_rss(self, pid: int) -> int | None:
        info = ProcTaskInfo()
        size = ctypes.sizeof(info)
        result = self.lib.proc_pidinfo(
            pid, PROC_PIDTASKINFO, 0, ctypes.byref(info), size
        )
        return info.resident_size if result == size else None

    def bsd_info(self, pid: int) -> ProcBsdInfo | None:
        info = ProcBsdInfo()
        size = ctypes.sizeof(info)
        result = self.lib.proc_pidinfo(
            pid, PROC_PIDTBSDINFO, 0, ctypes.byref(info), size
        )
        return info if result == size else None

    def all_pids(self) -> list[int]:
        capacity = max(self.lib.proc_listallpids(None, 0), 1024) + 256
        buffer = (ctypes.c_int * capacity)()
        count = self.lib.proc_listallpids(buffer, ctypes.sizeof(buffer))
        if count <= 0:
            return []
        return [pid for pid in buffer[: min(count, capacity)] if pid > 0]

    def path(self, pid: int) -> str:
        buffer = ctypes.create_string_buffer(MAX_PID_PATH)
        length = self.lib.proc_pidpath(pid, buffer, len(buffer))
        if length <= 0:
            return ""
        return os.fsdecode(buffer.value)


def parse_named_pid(value: str) -> tuple[str, int]:
    try:
        label, raw_pid = value.split("=", 1)
        pid = int(raw_pid)
    except (ValueError, TypeError) as error:
        raise argparse.ArgumentTypeError("expected LABEL=PID") from error
    if not label or pid <= 0:
        raise argparse.ArgumentTypeError("LABEL must be nonempty and PID positive")
    return label, pid


def read_pid_file(path: Path) -> int | None:
    try:
        raw = path.read_text(encoding="ascii").strip()
        pid = int(raw)
    except (FileNotFoundError, OSError, ValueError):
        return None
    return pid if pid > 0 else None


def descendants(
    libproc: LibProc, root_pid: int, label_all_descendants_as_daemon: bool
) -> dict[int, str]:
    parent_by_pid: dict[int, int] = {}
    name_by_pid: dict[int, str] = {}
    for pid in libproc.all_pids():
        info = libproc.bsd_info(pid)
        if info is None:
            continue
        parent_by_pid[pid] = int(info.ppid)
        raw_name = bytes(info.name).split(b"\0", 1)[0]
        name_by_pid[pid] = os.fsdecode(raw_name)
    selected = {root_pid}
    changed = True
    while changed:
        changed = False
        for pid, parent in parent_by_pid.items():
            if parent in selected and pid not in selected:
                selected.add(pid)
                changed = True
    labels: dict[int, str] = {}
    for pid in selected:
        if pid == root_pid:
            labels[pid] = "haider"
            continue
        path_name = Path(libproc.path(pid)).name
        name = path_name or name_by_pid.get(pid, "")
        if label_all_descendants_as_daemon:
            labels[pid] = "haiderd"
        elif name in {"haider", "haiderd"}:
            labels[pid] = name
    return labels


def published_daemons(
    libproc: LibProc, directories: list[Path], sampler_started_wall_ns: int
) -> dict[int, str]:
    """Read fresh daemon-owned PID claims without spawning a path helper.

    Autospawn deliberately reparents the daemon, so parentage alone is not a
    stable identity after its readiness handshake. The profile-scoped claim
    is the daemon's own published identity and is removed during shutdown.
    """

    labels: dict[int, str] = {}
    for directory in directories:
        try:
            candidates = directory.rglob("haiderd.pid")
            for candidate in candidates:
                try:
                    if candidate.stat().st_mtime_ns < sampler_started_wall_ns:
                        continue
                    pid = int(candidate.read_text(encoding="ascii").strip())
                except (FileNotFoundError, OSError, ValueError):
                    continue
                if pid > 0 and Path(libproc.path(pid)).name == "haiderd":
                    labels[pid] = "haiderd"
        except OSError:
            continue
    return labels


def sample(args: argparse.Namespace) -> int:
    if sys.platform != "darwin":
        print("m1-rss-sampler: proc_pidinfo is supported only on Darwin", file=sys.stderr)
        return 2
    libproc = LibProc()
    tracked = dict(args.pid)
    root_pid: int | None = None
    saw_root = False
    saw_sample = False
    saw_daemon = False
    next_sample = time.monotonic_ns()
    next_discovery = next_sample
    sampler_started_wall_ns = time.time_ns()
    pending_lines: list[str] = ["wall_ns\tmono_ns\tpid\tlabel\trss_bytes\n"]
    args.output.parent.mkdir(parents=True, exist_ok=True)
    with args.output.open("w", encoding="ascii", buffering=64 * 1024) as output:
        while True:
            now_mono = time.monotonic_ns()
            if args.stop_file.exists():
                break
            if root_pid is None:
                root_pid = read_pid_file(args.root_pid_file)
                saw_root = saw_root or root_pid is not None
            if root_pid is not None and now_mono >= next_discovery:
                tracked.update(
                    descendants(
                        libproc,
                        root_pid,
                        args.label_all_descendants_as_daemon,
                    )
                )
                tracked.update(
                    published_daemons(
                        libproc, args.daemon_pid_dir, sampler_started_wall_ns
                    )
                )
                next_discovery = now_mono + DISCOVERY_INTERVAL_NS
            wall_ns = time.time_ns()
            for pid, label in list(tracked.items()):
                rss = libproc.task_rss(pid)
                if rss is None:
                    if pid != root_pid:
                        tracked.pop(pid, None)
                    continue
                saw_sample = True
                saw_daemon = saw_daemon or label == "haiderd"
                pending_lines.append(f"{wall_ns}\t{now_mono}\t{pid}\t{label}\t{rss}\n")
            if len(pending_lines) >= 32:
                output.writelines(pending_lines)
                pending_lines.clear()
                output.flush()
            next_sample += SAMPLE_INTERVAL_NS
            delay_ns = next_sample - time.monotonic_ns()
            if delay_ns > 0:
                time.sleep(delay_ns / 1_000_000_000)
            else:
                next_sample = time.monotonic_ns()
        if pending_lines:
            output.writelines(pending_lines)
        output.flush()
    if not saw_root:
        print("m1-rss-sampler: root PID file was never observed", file=sys.stderr)
        return 3
    if not saw_sample:
        print("m1-rss-sampler: no RSS sample was recorded", file=sys.stderr)
        return 4
    if args.require_daemon and not saw_daemon:
        print("m1-rss-sampler: daemon PID was never observed", file=sys.stderr)
        return 5
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--root-pid-file", required=True, type=Path)
    parser.add_argument("--stop-file", required=True, type=Path)
    parser.add_argument("--pid", action="append", default=[], type=parse_named_pid)
    parser.add_argument("--daemon-pid-dir", action="append", default=[], type=Path)
    parser.add_argument("--require-daemon", action="store_true")
    parser.add_argument(
        "--label-all-descendants-as-daemon",
        action="store_true",
        help="self-test only: prove parent discovery without requiring a haiderd binary",
    )
    return parser


if __name__ == "__main__":
    raise SystemExit(sample(build_parser().parse_args()))
