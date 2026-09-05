"""Public CLI delegation consumes one child segment and returns its durable report."""

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
from gate.context import parse_single_json

id = "t0.agent.spawn_result"
tier = "t0"
area = "agent"
needs = ("binary", "daemon", "network:none")
SENTINEL = "QA_AGENT_DURABLE_CHILD_RESULT"
script = [
    {"step": "emit_text", "text": SENTINEL},
    {"step": "finish", "reason": "end_turn"},
]
turns_expected = 1
OBSERVATION = BudgetPart(
    "public agent CLI observation timeout",
    10.0,
    "haider agent spawn/wait --timeout 10s; the wait deadline only observes",
)
SPAWN_BOUND = DAEMON_STARTUP + STATUS_REQUEST + OBSERVATION + RUN_TERMINAL_GRACE
WAIT_BOUND = DAEMON_STARTUP + STATUS_REQUEST + OBSERVATION + RUN_TERMINAL_GRACE
# Registry #94: each CLI invocation encloses cold startup (30), its request
# (60), --timeout (10), and terminal publication (2): 102s each. Cleanup adds
# status 60 + stop 20 + stop-process grace 2 + PID observation 2. Total 288s.
budget = (
    SPAWN_BOUND
    + WAIT_BOUND
    + STATUS_REQUEST
    + DAEMON_STOP
    + PROCESS_EXIT_GRACE
    + PROCESS_EXIT_GRACE
)
timed = False


def _document(result, verb):
    if result.timed_out or result.returncode != 0:
        raise ValueError(
            f"{verb} exit expected=0 actual={result.returncode} "
            f"outer_timeout={result.timed_out}"
        )
    document = parse_single_json(result.stdout, verb)
    expected_schema = f"haider.agent.{verb}.v1"
    if document.get("schema") != expected_schema:
        raise ValueError(f"{verb} schema expected={expected_schema} actual={document.get('schema')!r}")
    if document.get("ok") is not True or document.get("error") is not None:
        raise ValueError(f"{verb} expected typed success actual={document!r}")
    payload = document.get("result")
    if not isinstance(payload, dict):
        raise ValueError(f"{verb} result expected=object actual={payload!r}")
    return payload


def run(ctx) -> list[Evidence]:
    commands = []
    try:
        spawned = ctx.run_haider(
            ["agent", "spawn", "return the QA child report", "--task", "qa-spawn-result",
             "--provider", "fake", "--model", "fake-model", "--json", "--timeout", "10s"],
            timeout=SPAWN_BOUND,
        )
        commands.append(("agent-spawn", spawned))
        child = _document(spawned, "spawn")
        for field in ("session_id", "run_id", "agent_id", "child_session_id", "child_run_id"):
            if not isinstance(child.get(field), str) or not child[field]:
                raise ValueError(f"spawn {field} expected=nonempty actual={child.get(field)!r}")
        if child["session_id"] == child["child_session_id"] or child["run_id"] == child["child_run_id"]:
            raise ValueError("spawn parent/child identities must remain distinct")
        waited = ctx.run_haider(
            ["agent", "wait", child["session_id"], child["agent_id"],
             "--json", "--timeout", "10s", "--no-spawn"],
            timeout=WAIT_BOUND,
        )
        commands.append(("agent-wait", waited))
        result = _document(waited, "wait")
        for field in ("session_id", "agent_id", "child_session_id", "child_run_id"):
            if result.get(field) != child[field]:
                raise ValueError(f"wait {field} expected={child[field]!r} actual={result.get(field)!r}")
        if result.get("state") != "done":
            raise ValueError(f"child terminal expected=done actual={result.get('state')!r}")
        if result.get("report_source") != "child_result":
            raise ValueError(f"report provenance expected=child_result actual={result.get('report_source')!r}")
        for field in ("terminal_seq", "child_result_seq"):
            sequence = result.get(field)
            if isinstance(sequence, bool) or not isinstance(sequence, int) or sequence < 1:
                raise ValueError(f"durable {field} expected=positive integer actual={sequence!r}")
        report = result.get("report")
        if not isinstance(report, dict) or report.get("agent") != child["agent_id"]:
            raise ValueError(f"ChildResult agent expected={child['agent_id']} actual={report!r}")
        if report.get("summary") != SENTINEL:
            raise ValueError(f"ChildResult summary expected={SENTINEL} actual={report.get('summary')!r}")
    except Exception as error:
        return [Evidence("spawn_result", FAIL, str(error), [
            ctx.command_artefact(label, command) for label, command in commands
        ])]
    # runner.execute_check always appends CheckContext.cleanup evidence. Its
    # status-owned PID, clean-stop receipt and PID disappearance are required;
    # this module never substitutes a process-name census or a success stub.
    return [Evidence(
        "spawn_result", PASS,
        f"spawn_exit=0 wait_exit=0 child_state=done child_result_seq={result['child_result_seq']} "
        f"terminal_seq={result['terminal_seq']} report_nonce=true finite_child_segments=1",
    )]
