"""Durable replay is read-only; resume and recovery are finite and typed."""

from __future__ import annotations

import json
import sqlite3
import time
from pathlib import Path

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
from gate.headless import json_document, nested_call_ids, provider_request_ordinals

id = "t0.run.replay_resume_recover"
tier = "t0"
area = "headless"
needs = ("binary", "daemon", "network:none")
CALL_ID = "qa-replay-call-1"
SOURCE_SENTINEL = "QA_REPLAY_SOURCE"
CONTROL_SENTINEL = "QA_REPLAY_CONTROL"
script = [
    {
        "step": "emit_server_tool_use",
        "call_id": CALL_ID,
        "name": "qa_probe",
        "args": {"value": 1},
    },
    {
        "step": "emit_server_tool_result",
        "call_id": CALL_ID,
        "preview": "qa server result",
        "is_error": False,
    },
    {"step": "emit_text", "text": SOURCE_SENTINEL},
    {"step": "finish", "reason": "end_turn"},
    {"step": "emit_text", "text": CONTROL_SENTINEL},
    {"step": "finish", "reason": "end_turn"},
]
turns_expected = 2
RUN_TEN_SECONDS = BudgetPart(
    "headless request timeout",
    10.0,
    "haider run --timeout 10s; crates/haider-cli/src/run.rs:216-223",
)
REPLAY_FIVE_SECONDS = BudgetPart(
    "durable replay timeout",
    5.0,
    "haider run --replay --timeout 5s; crates/haider-cli/src/run.rs:639-677",
)
RESUME_FIVE_SECONDS = BudgetPart(
    "resume observation timeout",
    5.0,
    "haider resume --timeout 5s; crates/haider-cli/src/automation.rs:204-240",
)
JOURNAL_SETTLE = BudgetPart(
    "read-only journal terminal observation",
    5.0,
    "SQLite poll has no negotiated transport; terminal is bounded before replay",
)
# Registry #94: start 30+60, journal settle 5, status 60, event snapshot
# 60, replay 5+2, post-replay status 60, attached control 10+2, resume
# 5+2, recovery 60, cleanup 60+20+2+2. Total=445s.
budget = (
    DAEMON_STARTUP
    + STATUS_REQUEST
    + JOURNAL_SETTLE
    + STATUS_REQUEST
    + STATUS_REQUEST
    + REPLAY_FIVE_SECONDS
    + RUN_TERMINAL_GRACE
    + STATUS_REQUEST
    + RUN_TEN_SECONDS
    + RUN_TERMINAL_GRACE
    + RESUME_FIVE_SECONDS
    + RUN_TERMINAL_GRACE
    + STATUS_REQUEST
    + STATUS_REQUEST
    + DAEMON_STOP
    + PROCESS_EXIT_GRACE
    + PROCESS_EXIT_GRACE
)
timed = False


def _wait_terminal(database: Path, session_id: str, run_id: str) -> int:
    deadline = time.monotonic() + JOURNAL_SETTLE.seconds
    last_state = "database_missing"
    while time.monotonic() < deadline:
        if database.is_file():
            try:
                connection = sqlite3.connect(f"file:{database}?mode=ro", uri=True)
                try:
                    row = connection.execute(
                        "SELECT terminal, state_seq FROM run_heads "
                        "WHERE session_id = ? AND run_id = ?",
                        (session_id, run_id),
                    ).fetchone()
                finally:
                    connection.close()
                last_state = f"run_head={row!r}"
                if (
                    isinstance(row, tuple)
                    and len(row) == 2
                    and row[0] == 1
                    and isinstance(row[1], int)
                    and row[1] > 0
                ):
                    return row[1]
            except (OSError, sqlite3.Error) as error:
                last_state = f"{type(error).__name__}: {error}"
        time.sleep(0.025)
    raise RuntimeError(f"journal terminal observation expired actual={last_state}")


def _event_snapshot(ctx, run_id: str, terminal_seq: int):
    result = ctx.run_haider(["events", "--no-spawn"], timeout=STATUS_REQUEST)
    events = []
    for index, line in enumerate(result.stdout.splitlines(), start=1):
        if not line.strip():
            continue
        value = json.loads(line)
        if not isinstance(value, dict):
            raise ValueError(f"events line {index} expected object")
        if (
            value.get("run_id") == run_id
            and isinstance(value.get("seq"), int)
            and value["seq"] <= terminal_seq
        ):
            events.append(value)
    if result.timed_out or result.returncode != 0:
        raise RuntimeError(
            f"events snapshot exit={result.returncode} timed_out={result.timed_out}"
        )
    return result, events


