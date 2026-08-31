"""Prove every shared command palette activation closes in one action."""

from __future__ import annotations

import json

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

id = "t0.tui.palette_activation_closure"
tier = "t0"
area = "tui"
needs = ("binary", "daemon", "pty", "network:none")
expected_fail_until = "0.0.968"
script = [
    {"step": "emit_text", "text": "QA_PALETTE_HISTORY_READY"},
    {"step": "finish", "reason": "end_turn"},
]
turns_expected = 1

# Registry #94: one history seed, every catalog action (currently 41), and seven argument-stage probes
# each own 25s boot + 12s observation + three 4s two-size repaint envelopes + 2.5s
# clean exit. Fifty-six request allowances cover status/catalog, the history
# session, one isolated live session per command, seven argument-stage
# sessions, and cleanup. Total =
# 49*(25+12+3*4+2.5)+56*60+30+20+2+2 = 5937.5s.
budget = BudgetSum(
    (
        DAEMON_STARTUP,
        *((STATUS_REQUEST,) * 56),
        *((TUI_BOOT, TUI_ACTION, TUI_REPAINT, TUI_REPAINT, TUI_REPAINT, TUI_EXIT) * 49),
        DAEMON_STOP,
        PROCESS_EXIT_GRACE,
        PROCESS_EXIT_GRACE,
    )
)
timed = False

DEMO_COMMANDS = frozenset(("aura", "voice", "say", "tools"))
ARG_STAGE_COMMANDS = frozenset(("login", "provider", "account", "theme", "model", "usage", "effort"))
HISTORY_COMMANDS = frozenset(("fork", "branch", "undo", "redo", "checkpoints", "rollback", "compact", "history"))
REMEDY_MARKERS = (
    "try /",
    "use /",
    "requires ",
    "needs ",
    "pick ",
    "run /",
    "not supported",
    "unavailable",
    "session only",
    "demo only",
    "give ",
)

SEMANTIC_MARKERS = {
    "help": ("commands",),
    "model": ("models",),
    "theme": ("themes", "follow the terminal"),
    "provider": ("provider",),
    "effort": ("effort",),
    "fast": ("fast",),
    "tree": ("session tree", "branches"),
    "fork": ("history", "prompt"),
    "branch": ("branches",),
    "checkpoints": ("checkpoint",),
    "undo": ("checkpoint", "undo"),
    "redo": ("checkpoint", "redo"),
    "rollback": ("checkpoint", "rollback"),
    "attach": ("attach", "file"),
    "sessions": ("sessions",),
    "aura": ("aura",),
    "peer": ("peers", "peer"),
    "ssh": ("ssh",),
    "shells": ("shells", "terminal"),
    "accounts": ("accounts",),
    "account": ("account",),
    "providers": ("providers",),
    "usage": ("usage",),
    "login": ("login", "oauth", "api"),
    "clear": ("sessions", "new session"),
    "back": ("sessions", "new session"),
    "compact": ("compact",),
    "tokens": ("tokens", "context"),
    "history": ("history", "prompts"),
    "hooks": ("hooks",),
    "voice": ("voice",),
    "say": ("voice", "speak"),
    "talk": ("talk", "dictat"),
    "tools": ("tools",),
    "queue": ("queue", "steer"),
    "graph": ("graph",),
    "workflows": ("workflows",),
    "loom": ("loom", "agent types"),
    "update": ("update",),
    "rename": ("rename",),
    "reset": ("reset",),
}

# These are the command-owned cards/screens that do not necessarily introduce
# a word absent from the session baseline (for example, a prompt can already
# contain "history"). Pin their actual UI anatomy instead of using a generic
# keyword-delta heuristic.
SURFACE_SIGNATURES = {
    "help": ("esc closes", "commands"),
    "fork": ("↑↓ / digits choose", "f fork into a new session"),
    "branch": ("branch — switch the displayed branch", "menu.answer"),
    "aura": ("native duplex", "hold to talk"),
    "history": ("↑↓ / digits choose", "⏎ load"),
    "sessions": ("session-", "turns"),
    "update": ("checking for updates",),
}

