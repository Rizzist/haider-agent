"""Autospawned idle retirement, generation respawn, and clean stop."""

from __future__ import annotations

from typing import Any

from gate import (
    DAEMON_DRAIN,
    DAEMON_STARTUP,
    DAEMON_STOP,
    FAIL,
    PASS,
    PROCESS_EXIT_GRACE,
    STATUS_REQUEST,
    BudgetPart,
    Evidence,
)
from gate.context import parse_single_json, process_is_alive, wait_pid_gone

id = "t1.daemon.lifecycle_triad"
tier = "t1"
area = "daemon"
needs = ("binary", "daemon", "network:none")
script = [{"step": "finish", "reason": "end_turn"}]
turns_expected = 0
timed = True
IDLE_TTL = BudgetPart(
    "requested run-daemon idle TTL",
    1.0,
    "HAIDER_RUN_DAEMON_IDLE_TTL_MS=1000; crates/haider-cli/src/run.rs:488-512",
)
# Registry #94: first status 30+60; idle retirement is 1s TTL + 5s daemon
# drain + 2s PID observation; respawn status 30+60; explicit stop is 20+2
# plus an independent 2s PID observation; cleanup is 60+20+2 plus two
# historical PID observations. Total = 298s.
budget = (
    DAEMON_STARTUP
    + STATUS_REQUEST
    + IDLE_TTL
    + DAEMON_DRAIN
    + PROCESS_EXIT_GRACE
    + DAEMON_STARTUP
    + STATUS_REQUEST
    + DAEMON_STOP
    + PROCESS_EXIT_GRACE
    + PROCESS_EXIT_GRACE
    + STATUS_REQUEST
    + DAEMON_STOP
    + PROCESS_EXIT_GRACE
    + PROCESS_EXIT_GRACE
    + PROCESS_EXIT_GRACE
)


def _identity(
    failures: list[str], document: dict[str, Any], label: str
) -> tuple[int | None, int | None]:
    daemon = document.get("daemon") if isinstance(document.get("daemon"), dict) else {}
    pid = daemon.get("pid")
    generation = daemon.get("generation")
    if isinstance(pid, bool) or not isinstance(pid, int) or pid <= 0:
        failures.append(f"{label} pid expected=positive_integer actual={pid!r}")
        pid = None
    if isinstance(generation, bool) or not isinstance(generation, int) or generation <= 0:
        failures.append(
            f"{label} generation expected=positive_integer actual={generation!r}"
        )
        generation = None
    if daemon.get("ready") is not True:
        failures.append(f"{label} ready expected=true actual={daemon.get('ready')!r}")
    return pid, generation


def run(ctx) -> list[Evidence]:
    ctx.env["HAIDER_RUN_DAEMON_IDLE_TTL_MS"] = "1000"
    failures: list[str] = []
    results = []

    first_result = ctx.run_haider(
        ["status", "--json"], timeout=DAEMON_STARTUP + STATUS_REQUEST
    )
    results.append(("first-status", first_result))
    first: dict[str, Any] = {}
    try:
        first = parse_single_json(first_result.stdout, "first status")
    except Exception as error:
        failures.append(f"first status JSON actual={error}")
    if first_result.timed_out or first_result.returncode != 0:
        failures.append(
            f"first status expected_exit=0 actual={first_result.returncode} "
            f"timed_out={str(first_result.timed_out).lower()}"
        )
    failures.extend(ctx.observe_status(first))
    first_pid, first_generation = _identity(failures, first, "first status")
    if ctx.ownership_refused or first_pid is None:
        return [
            Evidence(
                "lifecycle_triad",
                FAIL,
                "; ".join(failures or ["first ownership expected=trusted actual=untrusted"]),
                [ctx.command_artefact(name, result) for name, result in results],
            )
        ]

    idle_window = IDLE_TTL + DAEMON_DRAIN + PROCESS_EXIT_GRACE
    idle_exited = first_pid is not None and wait_pid_gone(first_pid, idle_window)
    if not idle_exited:
        failures.append(
            "idle-exit defect expected=pid_gone within="
            f"{idle_window.milliseconds}ms (1000ms TTL + 5000ms drain + 2000ms grace) "
            f"actual_alive={str(first_pid is not None and process_is_alive(first_pid)).lower()} "
            f"pid={first_pid!r}"
        )

    second_result = ctx.run_haider(
        ["status", "--json"], timeout=DAEMON_STARTUP + STATUS_REQUEST
    )
    results.append(("second-status", second_result))
    second: dict[str, Any] = {}
    try:
        second = parse_single_json(second_result.stdout, "second status")
    except Exception as error:
        failures.append(f"second status JSON actual={error}")
    if second_result.timed_out or second_result.returncode != 0:
        failures.append(
            f"second status expected_exit=0 actual={second_result.returncode} "
            f"timed_out={str(second_result.timed_out).lower()}"
        )
    failures.extend(ctx.observe_status(second))
    second_pid, second_generation = _identity(failures, second, "second status")
    if ctx.ownership_refused or second_pid is None:
        return [
            Evidence(
                "lifecycle_triad",
                FAIL,
                "; ".join(failures or ["second ownership expected=trusted actual=untrusted"]),
                [ctx.command_artefact(name, result) for name, result in results],
            )
        ]
    if first_pid is not None and second_pid == first_pid:
        failures.append(f"respawn pid expected!=first({first_pid}) actual={second_pid!r}")
    if (
        first_generation is not None
        and second_generation is not None
        and second_generation != first_generation + 1
    ):
        failures.append(
            f"daemon.generation expected={first_generation + 1} actual={second_generation}"
        )

    stop_result = ctx.run_haider(
        ["daemon", "stop", "--json"], timeout=DAEMON_STOP + PROCESS_EXIT_GRACE
    )
    results.append(("stop", stop_result))
    stop: dict[str, Any] = {}
    try:
        stop = parse_single_json(stop_result.stdout, "daemon stop")
    except Exception as error:
        failures.append(f"daemon stop JSON actual={error}")
    stopped = stop.get("daemon") if isinstance(stop.get("daemon"), dict) else {}
    if stop_result.timed_out or stop_result.returncode != 0:
        failures.append(
            f"daemon stop expected_exit=0 actual={stop_result.returncode} "
            f"timed_out={str(stop_result.timed_out).lower()}"
        )
    if stop.get("outcome") != "stopped_cleanly":
        failures.append(
            f"daemon stop outcome expected=stopped_cleanly actual={stop.get('outcome')!r}"
        )
    if stopped.get("pid") != second_pid:
        failures.append(f"daemon stop pid expected={second_pid!r} actual={stopped.get('pid')!r}")
    if stopped.get("process_exited") is not True:
        failures.append(
            "daemon stop process_exited expected=true "
            f"actual={stopped.get('process_exited')!r}"
        )
    stopped_gone = second_pid is not None and wait_pid_gone(second_pid, PROCESS_EXIT_GRACE)
    if not stopped_gone:
        failures.append(f"final pid gone expected=true actual=false pid={second_pid!r}")

    if failures:
        return [
            Evidence(
                "lifecycle_triad",
                FAIL,
                "; ".join(failures),
                [ctx.command_artefact(name, result) for name, result in results],
            )
        ]
    return [
        Evidence(
            "lifecycle_triad",
            PASS,
            f"autospawn_pid={first_pid} generation={first_generation}; "
            "idle_exit=true window=8000ms (1000ms TTL + 5000ms drain + 2000ms grace); "
            f"respawn_pid={second_pid} generation={second_generation}; "
            "stop=stopped_cleanly process_exited=true pid_gone=true",
        )
    ]
