"""Kill a daemon during a hanging provider turn and prove finite recovery."""

from __future__ import annotations

import json
import os
import signal
import time
from typing import Any

from gate import (
    DAEMON_STARTUP,
    DAEMON_STOP,
    ENV_BLOCKED,
    FAIL,
    PASS,
    PROCESS_EXIT_GRACE,
    RUN_TERMINAL_GRACE,
    RUN_TIMEOUT,
    STATUS_REQUEST,
    BudgetPart,
    Evidence,
)
from gate.context import parse_single_json, wait_pid_gone

id = "t1.daemon.kill9_midturn"
tier = "t1"
area = "daemon"
needs = ("binary", "daemon", "network:none")
FRESH_SENTINEL = "QA_GATE_FRESH_AFTER_KILL9"
script = [
    {"step": "hang"},
    {"step": "emit_text", "text": FRESH_SENTINEL},
    {"step": "finish", "reason": "end_turn"},
]
turns_expected = 2
timed = True

RECOVERY_REQUEST_PATH = BudgetPart(
    "session recover five-request path",
    300.0,
    "5 * crates/haider-client/src/client.rs:43-46 REQUEST_TIMEOUT; "
    "crates/haider-cli/src/session_recover.rs:266-374,455-520",
)
RESUME_WAIT = BudgetPart(
    "explicit headless resume wait",
    5.0,
    "haider resume <id> --json --timeout 5s; crates/haider-cli/src/automation.rs:204-240",
)
MIDTURN_ARM = BudgetPart(
    "continuous hanging-turn arming deadline",
    30.0,
    "crates/haider-client/src/spawn.rs:58 STARTUP_DEADLINE",
)
# Registry #94: start 30+60; continuous run-status arm 30; daemon status 60;
# kill observation 2; respawn 30+60; recover 30+5*60; resume 5+2;
# fresh run 30+2; explicit stop 20+2 plus PID observation 2; cleanup
# 60+20+2 plus two historical PID observations. Total = 751s.
budget = (
    DAEMON_STARTUP
    + STATUS_REQUEST
    + MIDTURN_ARM
    + STATUS_REQUEST
    + PROCESS_EXIT_GRACE
    + DAEMON_STARTUP
    + STATUS_REQUEST
    + DAEMON_STARTUP
    + RECOVERY_REQUEST_PATH
    + RESUME_WAIT
    + RUN_TERMINAL_GRACE
    + RUN_TIMEOUT
    + RUN_TERMINAL_GRACE
    + DAEMON_STOP
    + PROCESS_EXIT_GRACE
    + PROCESS_EXIT_GRACE
    + STATUS_REQUEST
    + DAEMON_STOP
    + PROCESS_EXIT_GRACE
    + PROCESS_EXIT_GRACE
    + PROCESS_EXIT_GRACE
)


def _json_lines(stdout: str, label: str) -> list[dict[str, Any]]:
    values: list[dict[str, Any]] = []
    for index, line in enumerate(stdout.splitlines(), start=1):
        if not line.strip():
            continue
        value = json.loads(line)
        if not isinstance(value, dict):
            raise ValueError(f"{label} line={index} expected=object")
        values.append(value)
    if not values:
        raise ValueError(f"{label} expected JSON lines actual=0")
    return values