# Exact, command-specific refusal outcomes are real activations. They are
# intentionally narrower than a general footer change, so the forbidden
# cleared-composer/dim-flash shape still fails.
REFUSAL_SIGNATURES = {
    "undo": ("not_found — no matching checkpoint exists on this branch",),
    "redo": ("not_found — no matching checkpoint exists on this branch",),
    "checkpoints": ("no durable file checkpoints on this branch",),
    "rollback": ("no matching durable turn on this branch",),
    "talk": ("talk failed — microphone unavailable",),
}

LOGIN_DEFECT_NOTE = (
    "defect: docs/research/w5-provider-research-report.md:480-488 defines /login "
    "as provider-then-method slots; crates/haider-tui/src/app.rs:12941-12943 "
    "replaces the composer with only the latest PaletteItem::Arg value"
)

RECEIPT_METHODS = {
    "model": "session.select_model",
    "provider": "session.select_model",
    "effort": "session.select_effort",
    "fast": "session.select_fast",
    "compact": "session.compact",
    "rename": "session.rename",
}


def _pin_tui_identity(ctx) -> None:
    """Keep one command's local picker choice from contaminating the next."""

    (ctx.profile_dir / "tui-settings.json").write_text(
        json.dumps(
            {
                "version": 1,
                "theme": "system",
                "notifications": True,
                "last_provider": "anthropic",
                "last_model": "claude-opus-5",
            },
            separators=(",", ":"),
        ),
        encoding="utf-8",
    )


def _screen_changed(before, after) -> bool:
    from gate.tui_probe import changed_body

    return bool(changed_body(before[0], after[0])) and bool(changed_body(before[1], after[1]))


def _remedy(command: str, before: str, after: str) -> str | None:
    """Require a newly painted, command-named, actionable refusal."""

    before_lines = {line.strip().lower() for line in before.splitlines()}
    for line in after.splitlines():
        lowered = line.strip().lower()
        if lowered in before_lines or f"/{command}" not in lowered:
            continue
        marker = next((candidate for candidate in REMEDY_MARKERS if candidate in lowered), None)
        if marker is not None:
            return marker
    return None


def _signature(command: str, before: str, after: str, signatures: dict[str, tuple[str, ...]]) -> str | None:
    """Return the exact newly painted command-owned surface/refusal signature."""

    expected = signatures.get(command)
    if expected is None:
        return None
    before_lower = before.lower()
    after_lower = after.lower()
    if all(marker.lower() in after_lower for marker in expected) and any(
        marker.lower() not in before_lower for marker in expected
    ):
        return " + ".join(expected)
    return None


