"""Compare palette, typed-slash, and RPC durable session effects."""

from __future__ import annotations

import json
import sqlite3

from gate import (
    DAEMON_STARTUP,
    DAEMON_STOP,
    FAIL,
    PASS,
    PROCESS_EXIT_GRACE,
    STATUS_REQUEST,
    BudgetSum,
    Evidence,
)
from gate.tui_probe import TUI_ACTION, TUI_BOOT, TUI_EXIT, TUI_REPAINT

id = "t0.tui.three_door_parity"
tier = "t0"
area = "tui"
needs = ("binary", "daemon", "pty", "network:none")
script = [
    {"step": "emit_text", "text": "QA_COMPACT_READY"},
    {"step": "finish", "reason": "end_turn"},
    {"step": "emit_text", "text": "QA_COMPACT_SUMMARY"},
    {"step": "finish", "reason": "end_turn"},
    {"step": "emit_text", "text": "QA_COMPACT_READY"},
    {"step": "finish", "reason": "end_turn"},
    {"step": "emit_text", "text": "QA_COMPACT_SUMMARY"},
    {"step": "finish", "reason": "end_turn"},
    {"step": "emit_text", "text": "QA_COMPACT_READY"},
    {"step": "finish", "reason": "end_turn"},
    {"step": "emit_text", "text": "QA_COMPACT_SUMMARY"},
    {"step": "finish", "reason": "end_turn"},
]
turns_expected = 6

# Registry #94: eleven PTY doors each own boot/action/repaint/reap. Eighty 60s
# request allowances conservatively cover status, 15 creates, 15 singular
# session projections, attachments/invocations, and the client-owned sweep;
# cleanup adds stop and two process-exit observations. Total = 80*60 +
# 11*(25+12+4+2.5) + 30 + 20 + 2 + 2 = 5332.5s.
budget = BudgetSum(
    (
        DAEMON_STARTUP,
        *((STATUS_REQUEST,) * 80),
        *((TUI_BOOT, TUI_ACTION, TUI_REPAINT, TUI_EXIT) * 11),
        DAEMON_STOP,
        PROCESS_EXIT_GRACE,
        PROCESS_EXIT_GRACE,
    )
)
timed = False

CHANGES = {
    "model": ("claude-opus-4-8", "session.select_model", "model"),
    "effort": ("xhigh", "session.select_effort", "effort"),
    "fast": (True, "session.select_fast", "fast"),
    "rename": ("QA parity title", "session.rename", "title"),
    "compact": (None, "session.compact", "compact"),
}

SUMMARY_KEYS = {
    "model": "last_model",
    "effort": "effort",
    "fast": "fast",
    "rename": "title",
}


def _receipt_rows(profile_dir, session_id: str) -> list[dict]:
    connection = sqlite3.connect(f"file:{profile_dir / 'store.sqlite'}?mode=ro", uri=True)
    try:
        rows = connection.execute(
                "select method, state, response_json, accepted_seq, final_revision "
                "from command_receipts where session_id=?1 "
                "and method <> 'session.create' order by rowid",
                (session_id,),
            ).fetchall()
        return [
            {
                "method": row[0],
                "state": row[1],
                "response": json.loads(row[2]) if row[2] else None,
                "accepted_seq": row[3],
                "final_revision": row[4],
            }
            for row in rows
        ]
    finally:
        connection.close()


def _meta(profile_dir, session_id: str):
    connection = sqlite3.connect(f"file:{profile_dir / 'store.sqlite'}?mode=ro", uri=True)
    try:
        row = connection.execute("select meta_json from sessions where id=?1", (session_id,)).fetchone()
        return json.loads(row[0]) if row else None
    finally:
        connection.close()


