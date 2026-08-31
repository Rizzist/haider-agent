"""Installed-binary JSONL acceptance/cursor/terminal smoke."""

from __future__ import annotations

import json
from typing import Any

from gate import (
    DAEMON_STARTUP,
    DAEMON_STOP,
    FAIL,
    PASS,
    PROCESS_EXIT_GRACE,
    RUN_TERMINAL_GRACE,
    RUN_TIMEOUT,
    STATUS_REQUEST,
    Evidence,
)

id = "t0.run.jsonl_contract"
tier = "t0"
area = "headless"
needs = ("binary", "daemon", "network:none")
SENTINEL = "QA_GATE_JSONL_SEGMENT_CONSUMED"
EXPECTED_TERMINAL_KIND = "success"
script = [
    {"step": "emit_text", "text": SENTINEL},
    {"step": "finish", "reason": "end_turn"},
]
turns_expected = 1
# Registry #94: 30s spawn + 30s explicit run + 2s terminal grace +
# cleanup's 60s status request + 20s stop + 2s subprocess grace + 2s
# independent process-exit observation. Total = 146s.
budget = (
    DAEMON_STARTUP
    + RUN_TIMEOUT
    + RUN_TERMINAL_GRACE
    + STATUS_REQUEST
    + DAEMON_STOP
    + PROCESS_EXIT_GRACE
    + PROCESS_EXIT_GRACE
)
timed = True


def _completed_agent_text(document: dict[str, Any]) -> str | None:
    payload = document.get("payload")
    if not isinstance(payload, dict) or payload.get("type") != "item":
        return None
    if payload.get("event") != "completed":
        return None
    item = payload.get("item")
    if not isinstance(item, dict) or item.get("item") != "agent_message":
        return None
    text = item.get("text")
    return text if isinstance(text, str) else None


def run(ctx) -> list[Evidence]:
    result = ctx.run_haider(
        [
            "run",
            "-p",
            "x",
            "--provider",
            "fake",
            "--model",
            "fake-model",
            "--output",
            "jsonl",
            "--timeout",
            "30s",
        ],
        timeout=DAEMON_STARTUP + RUN_TIMEOUT + RUN_TERMINAL_GRACE,
    )
    failures: list[str] = []
    if result.timed_out:
        failures.append("process timed_out actual=true")
    if result.returncode != 0:
        failures.append(f"exit expected=0 actual={result.returncode}")
    if not result.stdout.endswith("\n"):
        failures.append("stdout LF termination expected=true actual=false")
    if "\r" in result.stdout:
        failures.append("stdout CR bytes expected=0 actual=present")

    documents: list[dict[str, Any]] = []
    for index, line in enumerate(result.stdout.splitlines(), start=1):
        if not line.strip():
            failures.append(f"jsonl line={index} actual=blank")
            continue
        try:
            value = json.loads(line)
        except json.JSONDecodeError as error:
            failures.append(f"jsonl line={index} invalid_json actual={error.msg}")
            continue
        if not isinstance(value, dict):
            failures.append(f"jsonl line={index} expected=object actual={type(value).__name__}")
            continue
        documents.append(value)

    accepted: dict[str, Any] = documents[0] if documents else {}
    if accepted.get("event") != "accepted":
        failures.append(f"first event expected=accepted actual={accepted.get('event')!r}")
    session_id = accepted.get("session_id")
    if not isinstance(session_id, str) or not session_id:
        failures.append(f"accepted session_id expected=nonempty actual={session_id!r}")
    head_seq = accepted.get("head_seq")
    if isinstance(head_seq, bool) or not isinstance(head_seq, int) or head_seq < 1:
        failures.append(f"accepted head_seq expected=positive_integer actual={head_seq!r}")

    envelopes = documents[1:]
    if not envelopes:
        failures.append("envelopes expected=nonempty actual=0")
    sequences = [envelope.get("seq") for envelope in envelopes]
    if envelopes and sequences[0] != head_seq:
        failures.append(f"first seq expected={head_seq!r} actual={sequences[0]!r}")
    for index, (before, after) in enumerate(zip(sequences, sequences[1:]), start=1):
        if isinstance(before, bool) or not isinstance(before, int):
            failures.append(f"seq[{index}] expected=integer actual={before!r}")
        elif after != before + 1:
            failures.append(f"seq[{index + 1}] expected={before + 1} actual={after!r}")
    if envelopes and any(envelope.get("session_id") != session_id for envelope in envelopes):
        failures.append("envelope session_id expected=accepted session actual=mismatch")

    terminals = [
        envelope
        for envelope in envelopes
        if isinstance(envelope.get("payload"), dict)
        and "terminal_kind" in envelope["payload"]
    ]
    actual_terminal = (
        terminals[0]["payload"].get("terminal_kind") if len(terminals) == 1 else None
    )
    if len(terminals) != 1:
        failures.append(f"typed terminals expected=1 actual={len(terminals)}")
    elif actual_terminal != EXPECTED_TERMINAL_KIND:
        failures.append(
            f"terminal_kind expected={EXPECTED_TERMINAL_KIND} actual={actual_terminal}"
        )
    if len(terminals) == 1 and terminals[0]["payload"].get("type") != "run_state":
        failures.append(
            f"terminal payload.type expected=run_state actual={terminals[0]['payload'].get('type')!r}"
        )
    if len(terminals) == 1 and envelopes and terminals[0] is not envelopes[-1]:
        failures.append("terminal position expected=last actual=not_last")

    completed_texts = [
        text for envelope in envelopes if (text := _completed_agent_text(envelope)) is not None
    ]
    if SENTINEL not in completed_texts:
        failures.append(
            f"consumed segment sentinel expected={SENTINEL!r} actual={completed_texts!r}"
        )

    if failures:
        artefact = ctx.command_artefact("jsonl-contract", result)
        return [
            Evidence(
                "jsonl_contract",
                FAIL,
                "; ".join(failures),
                [artefact],
            )
        ]
    return [
        Evidence(
            "jsonl_contract",
            PASS,
            f"accepted head_seq={head_seq} envelopes={len(envelopes)} contiguous=true "
            f"terminal_kind={actual_terminal} exit=0 segment_consumed=true finite_segments=1",
        )
    ]