def _activate(ctx, catalog: list[dict], command: str, session_id: str):
    from gate.tui_probe import TuiProcess, action_rows, durable_snapshot, snapshot_delta

    demo = command in DEMO_COMMANDS
    if not demo:
        _pin_tui_identity(ctx)
    tui = TuiProcess(ctx, demo=demo, session_id=None if demo else session_id)
    try:
        baseline = tui.repaint_both()
        before = durable_snapshot(ctx.profile_dir)
        # `login`/`provider`/`account` replace an exact command match with
        # argument rows. Use the shortest unambiguous prefix, then choose the
        # exact catalog row, so this phase really accepts the command row.
        query_name = command[:-1] if command in {"login", "provider", "account"} else command
        tui.type_slow("/" + query_name)
        tui.settle(0.2)
        typed_frame = tui.repaint(118, 36)
        if query_name not in typed_frame.text:
            raise RuntimeError(
                f"composer text expected=/{query_name} actual_screen={typed_frame.text!r}"
            )

        # Prefix collisions (`account`/`accounts`) keep both command rows.
        # Navigate to the exact shared row. Exact arg-slot commands instead
        # expose their first argument stage immediately, which is itself the
        # activation contract under test.
        items = [
            item
            for item in catalog
            if item.get("kind") == "built_in" and str(item.get("name", "")).startswith(query_name)
        ]
        indices = [
            index
            for index, item in enumerate(items)
            if item.get("kind") == "built_in" and item.get("name") == command
        ]
        if not indices:
            raise RuntimeError(f"palette exact row expected={command!r} query={query_name!r} actual={items!r}")
        tui.down(indices[0])
        tui.enter()
        tui.settle(0.45)
        after_frames = tui.repaint_both()
        after = durable_snapshot(ctx.profile_dir)
        clean, audit = tui.close()
        wide_text = after_frames[0].text
        narrow_text = after_frames[1].text
        text = wide_text + "\n" + narrow_text
        events, receipts = action_rows(before, after)
        expected_receipt = RECEIPT_METHODS.get(command)
        durable = expected_receipt is not None and any(
            row == f"{expected_receipt}:committed" for row in receipts
        )
        screen = _screen_changed(baseline, after_frames)
        semantic = all(
            any(
                marker in frame.text.lower() and marker not in base.text.lower()
                for marker in SEMANTIC_MARKERS.get(command, ())
            )
            for base, frame in zip(baseline, after_frames)
        )
        surface_wide = _signature(command, baseline[0].text, wide_text, SURFACE_SIGNATURES)
        surface_narrow = _signature(command, baseline[1].text, narrow_text, SURFACE_SIGNATURES)
        surface = surface_wide if surface_wide is not None and surface_narrow is not None else None
        wide_remedy = _remedy(command, baseline[0].text, wide_text)
        narrow_remedy = _remedy(command, baseline[1].text, narrow_text)
        remedy = wide_remedy if wide_remedy is not None and narrow_remedy is not None else None
        refusal_wide = _signature(command, baseline[0].text, wide_text, REFUSAL_SIGNATURES)
        refusal_narrow = _signature(command, baseline[1].text, narrow_text, REFUSAL_SIGNATURES)
        refusal = refusal_wide if refusal_wide is not None and refusal_narrow is not None else None
        next_slot = command in ARG_STAGE_COMMANDS and all(
            f"/{command}" in frame.text
            and ("⇥ tab" in frame.text or "options · tab complete" in frame.text)
            for frame in after_frames
        )
        catalog_owned = any(item.get("name") == command for item in catalog)
        screen_oracle = catalog_owned and (screen and semantic or surface is not None)
        closed = durable or screen_oracle or remedy is not None or refusal is not None or next_slot
        # The forbidden shape is made explicit instead of disappearing into a
        # generic screen comparison: no body/card, no receipt, no next slot,
        # and no actionable refusal means the action was only a footer flash.
        flash_only = (
            not durable
            and not screen_oracle
            and remedy is None
            and refusal is None
            and not next_slot
        )
        return {
            "pass": closed and not flash_only and clean,
            "text": text,
            "line": (
                f"mode={'demo' if demo else 'live'} next_slot={str(next_slot).lower()} "
                f"screen_card_change={str(screen).lower()} semantic={str(semantic).lower()} "
                f"surface={surface!r} refusal={refusal!r} "
                f"rpc_catalog_owned={str(catalog_owned).lower()} durable={str(durable).lower()} "
                f"remedy={remedy!r} flash_only={str(flash_only).lower()} "
                f"action_events={events!r} action_receipts={receipts!r} "
                f"{snapshot_delta(before, after)} sizes=118x36,80x24 {audit}"
            ),
        }
    except Exception:
        tui.close()
        raise


