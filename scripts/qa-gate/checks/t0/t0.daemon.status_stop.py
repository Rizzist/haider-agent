"""Installed-binary status and idempotent stop lifecycle smoke."""

from __future__ import annotations

import os
from typing import Any

from gate import (
    DAEMON_STARTUP,
    DAEMON_STOP,
    FAIL,
    PASS,
    PROCESS_EXIT_GRACE,
    STATUS_REQUEST,
    VERSION_QUERY,
    Evidence,
)
from gate.context import (
    canonical_path,
    canonical_paths_equal,
    parse_single_json,
    path_is_within,
    process_is_alive,
    status_socket_path_valid,
    wait_pid_gone,
)

id = "t0.daemon.status_stop"
tier = "t0"
area = "daemon"
needs = ("binary", "daemon", "network:none")
script = [{"step": "finish", "reason": "end_turn"}]
turns_expected = 0
# Registry #94: 30s cold version query; initial status can wrap 30s startup +
# 60s request; first and second stops each own 20s + 2s observation; the
# first stop has another 2s independent PID observation; the no-spawn proof and
# runner cleanup each own a 60s status request; cleanup owns a final 20s+2s stop
# plus its own 2s PID observation. Total = 310s.
budget = (
    VERSION_QUERY
    + DAEMON_STARTUP
    + STATUS_REQUEST
    + DAEMON_STOP
    + PROCESS_EXIT_GRACE
    + PROCESS_EXIT_GRACE
    + DAEMON_STOP
    + PROCESS_EXIT_GRACE
    + STATUS_REQUEST
    + STATUS_REQUEST
    + DAEMON_STOP
    + PROCESS_EXIT_GRACE
    + PROCESS_EXIT_GRACE
)
timed = True


def _canonical_field(
    failures: list[str],
    document: dict[str, Any],
    field: str,
    *,
    expected_under: str | os.PathLike[str] | None = None,
) -> None:
    value = document.get(field)
    if not isinstance(value, str) or not os.path.isabs(value):
        failures.append(f"{field} expected=absolute_path actual={value!r}")
        return
    if value != canonical_path(value):
        failures.append(f"{field} expected=canonical actual={value!r}")
    if expected_under is not None and not path_is_within(value, expected_under):
        failures.append(
            f"{field} expected_under={canonical_path(expected_under)!r} actual={value!r}"
        )


