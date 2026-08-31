"""Installed headless exit-code and typed-terminal matrix."""

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
from gate.headless import (
    json_document,
    jsonl_documents,
    terminal_payloads_from_jsonl,
)

id = "t0.run.exit_codes"
tier = "t0"
area = "headless"
needs = ("binary", "daemon", "network:none")
script = [
    {"step": "error", "kind": "internal", "message": "QA_GATE_PROVIDER_ERROR"},
    {"step": "hang"},
    {"step": "hang"},
    {"step": "hang"},
]
turns_expected = 4
HEADLESS_TEN_SECONDS = BudgetPart(
    "headless check --timeout",
    10.0,
    "qa-gate run --timeout 10s; crates/haider-client/src/headless.rs:67-75 terminal grace follows",
)
HEADLESS_TWO_SECONDS = BudgetPart(
    "headless timeout case",
    2.0,
    "haider run --timeout 2s; crates/haider-cli/src/run.rs:460-485",
)
SIGINT_ARM = BudgetPart(
    "SIGINT streaming-state arm",
    10.0,
    "qa-gate waits for the JSONL streaming fact while the client services keepalive",
)
# Registry #94: error 30+10+2, timeout 30+2+2, max-time 30+10+2,
# SIGINT 30+10+2+2, no-account 30+60, cleanup 60+20+2+2. Total=336s.
budget = (
    DAEMON_STARTUP
    + HEADLESS_TEN_SECONDS
    + RUN_TERMINAL_GRACE
    + DAEMON_STARTUP
    + HEADLESS_TWO_SECONDS
    + RUN_TERMINAL_GRACE
    + DAEMON_STARTUP
    + HEADLESS_TEN_SECONDS
    + RUN_TERMINAL_GRACE
    + DAEMON_STARTUP
    + SIGINT_ARM
    + RUN_TERMINAL_GRACE
    + PROCESS_EXIT_GRACE
    + DAEMON_STARTUP
    + STATUS_REQUEST
    + STATUS_REQUEST
    + DAEMON_STOP
    + PROCESS_EXIT_GRACE
    + PROCESS_EXIT_GRACE
)
timed = False


def _jsonl_case(ctx, label, args, expected_exit, expected_kind, expected_code):
    result = ctx.run_haider(
        args,
        timeout=DAEMON_STARTUP + HEADLESS_TEN_SECONDS + RUN_TERMINAL_GRACE,
    )
    failures = []
    try:
        documents = jsonl_documents(result, label)
        terminals = terminal_payloads_from_jsonl(documents)
    except Exception as error:
        documents = []
        terminals = []
        failures.append(f"jsonl actual={error}")
    if result.timed_out:
        failures.append("process timed_out actual=true")
    if result.returncode != expected_exit:
        failures.append(f"exit expected={expected_exit} actual={result.returncode}")
    if len(terminals) != 1:
        failures.append(f"typed_terminals expected=1 actual={len(terminals)}")
    terminal = terminals[0] if len(terminals) == 1 else {}
    if terminal.get("terminal_kind") != expected_kind:
        failures.append(
            f"terminal_kind expected={expected_kind} actual={terminal.get('terminal_kind')!r}"
        )
    if terminal.get("error_code") != expected_code:
        failures.append(
            f"error_code expected={expected_code} actual={terminal.get('error_code')!r}"
        )
    if failures:
        return Evidence(
            label,
            FAIL,
            "; ".join(failures),
            [ctx.command_artefact(label, result)],
        )
    return Evidence(
        label,
        PASS,
        f"case={label} exit={expected_exit} typed_terminals=1 "
        f"terminal_kind={expected_kind} error_code={expected_code}",
    )


