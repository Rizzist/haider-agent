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
import threading
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


def wait_pid_gone(pid: int, budget: BudgetPart | BudgetSum) -> bool:
    deadline = time.monotonic() + budget_seconds(budget)
    while time.monotonic() < deadline:
        if not process_is_alive(pid):
            return True
        time.sleep(0.025)
    return not process_is_alive(pid)


def _lsof_identity(
    pid: int,
    timeout: BudgetPart | BudgetSum,
) -> tuple[str | None, str | None]:
    """Resolve one PID's executable and cwd without granting signal authority."""

    lsof = shutil.which("lsof")
    if lsof is None:
        return None, None
    try:
        result = subprocess.run(
            [lsof, "-a", "-p", str(pid), "-d", "txt,cwd", "-Fn"],
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=budget_seconds(timeout),
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired):
        return None, None
    executable = None
    cwd = None
    descriptor = None
    for line in result.stdout.splitlines():
        if line.startswith("f"):
            descriptor = line[1:]
        elif line.startswith("n"):
            path = line[1:]
            if descriptor == "cwd":
                cwd = path
            elif descriptor == "txt" and executable is None:
                executable = path
    return executable, cwd


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
    """One short, throwaway profile/runtime and at most one live daemon at a time."""

    def __init__(
        self,
        *,
        check_id: str,
        bin_dir: Path,
        script: list[dict[str, Any]],
        report_artefact_root: Path | None = None,
    ):
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
        self.report_artefact_dir = (
            Path(report_artefact_root) / self.check_id
            if report_artefact_root is not None
            else None
        )
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
        self.fake_script = json.loads(json.dumps(script))
        self.spawn_possible = False
        self.daemon_pids: set[int] = set()
        self.untrusted_status_pids: set[int] = set()
        self.daemon_versions: set[str] = set()
        self.status_violations: list[str] = []
        self.ownership_refused = False
        self.commands: list[CommandResult] = []
        self._isolated_active = False
        self._disposed = False

    def run_command(
        self,
        argv: Sequence[str | os.PathLike[str]],
        *,
        timeout: BudgetPart | BudgetSum,
        env_overrides: dict[str, str | os.PathLike[str] | None] | None = None,
        cwd: str | os.PathLike[str] | None = None,
        may_spawn: bool = False,
    ) -> CommandResult:
        command = tuple(map(os.fspath, argv))
        if may_spawn:
            self.spawn_possible = True
        started = time.monotonic()
        child_env = self.env.copy()
        for key, value in (env_overrides or {}).items():
            if value is None:
                child_env.pop(key, None)
            else:
                child_env[key] = os.fspath(value)
        popen_kwargs: dict[str, Any] = {
            "cwd": os.fspath(cwd) if cwd is not None else self.workspace_dir,
            "env": child_env,
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
        process = subprocess.Popen(command, **popen_kwargs)
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
            argv=command,
            returncode=process.returncode,
            stdout=stdout,
            stderr=stderr,
            timed_out=timed_out,
            wall_ms=round((time.monotonic() - started) * 1_000),
        )
        self.commands.append(result)
        return result

    def run_haider(
        self,
        args: Sequence[str],
        *,
        timeout: BudgetPart | BudgetSum,
        env_overrides: dict[str, str | os.PathLike[str] | None] | None = None,
    ) -> CommandResult:
        return self.run_command(
            (self.haider_bin, *map(str, args)),
            timeout=timeout,
            env_overrides=env_overrides,
            may_spawn=self._may_spawn(args),
        )

    def run_binary(
        self,
        binary: str | os.PathLike[str],
        args: Sequence[str],
        *,
        timeout: BudgetPart | BudgetSum,
        env_overrides: dict[str, str | os.PathLike[str] | None] | None = None,
        may_spawn: bool = False,
    ) -> CommandResult:
        return self.run_command(
            (binary, *map(str, args)),
            timeout=timeout,
            env_overrides=env_overrides,
            may_spawn=may_spawn,
        )

    def set_fake_provider_script(self, script: list[dict[str, Any]]) -> None:
        self.fake_script = json.loads(json.dumps(script))
        self.env["HAIDER_TEST_FAKE_PROVIDER"] = json.dumps(script, separators=(",", ":"))

    def interrupt_haider_after_stdout(
        self,
        args: Sequence[str],
        *,
        marker: str,
        arm_timeout: BudgetPart | BudgetSum,
        terminal_timeout: BudgetPart | BudgetSum,
    ) -> CommandResult:
        """Send the client SIGINT only after a machine-output marker is visible.

        The client remains alive and services its negotiated connection while
        this harness observes stdout, satisfying registry #95. Both phases use
        named budgets so the outer wait is the sum of the arm observation and
        the client's terminal grace (registry #94).
        """

        if not marker:
            raise ContractError("interrupt stdout marker must be non-empty")
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
        stdout_parts: list[str] = []
        stderr_parts: list[str] = []

        def drain(pipe: Any, parts: list[str]) -> None:
            if pipe is None:
                return
            for line in iter(pipe.readline, ""):
                parts.append(line)

        readers = [
            threading.Thread(target=drain, args=(process.stdout, stdout_parts), daemon=True),
            threading.Thread(target=drain, args=(process.stderr, stderr_parts), daemon=True),
        ]
        for reader in readers:
            reader.start()

        timed_out = False
        arm_deadline = time.monotonic() + budget_seconds(arm_timeout)
        while marker not in "".join(stdout_parts) and process.poll() is None:
            if time.monotonic() >= arm_deadline:
                timed_out = True
                break
            time.sleep(0.01)

        if not timed_out and process.poll() is None:
            interrupt = (
                signal.SIGINT
                if os.name == "posix"
                else getattr(signal, "CTRL_C_EVENT", signal.SIGINT)
            )
            try:
                process.send_signal(interrupt)
            except (OSError, ValueError):
                process.terminate()
            try:
                process.wait(timeout=budget_seconds(terminal_timeout))
            except subprocess.TimeoutExpired:
                timed_out = True

        if timed_out and process.poll() is None:
            if os.name == "posix":
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
            else:
                process.kill()
        if process.poll() is None:
            process.wait()
        for reader in readers:
            reader.join()
        result = CommandResult(
            argv=argv,
            returncode=process.returncode,
            stdout="".join(stdout_parts),
            stderr="".join(stderr_parts),
            timed_out=timed_out,
            wall_ms=round((time.monotonic() - started) * 1_000),
        )
        self.commands.append(result)
        return result

    def run_isolated_haider(
        self,
        label: str,
        args: Sequence[str],
        *,
        timeout: BudgetPart | BudgetSum,
    ) -> tuple[CommandResult, Evidence]:
        """Run one sequential subcase with a fresh profile, script, and daemon.

        Budget controls need independent consumptive fake scripts. This is not
        a second runner: the child uses the same context/cleanup laws, cannot
        overlap another child, and returns its no-orphan evidence to the one
        owning check.
        """

        if not label or any(character in label for character in "\r\n"):
            raise ContractError("isolated subcase label must be non-empty and single-line")
        if self._isolated_active:
            raise ContractError("isolated subcases must be sequential")
        self._isolated_active = True
        child: CheckContext | None = None
        result: CommandResult | None = None
        cleanup: Evidence | None = None
        try:
            child = CheckContext(
                check_id=f"{self.check_id}.{label}",
                bin_dir=self.bin_dir,
                script=self.fake_script,
            )
            result = child.run_haider(args, timeout=timeout)
        finally:
            if child is not None:
                try:
                    cleanup = child.cleanup()
                except Exception as error:
                    emergency = child.emergency_cleanup()
                    cleanup = Evidence(
                        "no_orphan_daemons",
                        FAIL,
                        f"isolated cleanup_runner_error type={type(error).__name__} "
                        f"actual={str(error)!r} emergency_cleanup={emergency}",
                    )
                self.daemon_versions.update(child.daemon_versions)
                child.dispose(keep=cleanup.status == FAIL)
            self._isolated_active = False
        if result is None or cleanup is None:
            raise ContractError("isolated subcase ended without a result and cleanup evidence")
        return result, Evidence(
            f"{label}_no_orphan_daemons",
            cleanup.status,
            f"isolated_subcase={label} {cleanup.evidence_line}",
            cleanup.artefacts,
        )

    @staticmethod
    def _may_spawn(args: Sequence[str]) -> bool:
        if not args:
            return True
        if args[0] in ("run", "--ready", "account", "resume"):
            return True
        if args[0] in ("session", "sessions", "agent", "workflow"):
            return "--no-spawn" not in args
        return args[0] == "status" and "--no-spawn" not in args

    def observe_status(
        self,
        document: dict[str, Any],
        *,
        profile_dir: str | os.PathLike[str] | None = None,
        runtime_root: str | os.PathLike[str] | None = None,
    ) -> list[str]:
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
        expected_profile = self.profile_dir if profile_dir is None else profile_dir
        expected_runtime = self.runtime_root if runtime_root is None else runtime_root
        profile_path = document.get("profile_path")
        if not isinstance(profile_path, str) or not canonical_paths_equal(
            profile_path, expected_profile
        ):
            ownership_problems.append(
                "status profile_path escaped root "
                f"actual={profile_path!r} expected={canonical_path(expected_profile)!r}"
            )
        runtime_dir = document.get("runtime_dir")
        if not isinstance(runtime_dir, str) or not path_is_within(runtime_dir, expected_runtime):
            ownership_problems.append(
                "status runtime_dir escaped root "
                f"actual={runtime_dir!r} expected_under={canonical_path(expected_runtime)!r}"
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
            concurrent = sorted(
                observed
                for observed in self.daemon_pids
                if observed != pid and process_is_alive(observed)
            )
            if concurrent:
                problems.append(
                    "sequential daemon generations expected=retired "
                    f"actual_live={concurrent} new_pid={pid}"
                )
            self.daemon_pids.add(pid)
            version = daemon.get("version")
            if not isinstance(version, str) or not version:
                problems.append(f"status daemon.version actual={version!r}")
            else:
                self.daemon_versions.add(version)
        self.status_violations.extend(problems)
        return problems

    def observe_legacy_status(
        self,
        document: dict[str, Any],
        *,
        daemon_binary: str | os.PathLike[str],
        expected_version: str,
        identity_timeout: BudgetPart | BudgetSum,
    ) -> tuple[int | None, list[str]]:
        """Own a legacy daemon only through its exact profile PID and executable."""

        problems: list[str] = []
        if document.get("schema") != "haider.observe.v1":
            problems.append(f"legacy status schema actual={document.get('schema')!r}")
        if not canonical_paths_equal(document.get("profile_path", ""), self.profile_dir):
            problems.append(
                "legacy status profile_path escaped root "
                f"actual={document.get('profile_path')!r} "
                f"expected={canonical_path(self.profile_dir)!r}"
            )
        runtime_dir = document.get("runtime_dir")
        if not isinstance(runtime_dir, str) or not path_is_within(
            runtime_dir, self.runtime_root
        ):
            problems.append(
                "legacy status runtime_dir escaped root "
                f"actual={runtime_dir!r} expected_under={canonical_path(self.runtime_root)!r}"
            )
        daemon = document.get("daemon") if isinstance(document.get("daemon"), dict) else {}
        if daemon.get("version") != expected_version:
            problems.append(
                f"legacy daemon version expected={expected_version!r} "
                f"actual={daemon.get('version')!r}"
            )

        pid: int | None = None
        pid_path = Path(runtime_dir) / "haiderd.pid" if isinstance(runtime_dir, str) else None
        if (
            pid_path is None
            or not path_is_within(pid_path, self.runtime_root)
            or pid_path.is_symlink()
        ):
            problems.append(f"legacy daemon pid file refused path={pid_path!r}")
        else:
            try:
                candidate = int(pid_path.read_text(encoding="ascii").strip())
                if candidate <= 0:
                    raise ValueError("PID is not positive")
                pid = candidate
            except (OSError, UnicodeError, ValueError) as error:
                problems.append(
                    f"legacy daemon pid file unreadable path={pid_path} actual={error}"
                )

        if pid is not None:
            executable = None
            proc_exe = Path("/proc") / str(pid) / "exe"
            if proc_exe.exists():
                try:
                    executable = os.readlink(proc_exe).removesuffix(" (deleted)")
                except OSError:
                    pass
            lsof_executable, _cwd = _lsof_identity(pid, identity_timeout)
            executable = executable or lsof_executable
            if executable is None or not canonical_paths_equal(executable, daemon_binary):
                problems.append(
                    f"legacy daemon executable expected={canonical_path(daemon_binary)!r} "
                    f"actual={executable!r} pid={pid}"
                )
            if not process_is_alive(pid):
                problems.append(f"legacy daemon pid expected=alive actual=gone pid={pid}")

        if problems:
            self.ownership_refused = True
            if pid is not None:
                self.untrusted_status_pids.add(pid)
            self.status_violations.extend(problems)
            return None, problems
        assert pid is not None
        concurrent = sorted(
            observed
            for observed in self.daemon_pids
            if observed != pid and process_is_alive(observed)
        )
        if concurrent:
            problems.append(
                "sequential daemon generations expected=retired "
                f"actual_live={concurrent} new_pid={pid}"
            )
        self.daemon_pids.add(pid)
        self.daemon_versions.add(expected_version)
        self.status_violations.extend(problems)
        return pid, problems

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

    def publish_artefact(self, name: str, source: str | os.PathLike[str]) -> str:
        """Copy a PASS-worthy file or directory out of disposable scratch."""

        if self.report_artefact_dir is None:
            raise ContractError("persistent run artefact directory is unavailable")
        safe_name = "".join(
            character if character.isalnum() or character in ".-_" else "_"
            for character in name
        )
        source_path = Path(source)
        if not source_path.exists():
            raise ContractError(f"cannot publish missing artefact {source_path}")
        destination = self.report_artefact_dir / safe_name
        destination.parent.mkdir(parents=True, exist_ok=True)
        if destination.exists():
            if destination.is_dir():
                shutil.rmtree(destination)
            else:
                destination.unlink()
        if source_path.is_dir():
            shutil.copytree(source_path, destination)
        else:
            shutil.copy2(source_path, destination)
        return str(destination)

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