def _status(ctx, run_id: str):
    result = ctx.run_haider(
        ["run", "--status", run_id, "--output", "json"], timeout=STATUS_REQUEST
    )
    document = json_document(result, "run status")
    return result, document


def run(ctx) -> list[Evidence]:
    start = ctx.run_haider(
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
            "durable replay source",
        ],
        timeout=DAEMON_STARTUP + STATUS_REQUEST,
    )
    failures = []
    artefacts = []
    try:
        start_document = json_document(start, "start")
    except Exception as error:
        start_document = {}
        failures.append(f"start.json actual={error}")
    if start.timed_out or start.returncode != 0:
        failures.append(
            f"start.exit expected=0 actual={start.returncode} "
            f"timed_out={str(start.timed_out).lower()}"
        )
    session_id = start_document.get("session_id")
    run_id = start_document.get("run_id")
    if not isinstance(session_id, str) or not session_id:
        failures.append(f"session_id expected=nonempty actual={session_id!r}")
    if not isinstance(run_id, str) or not run_id:
        failures.append(f"run_id expected=nonempty actual={run_id!r}")
    if failures:
        return [
            Evidence(
                "replay_resume_recover",
                FAIL,
                "; ".join(failures),
                [ctx.command_artefact("replay-start", start)],
            )
        ]

    try:
        terminal_seq = _wait_terminal(
            ctx.profile_dir / "store.sqlite", session_id, run_id
        )
    except Exception as error:
        return [
            Evidence(
                "replay_resume_recover",
                FAIL,
                f"source projection expected=terminal actual={error}",
                [ctx.command_artefact("replay-start", start)],
            )
        ]

    before_status, before_document = _status(ctx, run_id)
    before = before_document.get("result")
    if before_status.returncode != 0 or not isinstance(before, dict):
        failures.append(
            f"status_before expected=typed_success actual_exit={before_status.returncode}"
        )
        before = {}
    if before_document.get("schema") != "haider.run.status.v1":
        failures.append(
            "status_before.schema expected=haider.run.status.v1 "
            f"actual={before_document.get('schema')!r}"
        )
    if before.get("run_id") != run_id or before.get("session_id") != session_id:
        failures.append("status_before identity expected=source actual=mismatch")
    if before.get("terminal_seq") != terminal_seq:
        failures.append(
            f"status_before.terminal_seq expected={terminal_seq} "
            f"actual={before.get('terminal_seq')!r}"
        )

    try:
        events_result, source_events = _event_snapshot(ctx, run_id, terminal_seq)
    except Exception as error:
        return [
            Evidence(
                "replay_resume_recover",
                FAIL,
                f"source event snapshot expected=typed_jsonl actual={error}",
                [ctx.command_artefact("status-before", before_status)],
            )
        ]

    replay = ctx.run_haider(
        [
            "run",
            "--replay",
            run_id,
            "--output",
            "json",
            "--timeout",
            "5s",
        ],
        timeout=REPLAY_FIVE_SECONDS + RUN_TERMINAL_GRACE,
    )
    try:
        replay_document = json_document(replay, "replay")
    except Exception as error:
        replay_document = {}
        failures.append(f"replay.json actual={error}")
    replay_events = replay_document.get("events")
    if replay.timed_out or replay.returncode != 0:
        failures.append(
            f"replay.exit expected=0 actual={replay.returncode} "
            f"timed_out={str(replay.timed_out).lower()}"
        )
    if replay_document.get("schema") != "haider.run.replay.v1":
        failures.append(
            "replay.schema expected=haider.run.replay.v1 "
            f"actual={replay_document.get('schema')!r}"
        )
    if replay_document.get("provider_requests") != 0:
        failures.append(
            "replay.provider_requests expected=0 "
            f"actual={replay_document.get('provider_requests')!r}"
        )
    if replay_events != source_events:
        failures.append(
            f"replay.events expected_exact_source={len(source_events)} "
            f"actual={len(replay_events) if isinstance(replay_events, list) else replay_events!r}"
        )
    source_seqs = [event.get("seq") for event in source_events]
    replay_seqs = (
        [event.get("seq") for event in replay_events if isinstance(event, dict)]
        if isinstance(replay_events, list)
        else []
    )
    if replay_seqs != source_seqs:
        failures.append(f"replay.seq expected={source_seqs!r} actual={replay_seqs!r}")
    source_calls = nested_call_ids(source_events)
    replay_calls = nested_call_ids(replay_events)
    if replay_calls != source_calls or CALL_ID not in source_calls:
        failures.append(
            f"replay.call_ids expected={source_calls!r} actual={replay_calls!r}"
        )
    integrity = replay_document.get("integrity")
    if not isinstance(integrity, dict) or not all(
        integrity.get(field) is True
        for field in (
            "sequences_strictly_increasing",
            "run_id_stable",
            "exactly_one_typed_terminal",
            "terminal_seq_matches_status",
        )
    ):
        failures.append(f"replay.integrity expected=all_true actual={integrity!r}")

    after_status, after_document = _status(ctx, run_id)
    after = after_document.get("result")
    if after_status.returncode != 0 or not isinstance(after, dict):
        failures.append(
            f"status_after expected=typed_success actual_exit={after_status.returncode}"
        )
        after = {}
    for field in ("head_seq", "terminal_seq"):
        if after.get(field) != before.get(field):
            failures.append(
                f"replay mutated {field} expected={before.get(field)!r} "
                f"actual={after.get(field)!r}"
            )

    control = ctx.run_haider(
        [
            "run",
            "--provider",
            "fake",
            "--model",
            "fake-model",
            "--output",
            "json",
            "--timeout",
            "10s",
            "-p",
            "consume remaining control segment",
        ],
        timeout=RUN_TEN_SECONDS + RUN_TERMINAL_GRACE,
    )
    try:
        control_document = json_document(control, "control")
    except Exception as error:
        control_document = {}
        failures.append(f"control.json actual={error}")
    control_requests = provider_request_ordinals(control_document)
    if control.returncode != 0 or control_document.get("response") != CONTROL_SENTINEL:
        failures.append(
            f"control expected={CONTROL_SENTINEL}/exit0 "
            f"actual={control_document.get('response')!r}/exit{control.returncode}"
        )
    if len(control_requests) != 1:
        failures.append(
            f"control.requests expected=1 actual={len(control_requests)}"
        )

    resume = ctx.run_haider(
        ["resume", session_id, "--json", "--timeout", "5s"],
        timeout=RESUME_FIVE_SECONDS + RUN_TERMINAL_GRACE,
    )
    try:
        resume_document = json_document(resume, "resume")
    except Exception as error:
        resume_document = {}
        failures.append(f"resume.json actual={error}")
    if resume.timed_out or resume.returncode != 0:
        failures.append(
            f"resume.exit expected=0 actual={resume.returncode} "
            f"timed_out={str(resume.timed_out).lower()}"
        )
    if resume_document.get("schema") != "haider.session.resume.v1":
        failures.append(
            "resume.schema expected=haider.session.resume.v1 "
            f"actual={resume_document.get('schema')!r}"
        )
    if resume_document.get("completed") is not True:
        failures.append(
            f"resume.completed expected=true actual={resume_document.get('completed')!r}"
        )

    recover = ctx.run_haider(
        ["session", session_id, "recover", "--probe", "--json"],
        timeout=STATUS_REQUEST,
    )
    try:
        recover_document = json_document(recover, "recover")
    except Exception as error:
        recover_document = {}
        failures.append(f"recover.json actual={error}")
    if recover.timed_out:
        failures.append("recover outer process timed_out actual=true")
    if recover_document.get("schema") != "haider.session_recovery.v1":
        failures.append(
            "recover.schema expected=haider.session_recovery.v1 "
            f"actual={recover_document.get('schema')!r}"
        )
    completed = recover_document.get("completed")
    recover_code = recover_document.get("error", {}).get("code")
    if not (
        (recover.returncode == 0 and completed in (None, True))
        or (recover.returncode == 77 and completed is False and recover_code == "no_recovery")
    ):
        failures.append(
            "recover expected=typed_success_or_no_recovery/77 "
            f"actual_exit={recover.returncode} completed={completed!r} code={recover_code!r}"
        )

    if failures:
        for label, result in (
            ("status-before", before_status),
            ("source-events", events_result),
            ("replay", replay),
            ("status-after", after_status),
            ("control", control),
            ("resume", resume),
            ("recover", recover),
        ):
            artefacts.append(ctx.command_artefact(label, result))
    line = (
        "; ".join(failures)
        if failures
        else f"run_id={run_id} seq_order={source_seqs!r} call_ids={source_calls!r} "
        "replay_documents=1 replay_provider_requests=0 journal_unchanged=true "
        f"control_requests=1 resume_completed=true recover_code={recover_code or 'none'}"
    )
    return [
        Evidence(
            "replay_resume_recover",
            FAIL if failures else PASS,
            line,
            artefacts,
        )
    ]
