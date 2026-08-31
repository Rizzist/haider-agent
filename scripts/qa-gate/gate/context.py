"""Hermetic per-check process context and daemon cleanup law."""

from __future__ import annotations

import importlib.util
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass
from typing import Any, Sequence

from .contract import (
    DAEMON_STARTUP,
    DAEMON_STOP,
    FAIL,
    PASS,
    PROCESS_EXIT_GRACE,
    STATUS_REQUEST,
    BudgetPart,
    BudgetSum,
    ContractError,
    Evidence,
    budget_seconds,
)

COLOUR_ENV = (
    "NO_COLOR",
    "CLICOLOR",
    "CLICOLOR_FORCE",
    "FORCE_COLOR",
    "COLORTERM",
)


def canonical_path(path: os.PathLike[str] | str) -> str:
    """Canonical absolute spelling, including `/tmp` versus `/private/tmp`."""

    return os.path.realpath(os.path.abspath(os.fspath(path)))


def canonical_paths_equal(left: os.PathLike[str] | str, right: os.PathLike[str] | str) -> bool:
    return canonical_path(left) == canonical_path(right)


def path_is_within(path: os.PathLike[str] | str, root: os.PathLike[str] | str) -> bool:
    candidate = canonical_path(path)
    boundary = canonical_path(root)
    try:
        return os.path.commonpath((candidate, boundary)) == boundary
    except ValueError:
        return False


def status_socket_path_valid(
    socket_path: object,
    runtime_dir: object,
    *,
    platform_name: str = os.name,
) -> bool:
    if platform_name == "nt":
        return isinstance(socket_path, str) and socket_path.startswith(r"\\.\pipe\haider-")
    return (
        isinstance(socket_path, str)
        and isinstance(runtime_dir, str)
        and path_is_within(socket_path, runtime_dir)
    )


def process_is_alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    except OSError:
        return False
    return True


def wait_pid_gone(pid: int, budget: BudgetPart) -> bool:
    deadline = time.monotonic() + budget_seconds(budget)
    while time.monotonic() < deadline:
        if not process_is_alive(pid):
            return True
        time.sleep(0.025)
    return not process_is_alive(pid)


def parse_single_json(stdout: str, label: str) -> dict[str, Any]:
    lines = [line for line in stdout.splitlines() if line.strip()]
    if len(lines) != 1:
        raise ContractError(f"{label} expected one JSON line, actual={len(lines)}")
    try:
        document = json.loads(lines[0])
    except json.JSONDecodeError as error:
        raise ContractError(f"{label} emitted invalid JSON: {error}") from error
    if not isinstance(document, dict):
        raise ContractError(f"{label} JSON must be an object")
    return document


def _probelib_throwaway(path: Path) -> str:
    """Reuse the existing probe refusal on POSIX; provide its law on Windows."""

    if os.name != "nt":
        probelib_path = Path(__file__).resolve().parents[2] / "tui-probes" / "probelib.py"
        spec = importlib.util.spec_from_file_location("haider_qa_probelib", probelib_path)
        if spec is None or spec.loader is None:
            raise ContractError(f"cannot load throwaway guard at {probelib_path}")
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        return module.require_throwaway_profile(path)

    # probelib imports POSIX-only pty/termios modules. Keep the same path law
    # on Windows until the shared TUI probe library itself is portable.
    resolved = canonical_path(path)
    temp_root = canonical_path(tempfile.gettempdir())
    try:
        within = os.path.commonpath((resolved, temp_root)) == temp_root
    except ValueError:
        within = False
    named = any(part.startswith("haider-probe-") for part in Path(resolved).parts)
    if not within or not named:
        raise ContractError(f"probe refused non-throwaway profile: {resolved}")
    return resolved


@dataclass(frozen=True)
class CommandResult:
    argv: tuple[str, ...]
    returncode: int
    stdout: str
    stderr: str
    timed_out: bool
    wall_ms: int