def _status_identity(
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
    return pid, generation


def run(ctx) -> list[Evidence]:
    if os.name != "posix" or not hasattr(signal, "SIGKILL"):
        return [
            Evidence(
                "kill9_capability",
                ENV_BLOCKED,
                f"missing need: POSIX SIGKILL unavailable os.name={os.name!r}",
            )
        ]

    failures: list[str] = []
    results = []
    start = ctx.run_haider(
        [
            "run",
            "-p",
            "midturn kill9",
            "--provider",
            "fake",
            "--model",
            "fake-model",
            "--output",
            "json",
            "--start",
        ],
        timeout=DAEMON_STARTUP + STATUS_REQUEST,
    )
    results.append(("run-start", start))
    try:
        start_values = _json_lines(start.stdout, "run --start")
    except Exception as error:
        start_values = []
        failures.append(f"run --start JSON actual={error}")
    started = start_values[-1] if start_values else {}
    session_id = started.get("session_id")
    run_id = started.get("run_id")
    if start.timed_out or start.returncode != 0:
        failures.append(
            f"run --start expected_exit=0 actual={start.returncode} "
            f"timed_out={str(start.timed_out).lower()}"
        )
    if started.get("schema") != "haider.run.v1" or not isinstance(session_id, str):
        failures.append(
            "run --start schema/session expected=haider.run.v1/nonempty "
            f"actual={started.get('schema')!r}/{session_id!r}"
        )
    if started.get("outcome") != "started" or not isinstance(run_id, str):
        failures.append(
            f"run --start outcome expected=started actual={started.get('outcome')!r} "
            f"run_id={run_id!r}"
        )

    state = None
    arm_deadline = time.monotonic() + MIDTURN_ARM.seconds
    attempt = 0
    while time.monotonic() < arm_deadline and isinstance(run_id, str):
        remaining = arm_deadline - time.monotonic()
        if remaining <= 0:
            break
        attempt += 1
        attempt_timeout = BudgetPart(
            "remaining continuous hanging-turn arming deadline",
            remaining,
            "remaining portion of MIDTURN_ARM after earlier bounded status requests",
        )
        lifecycle = ctx.run_haider(
            ["run", "--status", run_id, "--json"], timeout=attempt_timeout
        )
        results.append((f"run-status-before-kill-{attempt}", lifecycle))
        try:
            lifecycle_document = parse_single_json(lifecycle.stdout, "run status")
        except Exception as error:
            failures.append(f"run status JSON actual={error}")
            break
        state = lifecycle_document.get("result", {}).get("state", {}).get("state")
        if lifecycle.timed_out or lifecycle.returncode != 0:
            failures.append(
                f"run status expected_exit=0 actual={lifecycle.returncode} "
                f"timed_out={str(lifecycle.timed_out).lower()}"
            )
            break
        if state in {"thinking", "streaming", "waiting", "retrying"}:
            break
        if state != "queued":
            break
    if state not in {"thinking", "streaming", "waiting", "retrying"}:
        failures.append(
            f"mid-turn state expected=nonterminal_active within={MIDTURN_ARM.milliseconds}ms "
            f"actual={state!r} attempts={attempt}"
        )

    status_result = ctx.run_haider(["status", "--json"], timeout=STATUS_REQUEST)
    results.append(("status-before-kill", status_result))
    try:
        status = parse_single_json(status_result.stdout, "status before kill")
    except Exception as error:
        status = {}
        failures.append(f"status before kill JSON actual={error}")
    if status_result.timed_out or status_result.returncode != 0:
        failures.append(
            f"status before kill expected_exit=0 actual={status_result.returncode} "
            f"timed_out={str(status_result.timed_out).lower()}"
        )
    failures.extend(ctx.observe_status(status))
    old_pid, old_generation = _status_identity(failures, status, "status before kill")
    if failures:
        return [
            Evidence(
                "kill9_midturn",
                FAIL,
                "; ".join(failures),
                [ctx.command_artefact(name, result) for name, result in results],
            )
        ]

    try:
        os.kill(old_pid, signal.SIGKILL)
    except PermissionError as error:
        return [
            Evidence(
                "midturn_armed",
                PASS,
                f"midturn=true state={state} session_id={session_id} pid={old_pid}",
            ),
            Evidence(
                "kill9_capability",
                ENV_BLOCKED,
                f"missing need: SIGKILL permission denied pid={old_pid} actual={error}",
                [ctx.command_artefact(name, result) for name, result in results],
            ),
        ]
    except OSError as error:
        failures.append(f"SIGKILL expected=sent pid={old_pid} actual={error}")
    if not wait_pid_gone(old_pid, PROCESS_EXIT_GRACE):
        failures.append(f"killed pid gone expected=true actual=false pid={old_pid}")

    ctx.set_fake_provider_script(script[1:])
    respawn_result = ctx.run_haider(
        ["status", "--json"], timeout=DAEMON_STARTUP + STATUS_REQUEST
    )
    results.append(("status-after-kill", respawn_result))
    try:
        respawn = parse_single_json(respawn_result.stdout, "status after kill")
    except Exception as error:
        respawn = {}
        failures.append(f"status after kill JSON actual={error}")
    if respawn_result.timed_out or respawn_result.returncode != 0:
        failures.append(
            f"status after kill expected_exit=0 actual={respawn_result.returncode} "
            f"timed_out={str(respawn_result.timed_out).lower()}"
        )
    failures.extend(ctx.observe_status(respawn))
    new_pid, new_generation = _status_identity(failures, respawn, "status after kill")
    if ctx.ownership_refused or new_pid is None:
        return [
            Evidence(
                "kill9_midturn",
                FAIL,
                "; ".join(failures or ["respawn ownership expected=trusted actual=untrusted"]),
                [ctx.command_artefact(name, result) for name, result in results],
            )
        ]
    if new_pid == old_pid:
        failures.append(f"respawn pid expected!={old_pid} actual={new_pid!r}")
    if old_generation is not None and new_generation != old_generation + 1:
        failures.append(
            f"respawn generation expected={old_generation + 1} actual={new_generation!r}"
        )

    recover = ctx.run_haider(
        ["session", str(session_id), "recover", "--probe", "--json"],
        timeout=DAEMON_STARTUP + RECOVERY_REQUEST_PATH,
    )
    results.append(("session-recover-probe", recover))
    try:
        recover_document = parse_single_json(recover.stdout, "session recover --probe")
    except Exception as error:
        recover_document = {}
        failures.append(f"session recover --probe JSON actual={error}")
    recover_error = (
        recover_document.get("error")
        if isinstance(recover_document.get("error"), dict)
        else {}
    )
    recover_code = "probe" if recover.returncode == 0 else str(recover_error.get("code"))
    if recover.timed_out or recover.returncode != 0:
        failures.append(
            f"session recover --probe expected_exit=0 actual={recover.returncode} "
            f"timed_out={str(recover.timed_out).lower()} actual_code={recover_code!r} "
            f"actual_message={recover_error.get('message')!r}"
        )
    if recover_document.get("schema") != "haider.session_recovery.v1":
        failures.append(
            "session recover schema expected=haider.session_recovery.v1 "
            f"actual={recover_document.get('schema')!r}"
        )
    if recover_document.get("session_id") != session_id:
        failures.append(
            f"session recover session_id expected={session_id!r} "
            f"actual={recover_document.get('session_id')!r}"
        )
    if recover.returncode == 0:
        resolution_seq = recover_document.get("resolution_seq")
        menu_id = recover_document.get("menu_id")
        expected_replacement = (
            f"{menu_id}-probe-{resolution_seq}"
            if isinstance(menu_id, str)
            and menu_id
            and isinstance(resolution_seq, int)
            and not isinstance(resolution_seq, bool)
            else None
        )
        if (
            expected_replacement is None
            or recover_document.get("completed") is not True
            or recover_document.get("chosen_option") != "probe"
            or recover_document.get("resulting_run_state") != "effect_unknown"
            or recover_document.get("replacement_menu_id") != expected_replacement
        ):
            failures.append(
                "session recover probe expected=completed/probe/effect_unknown/replacement "
                f"actual_completed={recover_document.get('completed')!r} "
                f"actual_option={recover_document.get('chosen_option')!r} "
                f"actual_state={recover_document.get('resulting_run_state')!r} "
                f"actual_replacement={recover_document.get('replacement_menu_id')!r}"
            )

    resume = ctx.run_haider(
        ["resume", str(session_id), "--json", "--timeout", "5s"],
        timeout=RESUME_WAIT + RUN_TERMINAL_GRACE,
    )
    results.append(("resume", resume))
    try:
        resume_document = parse_single_json(resume.stdout, "resume")
    except Exception as error:
        resume_document = {}
        failures.append(f"resume JSON actual={error}")
    resume_outcome = resume_document.get("outcome")
    if resume.timed_out or resume.returncode not in {0, 124}:
        failures.append(
            f"resume expected_exit=0|124 actual={resume.returncode} "
            f"timed_out={str(resume.timed_out).lower()}"
        )
    if resume_document.get("schema") != "haider.session.resume.v1":
        failures.append(
            "resume schema expected=haider.session.resume.v1 "
            f"actual={resume_document.get('schema')!r}"
        )
    if resume_document.get("session_id") != session_id:
        failures.append(
            f"resume session_id expected={session_id!r} "
            f"actual={resume_document.get('session_id')!r}"
        )
    if not isinstance(resume_document.get("completed"), bool) or not isinstance(
        resume_document.get("timed_out"), bool
    ):
        failures.append(
            "resume typed booleans expected=completed,timed_out "
            f"actual={resume_document.get('completed')!r},{resume_document.get('timed_out')!r}"
        )
    if resume_outcome not in {
        "idle",
        "recovery_required",
        "input_required",
        "errored",
        "cancelled",
        "unknown",
        "timeout",
    }:
        failures.append(f"resume outcome expected=typed actual={resume_outcome!r}")
    if recover_code != "probe":
        failures.append(
            "kill9 recovery defect expected=probe/effect_unknown "
            f"actual={recover_code}/{resume_outcome}"
        )

    fresh = ctx.run_haider(
        [
            "run",
            "-p",
            "fresh after kill9",
            "--provider",
            "fake",
            "--model",
            "fake-model",
            "--output",
            "json",
            "--timeout",
            "30s",
        ],
        timeout=RUN_TIMEOUT + RUN_TERMINAL_GRACE,
    )
    results.append(("fresh-run", fresh))
    try:
        fresh_values = _json_lines(fresh.stdout, "fresh run")
    except Exception as error:
        fresh_values = []
        failures.append(f"fresh run JSON actual={error}")
    fresh_result = fresh_values[-1] if fresh_values else {}
    if fresh.timed_out or fresh.returncode != 0:
        failures.append(
            f"fresh run expected_exit=0 actual={fresh.returncode} "
            f"timed_out={str(fresh.timed_out).lower()}"
        )
    if fresh_result.get("outcome") != "done" or fresh_result.get("response") != FRESH_SENTINEL:
        failures.append(
            f"fresh run expected=done/{FRESH_SENTINEL!r} "
            f"actual={fresh_result.get('outcome')!r}/{fresh_result.get('response')!r}"
        )

    stop = ctx.run_haider(
        ["daemon", "stop", "--json"], timeout=DAEMON_STOP + PROCESS_EXIT_GRACE
    )
    results.append(("daemon-stop", stop))
    try:
        stop_document = parse_single_json(stop.stdout, "daemon stop")
    except Exception as error:
        stop_document = {}
        failures.append(f"daemon stop JSON actual={error}")
    if stop.timed_out or stop.returncode != 0 or stop_document.get("outcome") != "stopped_cleanly":
        failures.append(
            f"daemon stop expected=stopped_cleanly/0 actual={stop_document.get('outcome')!r}/"
            f"{stop.returncode} timed_out={str(stop.timed_out).lower()}"
        )
    if new_pid is not None and not wait_pid_gone(new_pid, PROCESS_EXIT_GRACE):
        failures.append(f"final pid gone expected=true actual=false pid={new_pid}")

    if failures:
        return [
            Evidence(
                "kill9_midturn",
                FAIL,
                "; ".join(failures),
                [ctx.command_artefact(name, result) for name, result in results],
            )
        ]
    return [
        Evidence(
            "kill9_midturn",
            PASS,
            f"midturn=true state={state} session_id={session_id} killed_pid={old_pid} "
            f"respawn_pid={new_pid} generation={old_generation}->{new_generation}; "
            f"recover={recover_code} resulting_state=effect_unknown; "
            f"resume_outcome={resume_outcome} finite=true; fresh_run=done sentinel=true "
            "stop=stopped_cleanly orphan=false",
        )
    ]
