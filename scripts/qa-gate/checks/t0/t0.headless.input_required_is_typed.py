"""Headless request_input has one finite, typed, release-pinned resolution."""

from __future__ import annotations

import json

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
from gate.headless import jsonl_documents, terminal_payloads_from_jsonl

id = "t0.headless.input_required_is_typed"
tier = "t0"
area = "headless"
needs = ("binary", "daemon", "network:none")
CALL_ID = "qa-input-required"
CONTINUATION = "QA_INPUT_CONTINUED_WITHOUT_GUESS"
script = [
    {
        "step": "emit_request_input",
        "call_id": CALL_ID,
        "kind": "question",
        "title": "Need operator input",
    },
    {"step": "finish", "reason": "tool_use"},
    {"step": "expect_tool_result", "call_id": CALL_ID},
    {"step": "emit_text", "text": CONTINUATION},
    {"step": "finish", "reason": "end_turn"},
]
turns_expected = 2
HEADLESS_TWO_SECONDS = BudgetPart(
    "headless input resolution timeout",
    2.0,
    "haider run --timeout 2s; crates/haider-cli/src/run.rs:216-223",
)
# Registry #94: run 30+2+2, cleanup 60+20+2+2. Total=118s.
budget = (
    DAEMON_STARTUP
    + HEADLESS_TWO_SECONDS
    + RUN_TERMINAL_GRACE
    + STATUS_REQUEST
    + DAEMON_STOP
    + PROCESS_EXIT_GRACE
    + PROCESS_EXIT_GRACE
)
timed = False


def _input_resolution(documents):
    for document in documents:
        payload = document.get("payload")
        if not isinstance(payload, dict) or payload.get("type") != "tool_result":
            continue
        if payload.get("call_id") != CALL_ID:
            continue
        result = payload.get("result")
        if not isinstance(result, dict) or result.get("status") != "rejected":
            return f"status:{result.get('status')!r}" if isinstance(result, dict) else "malformed"
        preview = result.get("preview")
        try:
            parsed = json.loads(preview) if isinstance(preview, str) else {}
        except json.JSONDecodeError:
            return "invalid_preview"
        return parsed.get("code")
    return None


def run(ctx) -> list[Evidence]:
    result = ctx.run_haider(
        [
            "run",
            "--provider",
            "fake",
            "--model",
            "fake-model",
            "--output",
            "jsonl",
            "--timeout",
            "2s",
            "-p",
            "request operator input",
        ],
        timeout=DAEMON_STARTUP + HEADLESS_TWO_SECONDS + RUN_TERMINAL_GRACE,
        # This scripted fixture intentionally calls a non-core tool. Select
        # its named surface without changing permission or terminal pins.
        env_overrides={"HAIDER_TOOL_EXPOSURE": "request_input"},
    )
    failures = []
    try:
        documents = jsonl_documents(result, "headless input")
        terminals = terminal_payloads_from_jsonl(documents)
    except Exception as error:
        documents = []
        terminals = []
        failures.append(f"jsonl actual={error}")
    if result.timed_out:
        failures.append("outer process timed_out actual=true")
    # Installed 0.0.967 behavior: reject without guessing, feed the typed tool
    # result back to the provider, and continue to a successful terminal.
    if result.returncode != 0:
        failures.append(f"installed_pin.exit expected=0 actual={result.returncode}")
    resolution = _input_resolution(documents)
    if resolution != "no_human_available":
        failures.append(
            "installed_pin.input_resolution expected=no_human_available "
            f"actual={resolution!r}"
        )
    if len(terminals) != 1:
        failures.append(f"typed_terminals expected=1 actual={len(terminals)}")
    terminal = terminals[0] if len(terminals) == 1 else {}
    if terminal.get("terminal_kind") != "success":
        failures.append(
            "installed_pin.terminal_kind expected=success "
            f"actual={terminal.get('terminal_kind')!r}"
        )
    if terminal.get("state") != "done":
        failures.append(
            f"installed_pin.run_state expected=done actual={terminal.get('state')!r}"
        )
    texts = [
        document.get("payload", {}).get("item", {}).get("text")
        for document in documents
        if isinstance(document.get("payload"), dict)
        and isinstance(document["payload"].get("item"), dict)
    ]
    if CONTINUATION not in texts:
        failures.append(
            f"continuation expected={CONTINUATION!r} actual={texts!r}"
        )
    line = (
        "; ".join(failures)
        if failures
        else "installed_0.0.967_pin exit=0 input_resolution=no_human_available "
        "typed_terminals=1 terminal_kind=success run_state=done continuation=true"
    )
    return [
        Evidence(
            "input_required_typed",
            FAIL if failures else PASS,
            line,
            [ctx.command_artefact("headless-input", result)] if failures else [],
        )
    ]