class CheckContext:
    """One short, throwaway profile/runtime and at most one daemon."""

    def __init__(self, *, check_id: str, bin_dir: Path, script: list[dict[str, Any]]):
        self.check_id = check_id
        self.bin_dir = Path(canonical_path(bin_dir))
        self.haider_bin = self.bin_dir / ("haider.exe" if os.name == "nt" else "haider")
        self.haiderd_bin = self.bin_dir / ("haiderd.exe" if os.name == "nt" else "haiderd")

        temp_root = tempfile.gettempdir()
        root = Path(tempfile.mkdtemp(prefix="haider-probe-qa-", dir=temp_root))
        os.chmod(root, 0o700)
        self.root = Path(_probelib_throwaway(root))
        self.profile_dir = self.root / "p"
        self.runtime_root = self.root / "r"
        self.home_dir = self.root / "h"
        self.workspace_dir = self.root / "w"
        self.artefact_dir = self.root / "artefacts"
        for directory in (
            self.profile_dir,
            self.runtime_root,
            self.home_dir,
            self.workspace_dir,
            self.artefact_dir,
        ):
            directory.mkdir(mode=0o700)

        # A long root can make the product silently select /private/tmp. Refuse
        # the instrument itself before spawn; status later proves no fallback.
        if os.name != "nt" and len(canonical_path(self.root).encode()) > 64:
            raise ContractError(
                f"qa-gate short root exceeded 64 bytes: {canonical_path(self.root)}"
            )

        env = os.environ.copy()
        for key in tuple(env):
            if key.startswith("HAIDER_") or key.endswith(("_API_KEY", "_TOKEN", "_SECRET")):
                env.pop(key, None)
        for key in COLOUR_ENV:
            env.pop(key, None)
        env.update(
            {
                "HAIDER_PROFILE_DIR": str(self.profile_dir),
                "HAIDER_RUNTIME_DIR": str(self.runtime_root),
                "HAIDER_DISCOVERY_DISABLED": "1",
                "HAIDER_NO_UPDATE_CHECK": "1",
                "HAIDER_TEST_DEVICE_NAME": "test-mac",
                "HAIDER_TEST_FAKE_PROVIDER": json.dumps(script, separators=(",", ":")),
                "TERM": "xterm-256color",
                "HOME": str(self.home_dir),
                "USERPROFILE": str(self.home_dir),
                "XDG_CACHE_HOME": str(self.home_dir / ".cache"),
                "XDG_CONFIG_HOME": str(self.home_dir / ".config"),
                "XDG_DATA_HOME": str(self.home_dir / ".local" / "share"),
                "XDG_STATE_HOME": str(self.home_dir / ".local" / "state"),
                "TMPDIR": str(self.root),
            }
        )
        self.env = env
        self.spawn_possible = False
        self.daemon_pids: set[int] = set()
        self.untrusted_status_pids: set[int] = set()
        self.daemon_versions: set[str] = set()
        self.status_violations: list[str] = []
        self.ownership_refused = False
        self.commands: list[CommandResult] = []
        self._disposed = False

    def run_haider(
        self,
        args: Sequence[str],
        *,
        timeout: BudgetPart | BudgetSum,
    ) -> CommandResult:
        argv = (str(self.haider_bin), *map(str, args))
        if self._may_spawn(args):
            self.spawn_possible = True
        started = time.monotonic()
        popen_kwargs: dict[str, Any] = {
            "cwd": self.workspace_dir,
            "env": self.env,
            "stdout": subprocess.PIPE,
            "stderr": subprocess.PIPE,
            "text": True,
            "encoding": "utf-8",
            "errors": "replace",
        }
        if os.name == "posix":
            popen_kwargs["start_new_session"] = True
        elif hasattr(subprocess, "CREATE_NEW_PROCESS_GROUP"):
            popen_kwargs["creationflags"] = subprocess.CREATE_NEW_PROCESS_GROUP
        process = subprocess.Popen(argv, **popen_kwargs)
        timed_out = False
        try:
            stdout, stderr = process.communicate(timeout=budget_seconds(timeout))
        except subprocess.TimeoutExpired:
            timed_out = True
            if os.name == "posix":
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
            else:
                process.kill()
            stdout, stderr = process.communicate()
        result = CommandResult(
            argv=argv,
            returncode=process.returncode,
            stdout=stdout,
            stderr=stderr,
            timed_out=timed_out,
            wall_ms=round((time.monotonic() - started) * 1_000),
        )
        self.commands.append(result)
        return result

    @staticmethod
    def _may_spawn(args: Sequence[str]) -> bool:
        if not args:
            return True
        if args[0] == "run" or args[0] == "--ready":
            return True
        return args[0] == "status" and "--no-spawn" not in args

    def observe_status(self, document: dict[str, Any]) -> list[str]:
        """Record the only allowed daemon identity source and validate its root."""

        problems: list[str] = []
        ownership_problems: list[str] = []
        if document.get("schema") != "haider.observe.v1":
            ownership_problems.append(f"status schema actual={document.get('schema')!r}")
        daemon = document.get("daemon")
        if not isinstance(daemon, dict):
            ownership_problems.append("status daemon actual=missing_or_non_object")
            daemon = {}
        pid = daemon.get("pid")
        if isinstance(pid, bool) or not isinstance(pid, int) or pid <= 0:
            ownership_problems.append(f"status daemon.pid actual={pid!r}")
            pid = None
        profile_path = document.get("profile_path")
        if not isinstance(profile_path, str) or not canonical_paths_equal(
            profile_path, self.profile_dir
        ):
            ownership_problems.append(
                "status profile_path escaped root "
                f"actual={profile_path!r} expected={canonical_path(self.profile_dir)!r}"
            )
        runtime_dir = document.get("runtime_dir")
        if not isinstance(runtime_dir, str) or not path_is_within(runtime_dir, self.runtime_root):
            ownership_problems.append(
                "status runtime_dir escaped root "
                f"actual={runtime_dir!r} expected_under={canonical_path(self.runtime_root)!r}"
            )
        socket_path = daemon.get("socket_path")
        if os.name == "nt":
            if not status_socket_path_valid(socket_path, runtime_dir):
                ownership_problems.append(
                    f"status daemon.socket_path expected=named_pipe actual={socket_path!r}"
                )
        elif not status_socket_path_valid(socket_path, runtime_dir):
            ownership_problems.append(
                f"status daemon.socket_path actual={socket_path!r} expected_under={runtime_dir!r}"
            )
        pid_file_path = daemon.get("pid_file_path")
        if pid_file_path is not None and (
            not isinstance(pid_file_path, str)
            or not isinstance(runtime_dir, str)
            or not path_is_within(pid_file_path, runtime_dir)
        ):
            ownership_problems.append(
                f"status daemon.pid_file_path actual={pid_file_path!r} expected_under={runtime_dir!r}"
            )
        problems.extend(ownership_problems)
        if ownership_problems:
            self.ownership_refused = True
            if pid is not None:
                self.untrusted_status_pids.add(pid)
        elif pid is not None:
            self.daemon_pids.add(pid)
            version = daemon.get("version")
            if not isinstance(version, str) or not version:
                problems.append(f"status daemon.version actual={version!r}")
            else:
                self.daemon_versions.add(version)
        self.status_violations.extend(problems)
        return problems

    def write_artefact(self, name: str, content: str) -> str:
        safe_name = "".join(character if character.isalnum() or character in ".-_" else "_" for character in name)
        path = self.artefact_dir / safe_name
        path.write_text(content, encoding="utf-8")
        return str(path)

    def command_artefact(self, name: str, result: CommandResult) -> str:
        content = (
            f"argv={list(result.argv)!r}\n"
            f"returncode={result.returncode} timed_out={result.timed_out} wall_ms={result.wall_ms}\n"
            f"--- stdout ---\n{result.stdout}"
            f"--- stderr ---\n{result.stderr}"
        )
        return self.write_artefact(f"{name}.txt", content)

    def cleanup(self) -> Evidence:
        """Stop only the status-observed daemon and fail on any surviving PID."""

        if not self.spawn_possible:
            return Evidence("no_orphan_daemons", PASS, "no_orphan_daemons spawn=none")

        problems = list(self.status_violations)
        stop_outcome = "not_attempted"
        status = self.run_haider(
            ["status", "--json", "--no-spawn"],
            timeout=STATUS_REQUEST,
        )
        if status.timed_out:
            problems.append("cleanup status --no-spawn timed_out=true")
        elif status.returncode == 0:
            try:
                self.observe_status(parse_single_json(status.stdout, "cleanup status"))
            except ContractError as error:
                problems.append(str(error))
                self.ownership_refused = True
        elif status.returncode != 69:
            problems.append(
                f"cleanup status exit actual={status.returncode} stderr={status.stderr.strip()!r}"
            )

        if self.ownership_refused:
            stop_outcome = "refused_untrusted_status"
            problems.append(
                "cleanup refused daemon stop for untrusted status pid(s)="
                f"{sorted(self.untrusted_status_pids)}"
            )
        elif not self.daemon_pids:
            stop_outcome = "refused_no_owned_pid"
            problems.append("spawn-capable check produced no status-observed daemon pid")
        else:
            stop = self.run_haider(
                ["daemon", "stop", "--json"],
                timeout=DAEMON_STOP + PROCESS_EXIT_GRACE,
            )
            if stop.timed_out:
                problems.append("cleanup daemon stop timed_out=true")
            else:
                try:
                    stop_document = parse_single_json(stop.stdout, "cleanup daemon stop")
                    if stop_document.get("schema") != "haider.daemon-stop.v1":
                        problems.append(
                            "cleanup daemon stop schema "
                            f"actual={stop_document.get('schema')!r} expected='haider.daemon-stop.v1'"
                        )
                    stop_outcome = str(stop_document.get("outcome"))
                    if stop_outcome == "stopped_cleanly":
                        daemon = stop_document.get("daemon")
                        if not isinstance(daemon, dict) or daemon.get("process_exited") is not True:
                            problems.append(
                                "cleanup stopped_cleanly without daemon.process_exited=true"
                            )
                        elif daemon.get("pid") not in self.daemon_pids:
                            problems.append(
                                "cleanup stopped daemon pid lacks status provenance "
                                f"actual={daemon.get('pid')!r} observed={sorted(self.daemon_pids)}"
                            )
                        if stop.returncode != 0:
                            problems.append(f"cleanup stopped_cleanly exit actual={stop.returncode}")
                    elif stop_outcome == "not_running":
                        if stop.returncode != 69:
                            problems.append(f"cleanup not_running exit actual={stop.returncode}")
                    else:
                        problems.append(
                            f"cleanup daemon stop outcome actual={stop_outcome!r} exit={stop.returncode}"
                        )
                except ContractError as error:
                    problems.append(str(error))
        if len(self.daemon_pids) > 1:
            problems.append(f"one-daemon law observed pids={sorted(self.daemon_pids)}")
        survivors = [pid for pid in sorted(self.daemon_pids) if not wait_pid_gone(pid, PROCESS_EXIT_GRACE)]
        for pid in survivors:
            problems.append(f"no orphan daemons failed pid={pid} alive_after_stop=true")
            try:
                os.kill(pid, signal.SIGTERM)
            except ProcessLookupError:
                continue
            if not wait_pid_gone(pid, PROCESS_EXIT_GRACE):
                try:
                    os.kill(pid, getattr(signal, "SIGKILL", signal.SIGTERM))
                except ProcessLookupError:
                    pass

        # observe_status may have added root violations after the initial copy.
        for problem in self.status_violations:
            if problem not in problems:
                problems.append(problem)
        pids = ",".join(map(str, sorted(self.daemon_pids))) or "none"
        if problems:
            artefact = self.write_artefact("cleanup.txt", "\n".join(problems) + "\n")
            return Evidence(
                "no_orphan_daemons",
                FAIL,
                f"no_orphan_daemons actual=FAIL pids={pids} stop={stop_outcome} reason={problems[0]}",
                [artefact],
            )
        return Evidence(
            "no_orphan_daemons",
            PASS,
            f"no_orphan_daemons pids={pids} stop={stop_outcome} alive_after=false",
        )

    def emergency_cleanup(self) -> str:
        """Best-effort exact-PID fallback after cleanup machinery itself fails."""

        outcomes: list[str] = []
        for pid in sorted(self.daemon_pids):
            if not process_is_alive(pid):
                outcomes.append(f"pid={pid}:already_gone")
                continue
            try:
                os.kill(pid, signal.SIGTERM)
            except (OSError, ValueError) as error:
                outcomes.append(f"pid={pid}:sigterm_error={type(error).__name__}")
                continue
            if wait_pid_gone(pid, PROCESS_EXIT_GRACE):
                outcomes.append(f"pid={pid}:sigterm_gone")
                continue
            try:
                os.kill(pid, getattr(signal, "SIGKILL", signal.SIGTERM))
            except (OSError, ValueError) as error:
                outcomes.append(f"pid={pid}:hard_kill_error={type(error).__name__}")
                continue
            outcomes.append(
                f"pid={pid}:hard_kill_gone={str(wait_pid_gone(pid, PROCESS_EXIT_GRACE)).lower()}"
            )
        return ",".join(outcomes) or "no_status_observed_pid"

    def dispose(self, *, keep: bool) -> None:
        if self._disposed:
            return
        self._disposed = True
        if not keep:
            shutil.rmtree(self.root, ignore_errors=True)
