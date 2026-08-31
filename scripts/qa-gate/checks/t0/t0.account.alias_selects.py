"""An account alias selects and persists the matching custom provider."""

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
from gate.openai_stub import OpenAIStub

id = "t0.account.alias_selects"
tier = "t0"
area = "account"
needs = ("binary", "daemon", "network:none")
script = []
turns_expected = 0
RUN_TEN_SECONDS = BudgetPart(
    "selected account run timeout",
    10.0,
    "haider run --timeout 10s; crates/haider-cli/src/run.rs:216-223",
)
# Registry #94: first account add may start the daemon (30+60), second add 60,
# run 10+2, session inspection 60, cleanup 60+20+2+2. Total=306s.
budget = (
    DAEMON_STARTUP
    + STATUS_REQUEST
    + STATUS_REQUEST
    + RUN_TEN_SECONDS
    + RUN_TERMINAL_GRACE
    + STATUS_REQUEST
    + STATUS_REQUEST
    + DAEMON_STOP
    + PROCESS_EXIT_GRACE
    + PROCESS_EXIT_GRACE
)
timed = False


def _add_account(ctx, alias: str, server: OpenAIStub, *, first: bool):
    return ctx.run_haider(
        [
            "account",
            "add",
            alias,
            "--base-url",
            server.base_url,
            "--api-family",
            "openai",
            "--no-auth",
            "--full",
            "--json",
        ],
        timeout=(DAEMON_STARTUP + STATUS_REQUEST) if first else STATUS_REQUEST,
    )


def run(ctx) -> list[Evidence]:
    # This check owns real loopback listeners and must not use the daemon fake seam.
    ctx.env.pop("HAIDER_TEST_FAKE_PROVIDER", None)
    server_a = OpenAIStub("SENTINEL_ACCOUNT_A")
    server_b = OpenAIStub("SENTINEL_ACCOUNT_B")
    server_a.start()
    server_b.start()
    try:
        add_a = _add_account(ctx, "qa-a", server_a, first=True)
        add_b = _add_account(ctx, "qa-b", server_b, first=False)
        run_result = ctx.run_haider(
            [
                "run",
                "--account",
                "qa-b",
                "--model",
                "qa-b/fixture-model",
                "--output",
                "json",
                "--timeout",
                "10s",
                "-p",
                "select account b",
            ],
            timeout=RUN_TEN_SECONDS + RUN_TERMINAL_GRACE,
        )

        failures = []
        documents = {}
        for label, result in (("add_a", add_a), ("add_b", add_b), ("run", run_result)):
            try:
                documents[label] = json_document(result, label)
            except Exception as error:
                documents[label] = {}
                failures.append(f"{label}.json actual={error}")
            if result.timed_out or result.returncode != 0:
                failures.append(
                    f"{label}.exit expected=0 actual={result.returncode} "
                    f"timed_out={str(result.timed_out).lower()}"
                )

        response = documents["run"].get("response")
        run_error = documents["run"].get("error")
        error_code = run_error.get("code") if isinstance(run_error, dict) else None
        if error_code is not None:
            failures.append(f"error.code expected=none actual={error_code!r}")
        if response != "SENTINEL_ACCOUNT_B":
            failures.append(
                f"response expected=SENTINEL_ACCOUNT_B actual={response!r}"
            )
        a_requests = server_a.chat_count
        b_requests = server_b.chat_count
        if a_requests != 0:
            failures.append(f"a_requests expected=0 actual={a_requests}")
        if b_requests != 1:
            failures.append(f"b_requests expected=1 actual={b_requests}")

        session_id = documents["run"].get("session_id")
        session_result = None
        persisted_provider = None
        persisted_account = None
        if isinstance(session_id, str) and session_id:
            session_result = ctx.run_haider(
                ["session", session_id, "--json"], timeout=STATUS_REQUEST
            )
            try:
                session_document = json_document(session_result, "session")
            except Exception as error:
                session_document = {}
                failures.append(f"session.json actual={error}")
            if session_result.timed_out or session_result.returncode != 0:
                failures.append(
                    f"session.exit expected=0 actual={session_result.returncode} "
                    f"timed_out={str(session_result.timed_out).lower()}"
                )
            persisted_provider = session_document.get("session", {}).get("provider")
            if persisted_provider != "qa-b":
                failures.append(
                    "persisted_provider expected=qa-b "
                    f"actual={persisted_provider!r}"
                )
            persisted_account = session_document.get("session", {}).get("account_alias")
            if persisted_account != "qa-b":
                failures.append(
                    "persisted_account_alias expected=qa-b "
                    f"actual={persisted_account!r}"
                )
        else:
            failures.append(f"session_id expected=nonempty actual={session_id!r}")

        artefacts = []
        if failures:
            artefacts.extend(
                [
                    ctx.command_artefact("account-add-a", add_a),
                    ctx.command_artefact("account-add-b", add_b),
                    ctx.command_artefact("account-run-b", run_result),
                ]
            )
            if session_result is not None:
                artefacts.append(ctx.command_artefact("account-session-b", session_result))
        line = (
            "; ".join(failures)
            if failures
            else "selected=qa-b response=SENTINEL_ACCOUNT_B a_requests=0 "
            "b_requests=1 persisted_provider=qa-b persisted_account_alias=qa-b"
        )
        return [
            Evidence(
                "account_b_selected_and_persisted",
                FAIL if failures else PASS,
                line,
                artefacts,
            )
        ]
    finally:
        server_b.close()
        server_a.close()
