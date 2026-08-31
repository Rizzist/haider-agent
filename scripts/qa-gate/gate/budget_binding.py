"""Shared black-box assertions for pre-provider budget binding."""

from __future__ import annotations

from .contract import FAIL, PASS, BudgetPart, BudgetSum, Evidence
from .headless import event_payloads, json_document, provider_request_ordinals


def check_budget_binding(
    ctx,
    *,
    flag: str,
    low_value: str,
    high_value: str,
    dimension: str,
    control_sentinel: str,
    process_timeout: BudgetPart | BudgetSum,
) -> list[Evidence]:
    base = [
        "run",
        "--provider",
        "openai",
        "--model",
        "gpt-5",
        "--output",
        "json",
        "--timeout",
        "10s",
        "-p",
        f"qa-{dimension}-budget",
    ]
    control, control_cleanup = ctx.run_isolated_haider(
        f"{dimension}-above-bound",
        [*base, flag, high_value],
        timeout=process_timeout,
    )
    control_failures: list[str] = []
    try:
        control_document = json_document(control, f"{dimension} control")
    except Exception as error:
        control_document = {}
        control_failures.append(f"json actual={error}")
    control_requests = len(provider_request_ordinals(control_document))
    if control.timed_out:
        control_failures.append("process timed_out actual=true")
    if control.returncode != 0:
        control_failures.append(f"exit expected=0 actual={control.returncode}")
    if control_document.get("response") != control_sentinel:
        control_failures.append(
            f"response expected={control_sentinel!r} "
            f"actual={control_document.get('response')!r}"
        )
    if control_requests != 1:
        control_failures.append(f"requests_made={control_requests} expected=1")
    control_evidence = Evidence(
        "above_bound_control",
        FAIL if control_failures else PASS,
        "; ".join(control_failures)
        if control_failures
        else f"dimension={dimension} above_bound=true requests_made=1 expected=1 "
        "exit=0 spare_segments=0",
        [ctx.command_artefact(f"{dimension}-control", control)]
        if control_failures
        else [],
    )

    below = ctx.run_haider(
        [*base, flag, low_value],
        timeout=process_timeout,
    )
    below_failures: list[str] = []
    try:
        below_document = json_document(below, f"{dimension} below-bound")
    except Exception as error:
        below_document = {}
        below_failures.append(f"json actual={error}")
    requests_made = len(provider_request_ordinals(below_document))
    payloads = event_payloads(below_document)
    budget_facts = [
        payload for payload in payloads if payload.get("type") == "run_budget_exhausted"
    ]
    terminal_states = [
        payload.get("state")
        for payload in payloads
        if payload.get("type") == "run_state"
        and payload.get("state") in ("done", "errored", "cancelled")
    ]
    if below.timed_out:
        below_failures.append("process timed_out actual=true")
    if requests_made != 0:
        below_failures.append(
            f"requests_made={requests_made} expected=0 defect=budget_bound_after_exchange"
        )
    if below.returncode != 77:
        below_failures.append(f"exit actual={below.returncode} expected=77")
    error_code = below_document.get("error", {}).get("code")
    if error_code != "budget_exhausted":
        below_failures.append(
            f"error_code actual={error_code!r} expected=budget_exhausted"
        )
    actual_dimension = below_document.get("budget_exhausted", {}).get("dimension")
    if actual_dimension != dimension:
        below_failures.append(
            f"dimension actual={actual_dimension!r} expected={dimension}"
        )
    if len(budget_facts) != 1:
        below_failures.append(
            f"typed_budget_facts actual={len(budget_facts)} expected=1"
        )
    if terminal_states != ["errored"]:
        below_failures.append(
            f"terminal_states actual={terminal_states!r} expected=['errored']"
        )
    below_line = (
        "; ".join(below_failures)
        if below_failures
        else f"dimension={dimension} requests_made=0 expected=0 typed_budget_facts=1 "
        "terminal=errored exit=77 before_exchange=true"
    )
    below_evidence = Evidence(
        "below_bound",
        FAIL if below_failures else PASS,
        below_line,
        [ctx.command_artefact(f"{dimension}-below-bound", below)]
        if below_failures
        else [],
    )

    return [control_evidence, below_evidence, control_cleanup]