def _logical_snapshot(profile_dir):
    connection = sqlite3.connect(f"file:{profile_dir / 'store.sqlite'}?mode=ro", uri=True)
    try:
        database = tuple(connection.iterdump())
    finally:
        connection.close()
    files = tuple(
        (name, (profile_dir / name).read_bytes() if (profile_dir / name).exists() else None)
        for name in ("accounts.json", "providers.json", "tui-settings.json")
    )
    return database, files


def _contains_key_value(value, key: str, expected) -> bool:
    if isinstance(value, dict):
        if value.get(key) == expected:
            return True
        return any(_contains_key_value(child, key, expected) for child in value.values())
    if isinstance(value, list):
        return any(_contains_key_value(child, key, expected) for child in value)
    return False


def _session_summary(rpc, session_id: str) -> dict:
    """Read roster truth after the synchronous command result has settled."""

    response = rpc.request({"method": "session.list", "limit": 100})
    sessions = response.get("sessions")
    if not isinstance(sessions, list):
        raise RuntimeError(f"session.list sessions expected=list actual={sessions!r}")
    for summary in sessions:
        if isinstance(summary, dict) and summary.get("session_id") == session_id:
            return summary
    ids = [row.get("session_id") for row in sessions if isinstance(row, dict)]
    raise RuntimeError(f"session.list session expected={session_id!r} actual_ids={ids!r}")


def _event_tail(profile_dir, session_id: str, count: int) -> list[str]:
    connection = sqlite3.connect(f"file:{profile_dir / 'store.sqlite'}?mode=ro", uri=True)
    try:
        rows = connection.execute(
            "select payload_kind from events where session_id=?1 order by seq desc limit ?2",
            (session_id, count),
        ).fetchall()
        return [row[0] for row in reversed(rows)]
    finally:
        connection.close()


def _tui_change(ctx, command: str, value, session_id: str, *, palette: bool):
    from gate.tui_probe import TuiProcess

    tui = TuiProcess(ctx, session_id=session_id)
    try:
        if command == "compact":
            tui.type_slow("seed compact parity")
            tui.enter()
            if not tui.wait_for(lambda raw: b"QA_COMPACT_READY" in raw):
                raise RuntimeError("compact seed turn expected=QA_COMPACT_READY actual=absent")
            tui.settle(0.2)
        if palette and command == "model":
            tui.type_slow("/model")
            tui.enter()
            tui.settle(0.25)
            tui.type_slow(str(value))
            tui.enter()
            tui.settle(0.25)
            tui.enter()  # one API-provider stage
        elif palette and command == "effort":
            tui.type_slow(f"/effort {value}")
            tui.enter()
        elif palette and command == "rename":
            # Tab accepts the exact shared palette row without executing it;
            # append the requested title, then Enter performs that same
            # canonical command. This is the palette door, not a typed slash
            # bypass, and it catches completion anatomy that drops arguments.
            tui.type_slow("/rename")
            tui.tab()
            tui.type_slow(" " + str(value))
            tui.enter()
        elif palette:
            tui.type_slow("/" + command)
            tui.enter()
        else:
            argument = ""
            if command == "fast":
                argument = " on"
            elif value is not None:
                argument = " " + str(value)
            tui.type_slow(f"/{command}{argument} ")  # trailing space suppresses palette rows
            tui.enter()
            if command == "model":
                tui.settle(0.25)
                tui.enter()  # filtered model row
                tui.settle(0.25)
                tui.enter()  # one API-provider stage
        tui.settle(0.7)
        frames = tui.repaint_both()
        clean, audit = tui.close()
        return clean, audit, frames[0].text + "\n" + frames[1].text
    finally:
        if not tui.closed:
            tui.close()


def _rpc_change(rpc, command: str, value, session_id: str, serial: int):
    rpc.attach_control(session_id)
    if command == "fast":
        command_text = "/fast on"
    elif value is None:
        command_text = "/" + command
    else:
        command_text = f"/{command} {value}"
    return rpc.request(
        {
            "method": "command.invoke",
            "command_id": f"qa-parity-{command}-{serial}",
            "command": command_text,
            "session_id": session_id,
        }
    )


