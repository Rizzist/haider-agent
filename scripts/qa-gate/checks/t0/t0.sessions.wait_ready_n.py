"""N-session readiness is exact, finite, and typed on an unmet count."""

from __future__ import annotations

from gate import (
    DAEMON_STARTUP,
    DAEMON_STOP,
    FAIL,
    PASS,
    PROCESS_EXIT_GRACE,
    RUN_TERMINAL_GRACE,
    STATUS_REQUEST,
    BudgetPart,
    Evidence,
)
from gate.headless import json_document

id = "t0.sessions.wait_ready_n"
tier = "t0"
area = "automation"
needs = ("binary", "daemon", "network:none")
script = []
for index in range(1, 6):
    script.extend(
        [
            {"step": "emit_text", "text": f"QA_READY_SEGMENT_{index}"},
            {"step": "finish", "reason": "end_turn"},
        ]
    )
turns_expected = 5
READY_FIVE_SECONDS = BudgetPart(
    "positive readiness timeout",
    5.0,
    "haider sessions wait-ready --timeout 5s; crates/haider-cli/src/automation.rs:137-201",
)
READY_TWO_SECONDS = BudgetPart(
    "negative readiness timeout",
    2.0,
    "haider sessions wait-ready --timeout 2s; crates/haider-cli/src/automation.rs:137-201",
)
RESUME_FIVE_SECONDS = BudgetPart(
    "terminal-segment settle timeout",
    5.0,
    "haider resume --timeout 5s; crates/haider-cli/src/automation.rs:204-240",
)
# Registry #94: first start 30+60, five resident-daemon starts 5*60,
# three positive resumes 3*(5+2), positive ready 5+2, negative ready
# 2+2, cleanup 60+20+2+2. Total=506s.
budget = (
    DAEMON_STARTUP
    + STATUS_REQUEST
    + STATUS_REQUEST
    + STATUS_REQUEST
    + STATUS_REQUEST
    + STATUS_REQUEST
    + STATUS_REQUEST
    + RESUME_FIVE_SECONDS
    + RUN_TERMINAL_GRACE
    + RESUME_FIVE_SECONDS
    + RUN_TERMINAL_GRACE
    + RESUME_FIVE_SECONDS
    + RUN_TERMINAL_GRACE
    + READY_FIVE_SECONDS
    + RUN_TERMINAL_GRACE
    + READY_TWO_SECONDS
    + RUN_TERMINAL_GRACE
    + STATUS_REQUEST
    + DAEMON_STOP
    + PROCESS_EXIT_GRACE
    + PROCESS_EXIT_GRACE
)
timed = False


def _start(ctx, index: int, *, first: bool):
    result = ctx.run_haider(
        [
            "run",
            "--start",
            "--provider",
            "fake",
            "--model",
            "fake-model",
            "--output",
            "json",
            "-p",
            f"ready-{index}",
        ],
        timeout=(DAEMON_STARTUP + STATUS_REQUEST) if first else STATUS_REQUEST,
    )
    document = json_document(result, f"start {index}")
    session_id = document.get("session_id")
    failures = []
    if result.timed_out:
        failures.append("timed_out=true")
    if result.returncode != 0:
        failures.append(f"exit expected=0 actual={result.returncode}")
    if document.get("outcome") != "started":
        failures.append(f"outcome expected=started actual={document.get('outcome')!r}")
    if not isinstance(session_id, str) or not session_id:
        failures.append(f"session_id expected=nonempty actual={session_id!r}")
    if failures:
        raise RuntimeError(
            "; ".join(failures)
            + f" artefact={ctx.command_artefact(f'start-{index}', result)}"
        )
    return session_id