def _argument_stage(ctx, command: str, session_id: str):
    from gate.tui_probe import TuiProcess, action_rows, durable_snapshot, snapshot_delta

    _pin_tui_identity(ctx)
    tui = TuiProcess(ctx, session_id=session_id)
    try:
        baseline = tui.repaint_both()
        baseline_text = baseline[0].text + "\n" + baseline[1].text
        before = durable_snapshot(ctx.profile_dir)
        settings_path = ctx.profile_dir / "tui-settings.json"
        before_settings = settings_path.read_bytes() if settings_path.exists() else None
        if command == "login":
            tui.type_slow("/login")
            tui.enter()  # provider row: anthropic
            tui.settle(0.25)
            provider_frames = tui.repaint_both()
            provider_preserved = all(
                "/login anthropic" in frame.text and "oauth" in frame.text
                for frame in provider_frames
            )
            tui.enter()  # method row: api
            tui.settle(0.4)
            frames = tui.repaint_both()
            text = frames[0].text + "\n" + frames[1].text
            key_card = all(
                "anthropic · API key" in frame.text and "key is masked" in frame.text
                for frame in frames
            )
            composer_lines = sorted(
                {
                    line.strip()
                    for line in text.splitlines()
                    if "/login" in line and "⇥ tab" in line
                }
            )
            stage_ok = provider_preserved and key_card
            detail = (
                f"stage0_provider_preserved={str(provider_preserved).lower()} "
                f"stage1_key_card={str(key_card).lower()} "
                f"actual_composer_lines={composer_lines!r}"
            )
        else:
            targets = {
                "theme": "light",
                "model": "claude-opus-4-8",
                "usage": "qa-second",
                "effort": "xhigh",
                "provider": "qa-second",
                "account": "qa-second-account",
            }
            target = targets[command]
            query = f"/{command} {target}"
            tui.type_slow(query)
            tui.settle(0.2)
            offered_frames = tui.repaint_both()
            offered_text = offered_frames[0].text + "\n" + offered_frames[1].text
            has_options = all(target in frame.text for frame in offered_frames)
            if not all(query in frame.text for frame in offered_frames):
                raise RuntimeError(
                    f"composer text expected={query!r} actual_screen={offered_text!r}"
                )
            tui.enter()
            tui.settle(0.4)
            frames = tui.repaint_both()
            text = frames[0].text + "\n" + frames[1].text
            after = durable_snapshot(ctx.profile_dir)
            events, receipts = action_rows(before, after)
            expected_receipt = RECEIPT_METHODS.get(command)
            committed = expected_receipt is not None and any(
                row == f"{expected_receipt}:committed" for row in receipts
            )
            wide_remedy = _remedy(command, baseline[0].text, frames[0].text)
            narrow_remedy = _remedy(command, baseline[1].text, frames[1].text)
            remedy = (
                wide_remedy
                if wide_remedy is not None and narrow_remedy is not None
                else None
            )
            semantic = all(target in frame.text for frame in frames) and _screen_changed(
                baseline, frames
            )
            account_truth = False
            if command == "account":
                rows = json.loads((ctx.profile_dir / "accounts.json").read_text())
                account_truth = any(
                    row.get("alias") == target and row.get("active") is True for row in rows
                )
            effect = committed or semantic or remedy is not None or account_truth
            stage_ok = has_options and effect
            detail = (
                f"target={target!r} stage_offered={str(has_options).lower()} "
                f"one_action_effect={str(effect).lower()} committed={str(committed).lower()} "
                f"account_truth={str(account_truth).lower()} action_events={events!r} "
                f"action_receipts={receipts!r}"
            )
        after = durable_snapshot(ctx.profile_dir)
        after_settings = settings_path.read_bytes() if settings_path.exists() else None
        local_settings = before_settings != after_settings
        clean, audit = tui.close()
        stage_ok = stage_ok or (command == "theme" and has_options and local_settings)
        return (
            stage_ok and clean,
            f"{detail} local_settings_delta={str(local_settings).lower()} "
            f"{snapshot_delta(before, after)} {audit}",
            text,
        )
    except Exception:
        tui.close()
        raise