def run(ctx) -> list[Evidence]:
    from gate.tui_probe import RpcClient, session_json, start_daemon

    # A real persisted Anthropic profile gives all three doors the same
    # selectable model/effort/fast inventory without any network discovery.
    (ctx.profile_dir / "providers.json").write_text(
        json.dumps(
            {
                "providers": [
                    {
                        "provider_id": "anthropic",
                        "display_name": "anthropic",
                        "api_family": "anthropic_messages",
                        "base_url": "https://parity.openai.azure.com/openai/v1",
                        "enabled": True,
                        "auth_requirement": "api_key",
                        "configured_models": ["claude-opus-5", "claude-opus-4-8"],
                        "default_model": "claude-opus-5",
                        "provenance": "custom",
                        "trust": "full",
                    }
                ]
            },
            separators=(",", ":"),
        ),
        encoding="utf-8",
    )
    (ctx.profile_dir / "accounts.json").write_text(
        json.dumps(
            [
                {
                    "alias": "qa-parity-anthropic",
                    "provider": "anthropic",
                    "auth_method": "api_key",
                    "identity": "QA parity fixture",
                    "status": {"status": "ok"},
                    "active": True,
                }
            ],
            separators=(",", ":"),
        ),
        encoding="utf-8",
    )
    status = start_daemon(ctx)
    rpc = RpcClient(status["daemon"]["socket_path"])
    evidence: list[Evidence] = []
    try:
        serial = 0
        for command, (value, expected_method, truth_key) in CHANGES.items():
            doors: dict[str, dict] = {}
            for door in ("palette", "typed", "rpc"):
                session_id, _generation = rpc.create_session(
                    ctx.workspace_dir,
                    provider="anthropic",
                    model="claude-opus-5",
                    effort="high",
                    fast=False,
                )
                serial += 1
                text = ""
                audit = "rpc_transport=true"
                clean = True
                rpc_body = None
                if door == "rpc":
                    if command == "compact":
                        from gate.tui_probe import TuiProcess

                        seed_tui = TuiProcess(ctx, session_id=session_id)
                        try:
                            seed_tui.type_slow("seed compact parity")
                            seed_tui.enter()
                            if not seed_tui.wait_for(
                                lambda raw: b"QA_COMPACT_READY" in raw
                            ):
                                raise RuntimeError(
                                    "compact RPC seed expected=QA_COMPACT_READY actual=absent"
                                )
                            seed_tui.close()
                        finally:
                            if not seed_tui.closed:
                                seed_tui.close()
                    rpc_body = _rpc_change(rpc, command, value, session_id, serial)
                else:
                    clean, audit, text = _tui_change(
                        ctx, command, value, session_id, palette=door == "palette"
                    )
                # All three doors return only after their command result; the
                # receipt transaction is therefore already committed. Read it
                # once instead of adding an unowned polling sleep.
                receipt_rows = _receipt_rows(ctx.profile_dir, session_id)
                methods = [row["method"] for row in receipt_rows]
                metadata = _meta(ctx.profile_dir, session_id)
                receipt_outcome = None
                if isinstance(rpc_body, dict):
                    receipt_outcome = rpc_body.get("outcome", {}).get("kind")
                if command == "compact":
                    public = session_json(ctx, session_id)
                    projection = (
                        public.get("session")
                        if isinstance(public.get("session"), dict)
                        else {}
                    )
                    public_actual = projection.get("last_event_kinds", "<absent>")
                    public_truth = (
                        isinstance(public_actual, list)
                        and bool(public_actual)
                        and public_actual == _event_tail(ctx.profile_dir, session_id, len(public_actual))
                    )
                    committed_rows = [
                        row
                        for row in receipt_rows
                        if row["method"] == expected_method and row["state"] == "committed"
                    ]
                    durable_truth = bool(committed_rows) and all(
                        isinstance(row["response"], dict)
                        and row["response"].get("session_id") == session_id
                        and isinstance(row["response"].get("accepted_seq"), int)
                        for row in committed_rows
                    )
                else:
                    summary = _session_summary(rpc, session_id)
                    public_actual = summary.get(SUMMARY_KEYS[command], "<absent>")
                    public_truth = public_actual == value
                    durable_truth = _contains_key_value(metadata, truth_key, value)
                doors[door] = {
                    "methods": methods,
                    "receipt_rows": receipt_rows,
                    "public": public_truth,
                    "public_actual": public_actual,
                    "durable": durable_truth,
                    "clean": clean,
                    "audit": audit,
                    "rpc_kind": receipt_outcome,
                    "text": text,
                }
            receipt_equal = all(
                any(
                    row["method"] == expected_method and row["state"] == "committed"
                    for row in doors[door]["receipt_rows"]
                )
                for door in doors
            )
            durable_equal = all(doors[door]["durable"] for door in doors)
            public_equal = all(doors[door]["public"] for door in doors)
            clean = all(doors[door]["clean"] for door in doors)
            rpc_nested = doors["rpc"]["rpc_kind"] == "receipt"
            ok = receipt_equal and durable_equal and public_equal and clean and rpc_nested
            artefacts: list[str] = []
            if not ok:
                artefacts.append(
                    ctx.write_artefact(
                        f"three-door-{command}.json",
                        json.dumps(doors, indent=2, sort_keys=True, default=str),
                    )
                )
            evidence.append(
                Evidence(
                    command,
                    PASS if ok else FAIL,
                    f"{command} receipt_kind={expected_method} palette={doors['palette']['methods']!r} "
                    f"typed={doors['typed']['methods']!r} rpc={doors['rpc']['methods']!r} "
                    f"sqlite_truth=palette:{str(doors['palette']['durable']).lower()},"
                    f"typed:{str(doors['typed']['durable']).lower()},rpc:{str(doors['rpc']['durable']).lower()} "
                    f"authoritative_projection={'session_json_event_tail' if command == 'compact' else 'session.list'} "
                    f"projection_truth="
                    f"palette:{str(doors['palette']['public']).lower()},typed:{str(doors['typed']['public']).lower()},"
                    f"rpc:{str(doors['rpc']['public']).lower()} projection_actual="
                    f"palette:{doors['palette']['public_actual']!r},typed:{doors['typed']['public_actual']!r},"
                    f"rpc:{doors['rpc']['public_actual']!r} receipt_rows="
                    f"palette:{doors['palette']['receipt_rows']!r},typed:{doors['typed']['receipt_rows']!r},"
                    f"rpc:{doors['rpc']['receipt_rows']!r} rpc_outcome={doors['rpc']['rpc_kind']!r}",
                    artefacts,
                )
            )

        before = _logical_snapshot(ctx.profile_dir)
        client_items = [
            item
            for item in rpc.command_list("", in_session=True)
            if item.get("kind") == "built_in" and item.get("ownership") == "client_view"
        ]
        outcomes: list[tuple[str, object]] = []
        for index, item in enumerate(client_items, start=1):
            name = item["name"]
            body = rpc.request(
                {
                    "method": "command.invoke",
                    "command_id": f"qa-client-owned-{index}",
                    "command": "/" + name,
                }
            )
            outcomes.append((name, body.get("outcome", {}).get("kind")))
        after = _logical_snapshot(ctx.profile_dir)
        wrong = [(name, kind) for name, kind in outcomes if kind != "client_owned"]
        evidence.append(
            Evidence(
                "rpc_client_owned_zero_mutation",
                PASS if not wrong and before == after else FAIL,
                f"client_owned_count={len(outcomes)} wrong_outcomes={wrong!r} "
                f"daemon_mutation_rows={str(before != after).lower()}",
            )
        )
        return evidence
    finally:
        rpc.close()