def run(ctx) -> list[Evidence]:
    positive_ids = [_start(ctx, index, first=index == 1) for index in range(1, 4)]
    settle_failures = []
    settle_results = []
    for index, session_id in enumerate(positive_ids, start=1):
        result = ctx.run_haider(
            ["resume", session_id, "--json", "--timeout", "5s"],
            timeout=RESUME_FIVE_SECONDS + RUN_TERMINAL_GRACE,
        )
        settle_results.append((index, result))
        try:
            document = json_document(result, f"positive resume {index}")
        except Exception as error:
            document = {}
            settle_failures.append(f"resume[{index}] json actual={error}")
        if result.timed_out or result.returncode != 0:
            settle_failures.append(
                f"resume[{index}] exit expected=0 actual={result.returncode} "
                f"timed_out={str(result.timed_out).lower()}"
            )
        if document.get("completed") is not True:
            settle_failures.append(
                f"resume[{index}] completed expected=true actual={document.get('completed')!r}"
            )

    positive = ctx.run_haider(
        [
            "sessions",
            "wait-ready",
            "--count",
            "3",
            "--timeout",
            "5s",
            "--json",
        ],
        timeout=READY_FIVE_SECONDS + RUN_TERMINAL_GRACE,
    )
    positive_failures = list(settle_failures)
    try:
        positive_document = json_document(positive, "positive readiness")
    except Exception as error:
        positive_document = {}
        positive_failures.append(f"json actual={error}")
    ready_ids = positive_document.get("ready_session_ids")
    if positive.timed_out or positive.returncode != 0:
        positive_failures.append(
            f"exit expected=0 actual={positive.returncode} "
            f"timed_out={str(positive.timed_out).lower()}"
        )
    if positive_document.get("schema") != "haider.sessions.ready.v1":
        positive_failures.append(
            "schema expected=haider.sessions.ready.v1 "
            f"actual={positive_document.get('schema')!r}"
        )
    if positive_document.get("ready") is not True:
        positive_failures.append(
            f"ready expected=true actual={positive_document.get('ready')!r}"
        )
    if positive_document.get("ready_count") != 3:
        positive_failures.append(
            f"ready_count expected=3 actual={positive_document.get('ready_count')!r}"
        )
    if not isinstance(ready_ids, list) or set(ready_ids) != set(positive_ids) or len(ready_ids) != 3:
        positive_failures.append(
            f"ready_session_ids expected={positive_ids!r} actual={ready_ids!r}"
        )
    positive_artefacts = []
    if positive_failures:
        positive_artefacts.append(ctx.command_artefact("positive-ready", positive))
        positive_artefacts.extend(
            ctx.command_artefact(f"positive-resume-{index}", result)
            for index, result in settle_results
        )
    positive_evidence = Evidence(
        "three_ready",
        FAIL if positive_failures else PASS,
        "; ".join(positive_failures)
        if positive_failures
        else f"document_count=1 ready=true ready_count=3 ids={','.join(positive_ids)}",
        positive_artefacts,
    )

    negative_ids = [_start(ctx, index, first=False) for index in range(4, 7)]
    negative_args = [
        "sessions",
        "wait-ready",
        "--count",
        "3",
        "--timeout",
        "2s",
        "--json",
    ]
    for session_id in negative_ids:
        negative_args.extend(("--session", session_id))
    negative = ctx.run_haider(
        negative_args,
        timeout=READY_TWO_SECONDS + RUN_TERMINAL_GRACE,
    )
    negative_failures = []
    try:
        negative_document = json_document(negative, "negative readiness")
    except Exception as error:
        negative_document = {}
        negative_failures.append(f"json actual={error}")
    if negative.timed_out:
        negative_failures.append("outer process timed_out actual=true")
    if negative.returncode != 124:
        negative_failures.append(f"exit expected=124 actual={negative.returncode}")
    if negative_document.get("schema") != "haider.sessions.ready.v1":
        negative_failures.append(
            "schema expected=haider.sessions.ready.v1 "
            f"actual={negative_document.get('schema')!r}"
        )
    if negative_document.get("ready") is not False:
        negative_failures.append(
            f"ready expected=false actual={negative_document.get('ready')!r}"
        )
    if negative_document.get("timed_out") is not True:
        negative_failures.append(
            f"timed_out expected=true actual={negative_document.get('timed_out')!r}"
        )
    if negative_document.get("ready_count") != 2:
        negative_failures.append(
            f"ready_count expected=2 actual={negative_document.get('ready_count')!r}"
        )
    negative_error = negative_document.get("error")
    error_code = negative_error.get("code") if isinstance(negative_error, dict) else None
    if error_code != "timeout":
        negative_failures.append(
            f"error.code expected=timeout actual={error_code!r}"
        )
    state_counts = negative_document.get("state_counts")
    negative_line = (
        "; ".join(negative_failures)
        if negative_failures
        else "document_count=1 only_two_segments=true third_session=backoff "
        "ready=false ready_count=2 exit=124 error_code=timeout"
    )
    if negative_failures:
        negative_line += f"; state_counts={state_counts!r}"
    negative_evidence = Evidence(
        "third_not_ready",
        FAIL if negative_failures else PASS,
        negative_line,
        [ctx.command_artefact("negative-ready", negative)]
        if negative_failures
        else [],
    )
    return [positive_evidence, negative_evidence]