def run(ctx) -> list[Evidence]:
    from gate.tui_probe import RpcClient, start_daemon

    # Deterministic local inventory: real production profile/account formats,
    # no secret and no discovery. It gives model/provider/account/effort/fast
    # stages something executable rather than vacuously omitting them.
    (ctx.profile_dir / "providers.json").write_text(
        json.dumps(
            {
                "providers": [
                    {
                        "provider_id": "anthropic",
                        "display_name": "anthropic",
                        "api_family": "anthropic_messages",
                        "base_url": "https://closure.openai.azure.com/openai/v1",
                        "enabled": True,
                        "auth_requirement": "api_key",
                        "configured_models": ["claude-opus-5", "claude-opus-4-8"],
                        "default_model": "claude-opus-5",
                        "provenance": "custom",
                        "trust": "full",
                    },
                    {
                        "provider_id": "qa-second",
                        "display_name": "qa-second",
                        "api_family": "anthropic_messages",
                        "base_url": "https://closure-second.openai.azure.com/openai/v1",
                        "enabled": True,
                        "auth_requirement": "api_key",
                        "configured_models": ["qa-second-model"],
                        "default_model": "qa-second-model",
                        "provenance": "custom",
                        "trust": "full",
                    },
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
                    "alias": "qa-closure-account",
                    "provider": "anthropic",
                    "auth_method": "api_key",
                    "identity": "QA closure fixture",
                    "status": {"status": "ok"},
                    "active": True,
                },
                {
                    "alias": "qa-second-account",
                    "provider": "anthropic",
                    "auth_method": "api_key",
                    "identity": "QA second closure fixture",
                    "status": {"status": "ok"},
                    "active": False,
                },
            ],
            separators=(",", ":"),
        ),
        encoding="utf-8",
    )
    status = start_daemon(ctx)
    rpc = RpcClient(status["daemon"]["socket_path"])
    try:
        items = rpc.command_list("", in_session=True)
        commands = [item["name"] for item in items if item.get("kind") == "built_in"]
        if not commands:
            return [
                Evidence(
                    "catalog_cardinality",
                    FAIL,
                    f"shared catalog expected=nonempty actual={len(commands)} names={commands!r}",
                )
            ]
        session_id, _generation = rpc.create_session(
            ctx.workspace_dir,
            provider="anthropic",
            model="claude-opus-5",
            effort="high",
            fast=False,
        )
        from gate.tui_probe import TuiProcess

        seed = TuiProcess(ctx, session_id=session_id)
        try:
            seed.type_slow("seed palette closure history")
            seed.enter()
            if not seed.wait_for(lambda raw: b"QA_PALETTE_HISTORY_READY" in raw):
                raise RuntimeError(
                    "palette history seed expected=QA_PALETTE_HISTORY_READY actual=absent"
                )
            seed.close()
        finally:
            if not seed.closed:
                seed.close()
        evidence: list[Evidence] = []
        for command in commands:
            try:
                if command in DEMO_COMMANDS:
                    command_session = session_id  # ignored by the demo branch
                elif command in HISTORY_COMMANDS:
                    command_session = session_id
                else:
                    command_session, _command_generation = rpc.create_session(
                        ctx.workspace_dir,
                        provider="anthropic",
                        model="claude-opus-5",
                        effort="high",
                        fast=False,
                    )
                result = _activate(ctx, items, command, command_session)
                ok = result["pass"]
                line = result["line"]
                artefacts: list[str] = []
                if command in ARG_STAGE_COMMANDS:
                    arg_session_id, _arg_generation = rpc.create_session(
                        ctx.workspace_dir,
                        provider="anthropic",
                        model="claude-opus-5",
                        effort="high",
                        fast=False,
                    )
                    stage_ok, stage_line, stage_text = _argument_stage(
                        ctx, command, arg_session_id
                    )
                    ok = ok and stage_ok
                    line += " arg_stage=" + stage_line
                    if not stage_ok:
                        if command == "login":
                            line += f" {LOGIN_DEFECT_NOTE} expected_fail_until=0.0.968"
                        artefacts.append(ctx.write_artefact(f"palette-{command}-arg-stage.txt", stage_text))
                if not ok:
                    artefacts.append(ctx.write_artefact(f"palette-{command}.txt", result["text"]))
                evidence.append(
                    Evidence(
                        command,
                        PASS if ok else FAIL,
                        f"/{command} activation_closure expected=true actual={str(ok).lower()} {line}",
                        artefacts,
                    )
                )
            except Exception as error:
                from gate.tui_probe import daemon_transport_diagnosis

                evidence.append(
                    Evidence(
                        command,
                        FAIL,
                        f"/{command} activation_closure expected=true actual=probe_error "
                        f"type={type(error).__name__} detail={error} "
                        f"{daemon_transport_diagnosis(ctx.profile_dir)}",
                    )
                )
        return evidence
    finally:
        rpc.close()