def run(ctx) -> list[Evidence]:
    failures: list[str] = []
    results = []

    version_result = ctx.run_haider(["--version"], timeout=VERSION_QUERY)
    results.append(("version", version_result))
    version_output = version_result.stdout.strip()
    expected_version = (
        version_output.removeprefix("haider ")
        if version_result.returncode == 0 and version_output.startswith("haider ")
        else None
    )
    if not expected_version:
        failures.append(
            f"haider --version expected='haider <version>' actual={version_output!r} "
            f"exit={version_result.returncode}"
        )

    status_result = ctx.run_haider(
        ["status", "--json"],
        timeout=DAEMON_STARTUP + STATUS_REQUEST,
    )
    results.append(("status", status_result))
    status: dict[str, Any] = {}
    if status_result.timed_out or status_result.returncode != 0:
        failures.append(
            f"status expected_exit=0 actual={status_result.returncode} "
            f"timed_out={str(status_result.timed_out).lower()}"
        )
    try:
        status = parse_single_json(status_result.stdout, "status")
    except Exception as error:
        failures.append(f"status JSON actual={error}")

    if status:
        failures.extend(ctx.observe_status(status))
    if ctx.ownership_refused or not ctx.daemon_pids:
        if not ctx.daemon_pids:
            failures.append("status ownership expected=trusted status PID actual=none")
        artefacts = [ctx.command_artefact(name, result) for name, result in results]
        return [
            Evidence(
                "daemon_status_stop",
                FAIL,
                "; ".join(failures or ["status ownership expected=throwaway actual=untrusted"]),
                artefacts,
            )
        ]
    if status.get("schema") != "haider.observe.v1":
        failures.append(f"status schema expected=haider.observe.v1 actual={status.get('schema')!r}")
    daemon = status.get("daemon") if isinstance(status.get("daemon"), dict) else {}
    if daemon.get("ready") is not True:
        failures.append(f"daemon.ready expected=true actual={daemon.get('ready')!r}")
    pid = daemon.get("pid")
    if isinstance(pid, bool) or not isinstance(pid, int) or pid <= 0:
        failures.append(f"daemon.pid expected=positive_integer actual={pid!r}")
        pid = None
    elif not process_is_alive(pid):
        failures.append(f"daemon.pid kill-0 expected=alive actual=gone pid={pid}")
    actual_version = daemon.get("version")
    if actual_version != expected_version:
        failures.append(
            f"daemon.version expected={expected_version!r} actual={actual_version!r}"
        )

    profile_path = status.get("profile_path")
    if not isinstance(profile_path, str) or not canonical_paths_equal(profile_path, ctx.profile_dir):
        failures.append(
            f"profile_path expected={canonical_path(ctx.profile_dir)!r} actual={profile_path!r}"
        )
    runtime_dir = status.get("runtime_dir")
    if not isinstance(runtime_dir, str):
        failures.append(f"runtime_dir expected=path actual={runtime_dir!r}")
    else:
        _canonical_field(failures, status, "runtime_dir", expected_under=ctx.runtime_root)
    if os.name == "nt":
        socket_path = daemon.get("socket_path")
        if not status_socket_path_valid(socket_path, runtime_dir):
            failures.append(
                f"socket_path expected=windows_named_pipe actual={socket_path!r}"
            )
    else:
        _canonical_field(failures, daemon, "socket_path", expected_under=runtime_dir)
    _canonical_field(failures, daemon, "pid_file_path", expected_under=runtime_dir)
    _canonical_field(failures, daemon, "pipe_dir", expected_under=ctx.profile_dir)

    first_stop = ctx.run_haider(
        ["daemon", "stop", "--json"],
        timeout=DAEMON_STOP + PROCESS_EXIT_GRACE,
    )
    results.append(("first-stop", first_stop))
    first_document: dict[str, Any] = {}
    try:
        first_document = parse_single_json(first_stop.stdout, "first stop")
    except Exception as error:
        failures.append(f"first stop JSON actual={error}")
    if first_stop.timed_out or first_stop.returncode != 0:
        failures.append(
            f"first stop expected_exit=0 actual={first_stop.returncode} "
            f"timed_out={str(first_stop.timed_out).lower()}"
        )
    if first_document.get("schema") != "haider.daemon-stop.v1":
        failures.append(
            f"first stop schema expected=haider.daemon-stop.v1 actual={first_document.get('schema')!r}"
        )
    if first_document.get("outcome") != "stopped_cleanly":
        failures.append(
            f"first stop outcome expected=stopped_cleanly actual={first_document.get('outcome')!r}"
        )
    stopped_daemon = (
        first_document.get("daemon") if isinstance(first_document.get("daemon"), dict) else {}
    )
    if stopped_daemon.get("pid") != pid:
        failures.append(f"first stop pid expected={pid!r} actual={stopped_daemon.get('pid')!r}")
    if stopped_daemon.get("process_exited") is not True:
        failures.append(
            f"first stop process_exited expected=true actual={stopped_daemon.get('process_exited')!r}"
        )
    pid_gone = pid is not None and wait_pid_gone(pid, PROCESS_EXIT_GRACE)
    if not pid_gone:
        failures.append(f"pid gone expected=true actual=false pid={pid!r}")

    second_stop = ctx.run_haider(
        ["daemon", "stop", "--json"],
        timeout=DAEMON_STOP + PROCESS_EXIT_GRACE,
    )
    results.append(("second-stop", second_stop))
    second_document: dict[str, Any] = {}
    try:
        second_document = parse_single_json(second_stop.stdout, "second stop")
    except Exception as error:
        failures.append(f"second stop JSON actual={error}")
    if second_stop.timed_out or second_stop.returncode != 69:
        failures.append(
            f"second stop expected_exit=69 actual={second_stop.returncode} "
            f"timed_out={str(second_stop.timed_out).lower()}"
        )
    if second_document.get("outcome") != "not_running":
        failures.append(
            f"second stop outcome expected=not_running actual={second_document.get('outcome')!r}"
        )

    no_spawn = ctx.run_haider(
        ["status", "--json", "--no-spawn"],
        timeout=STATUS_REQUEST,
    )
    results.append(("post-second-stop-status", no_spawn))
    if no_spawn.timed_out or no_spawn.returncode != 69:
        actual = None
        if no_spawn.returncode == 0:
            try:
                actual = parse_single_json(no_spawn.stdout, "post-stop status").get("daemon", {}).get(
                    "pid"
                )
            except Exception:
                actual = "unparseable"
        failures.append(
            f"second stop spawned daemon expected=false actual_pid={actual!r} "
            f"status_exit={no_spawn.returncode}"
        )

    if failures:
        artefacts = [ctx.command_artefact(name, result) for name, result in results]
        return [Evidence("daemon_status_stop", FAIL, "; ".join(failures), artefacts)]
    return [
        Evidence(
            "daemon_status_stop",
            PASS,
            f"ready=true pid={pid} alive=true version={actual_version} "
            f"runtime_dir={canonical_path(runtime_dir)} stop=stopped_cleanly pid_gone=true "
            "second_stop=not_running second_exit=69 spawned=false",
        )
    ]