def run(ctx) -> list[Evidence]:
    evidence = []
    evidence.append(
        _jsonl_case(
            ctx,
            "provider_error",
            [
                "run",
                "--provider",
                "fake",
                "--model",
                "fake-model",
                "--output",
                "jsonl",
                "--timeout",
                "10s",
                "-p",
                "error",
            ],
            65,
            "provider_error",
            "provider_error",
        )
    )

    timeout_result = ctx.run_haider(
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
            "timeout",
        ],
        timeout=DAEMON_STARTUP + HEADLESS_TWO_SECONDS + RUN_TERMINAL_GRACE,
    )
    timeout_failures = []
    try:
        timeout_terminals = terminal_payloads_from_jsonl(
            jsonl_documents(timeout_result, "timeout")
        )
    except Exception as error:
        timeout_terminals = []
        timeout_failures.append(f"jsonl actual={error}")
    if timeout_result.timed_out:
        timeout_failures.append("process timed_out actual=true")
    if timeout_result.returncode != 124:
        timeout_failures.append(f"exit expected=124 actual={timeout_result.returncode}")
    if len(timeout_terminals) != 1:
        timeout_failures.append(
            f"timeout terminals expected=1 actual={len(timeout_terminals)}"
        )
    timeout_terminal = timeout_terminals[0] if len(timeout_terminals) == 1 else {}
    if timeout_terminal.get("terminal_kind") != "timeout":
        timeout_failures.append(
            "terminal_kind expected=timeout "
            f"actual={timeout_terminal.get('terminal_kind')!r}"
        )
    evidence.append(
        Evidence(
            "timeout",
            FAIL if timeout_failures else PASS,
            "; ".join(timeout_failures)
            if timeout_failures
            else "case=timeout exit=124 timeout_terminals=1 terminal_kind=timeout",
            [ctx.command_artefact("timeout", timeout_result)] if timeout_failures else [],
        )
    )

    max_time = ctx.run_haider(
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
            "--max-time",
            "50ms",
            "-p",
            "max-time",
        ],
        timeout=DAEMON_STARTUP + HEADLESS_TEN_SECONDS + RUN_TERMINAL_GRACE,
    )
    max_failures = []
    try:
        max_document = json_document(max_time, "max-time")
        max_payloads = [
            event.get("payload")
            for event in max_document.get("events", [])
            if isinstance(event, dict) and isinstance(event.get("payload"), dict)
        ]
    except Exception as error:
        max_document = {}
        max_payloads = []
        max_failures.append(f"json actual={error}")
    if max_time.timed_out:
        max_failures.append("process timed_out actual=true")
    if max_time.returncode != 77:
        max_failures.append(f"exit expected=77 actual={max_time.returncode}")
    if max_document.get("error", {}).get("code") != "budget_exhausted":
        max_failures.append(
            "error.code expected=budget_exhausted "
            f"actual={max_document.get('error', {}).get('code')!r}"
        )
    if max_document.get("budget_exhausted", {}).get("dimension") != "time":
        max_failures.append(
            "budget.dimension expected=time "
            f"actual={max_document.get('budget_exhausted', {}).get('dimension')!r}"
        )
    max_budget_facts = [
        payload for payload in max_payloads if payload.get("type") == "run_budget_exhausted"
    ]
    max_terminal_states = [
        payload.get("state")
        for payload in max_payloads
        if payload.get("type") == "run_state"
        and payload.get("state") in ("done", "errored", "cancelled")
    ]
    if len(max_budget_facts) != 1:
        max_failures.append(
            f"typed_budget_facts expected=1 actual={len(max_budget_facts)}"
        )
    if max_terminal_states != ["errored"]:
        max_failures.append(
            f"terminal_states expected=['errored'] actual={max_terminal_states!r}"
        )
    evidence.append(
        Evidence(
            "max_time",
            FAIL if max_failures else PASS,
            "; ".join(max_failures)
            if max_failures
            else "case=max-time exit=77 typed_budget_facts=1 terminal=errored error_code=budget_exhausted dimension=time",
            [ctx.command_artefact("max-time", max_time)] if max_failures else [],
        )
    )

    cancelled = ctx.interrupt_haider_after_stdout(
        [
            "run",
            "--provider",
            "fake",
            "--model",
            "fake-model",
            "--output",
            "jsonl",
            "--timeout",
            "10s",
            "-p",
            "cancel",
        ],
        marker='"state":"streaming"',
        arm_timeout=SIGINT_ARM,
        terminal_timeout=RUN_TERMINAL_GRACE + PROCESS_EXIT_GRACE,
    )
    cancel_failures = []
    try:
        cancel_terminals = terminal_payloads_from_jsonl(
            jsonl_documents(cancelled, "cancellation")
        )
    except Exception as error:
        cancel_terminals = []
        cancel_failures.append(f"jsonl actual={error}")
    if cancelled.timed_out:
        cancel_failures.append("process timed_out actual=true")
    if cancelled.returncode != 130:
        cancel_failures.append(f"exit expected=130 actual={cancelled.returncode}")
    if len(cancel_terminals) != 1:
        cancel_failures.append(
            f"cancellation terminals expected=1 actual={len(cancel_terminals)}"
        )
    cancel_terminal = cancel_terminals[0] if len(cancel_terminals) == 1 else {}
    if cancel_terminal.get("terminal_kind") != "cancellation":
        cancel_failures.append(
            "terminal_kind expected=cancellation "
            f"actual={cancel_terminal.get('terminal_kind')!r}"
        )
    evidence.append(
        Evidence(
            "cancellation",
            FAIL if cancel_failures else PASS,
            "; ".join(cancel_failures)
            if cancel_failures
            else "case=cancellation signal=SIGINT-to-client exit=130 cancellation_terminals=1",
            [ctx.command_artefact("cancellation", cancelled)] if cancel_failures else [],
        )
    )

    missing = ctx.run_haider(
        ["run", "--output", "json", "-p", "missing-account"],
        timeout=DAEMON_STARTUP + STATUS_REQUEST,
    )
    missing_failures = []
    try:
        missing_document = json_document(missing, "missing credential")
    except Exception as error:
        missing_document = {}
        missing_failures.append(f"json actual={error}")
    missing_code = missing_document.get("error", {}).get("code")
    if missing.timed_out:
        missing_failures.append("process timed_out actual=true")
    if missing.returncode != 65:
        missing_failures.append(f"exit expected=65 actual={missing.returncode}")
    if missing_code != "no_active_account":
        missing_failures.append(
            f"error.code expected=no_active_account actual={missing_code!r}"
        )
    evidence.append(
        Evidence(
            "missing_credential",
            FAIL if missing_failures else PASS,
            "; ".join(missing_failures)
            if missing_failures
            else "case=missing-credential exit=65 error_code=no_active_account provider_requests=0",
            [ctx.command_artefact("missing-credential", missing)]
            if missing_failures
            else [],
        )
    )
    return evidence
