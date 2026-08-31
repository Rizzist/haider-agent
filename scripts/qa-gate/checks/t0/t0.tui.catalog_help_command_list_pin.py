"""Pin RPC catalog, shared source catalog, and the terminal help command list."""

from __future__ import annotations

import re

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

id = "t0.tui.catalog_help_command_list_pin"
tier = "t0"
area = "tui"
needs = ("binary", "daemon", "pty", "network:none")
script = [{"step": "finish", "reason": "end_turn"}]
turns_expected = 0
expected_fail_until = "0.0.968"
DEFECT_NOTE = (
    "defect: docs/client-contract-v1.md:70-74 forbids mirroring slash-command names; "
    "crates/haider-tui/src/commands.rs:12-14 is a static HELP_TEXT mirror"
)

# Registry #94: status wraps startup+request; help plus the nine intentional
# out-of-catalog spellings each own one boot/action/three-repaint/reap envelope;
# three request allowances cover status, command.list, and cleanup. Total =
# 10*(25+12+3*4+2.5)+30+3*60+20+2+2 = 749s.
from gate.tui_probe import TUI_ACTION, TUI_BOOT, TUI_EXIT, TUI_REPAINT

budget = BudgetSum(
    (
        DAEMON_STARTUP,
        STATUS_REQUEST,
        STATUS_REQUEST,
        *((TUI_BOOT, TUI_ACTION, TUI_REPAINT, TUI_REPAINT, TUI_REPAINT, TUI_EXIT) * 10),
        STATUS_REQUEST,
        DAEMON_STOP,
        PROCESS_EXIT_GRACE,
        PROCESS_EXIT_GRACE,
    )
)
timed = False


def _source_catalog(text: str) -> list[str]:
    body = text.split("pub const COMMANDS", 1)[1].split("\n];", 1)[0]
    return re.findall(
        r"(?:client_cmd|operation_cmd|session_operation_cmd|session_client_cmd)\(\s*\"([^\"]+)\"",
        body,
    )


def _help_commands(text: str) -> list[str]:
    body = text.split("pub const HELP_TEXT", 1)[1].split("\n];", 1)[0]
    commands: list[str] = []
    for literal in re.findall(r'^\s*"(.*)",$', body, re.MULTILINE):
        match = re.match(r"\s*/([a-z]+)(?:\s*·\s*/([a-z]+))?", literal)
        if match:
            commands.append(match.group(1))
            if match.group(2):
                commands.append(match.group(2))
    return commands


def run(ctx) -> list[Evidence]:
    from gate.tui_probe import RpcClient, TuiProcess, changed_body, durable_snapshot, start_daemon

    status = start_daemon(ctx)
    rpc = RpcClient(status["daemon"]["socket_path"])
    tui = None
    try:
        rpc_items = rpc.command_list("", in_session=True)
        rpc_names = [item.get("name") for item in rpc_items if item.get("kind") == "built_in"]
        # The throwaway cwd has no source tree; use the repository root owned by
        # the loaded helper instead of ever consulting an installed profile.
        repo = __import__("pathlib").Path(__file__).resolve().parents[4]
        source_names = _source_catalog((repo / "crates/haider-rpc/src/command.rs").read_text())
        help_names = _help_commands((repo / "crates/haider-tui/src/commands.rs").read_text())

        tui = TuiProcess(ctx)
        tui.type("/help")
        tui.enter()
        tui.settle()
        wide, narrow = tui.repaint_both()
        help_clean, audit = tui.close()

        evidence: list[Evidence] = []
        if rpc_names == source_names and source_names:
            evidence.append(
                Evidence(
                    "rpc_equals_commands",
                    PASS,
                    f"command.list=COMMANDS count={len(source_names)} order_equal=true",
                )
            )
        else:
            evidence.append(
                Evidence(
                    "rpc_equals_commands",
                    FAIL,
                    f"command.list=COMMANDS expected=true actual=false rpc={rpc_names!r} source={source_names!r}",
                )
            )

        normalized_catalog = [name for name in source_names if name != "help"]
        missing_help = sorted(set(normalized_catalog) - set(help_names))
        extra_help = sorted(set(help_names) - set(normalized_catalog))
        visible = wide.text + "\n" + narrow.text
        help_painted = (
            "commands" in wide.text.lower()
            and "/model" in wide.text
            and "commands" in narrow.text.lower()
            and "/model" in narrow.text
        )
        if missing_help == [] and extra_help == [] and help_painted and help_clean:
            evidence.append(
                Evidence(
                    "help_equals_catalog",
                    PASS,
                    f"HELP_TEXT=COMMANDS-minus-self count={len(help_names)} painted_118x36_and_80x24=true {audit}",
                )
            )
        else:
            evidence.append(
                Evidence(
                    "help_equals_catalog",
                    FAIL,
                    "HELP_TEXT=COMMANDS-minus-self expected=true actual=false "
                    f"missing_from_help={missing_help!r} absent_from_COMMANDS={extra_help!r} "
                    f"painted={str(help_painted).lower()} expected_fail_until=0.0.968 "
                    f"{DEFECT_NOTE} {audit}",
                )
            )

        forms = {
            "notifications": "hidden preference",
            "notify": "notifications alias",
            "quit": "control command",
            "exit": "quit alias",
            "peers": "peer alias",
            "monitors": "flagged dispatcher missing from COMMANDS",
            "resume": "sessions alias",
            "retry": "failure-context action",
        }
        dispatch_source = (repo / "crates/haider-tui/src/app.rs").read_text()
        details: list[str] = []
        broken: list[str] = []
        for form, intent in forms.items():
            probe = TuiProcess(ctx)
            try:
                before_frames = probe.repaint_both()
                before_settings_path = ctx.profile_dir / "tui-settings.json"
                before_settings = (
                    before_settings_path.read_bytes() if before_settings_path.exists() else None
                )
                before_durable = durable_snapshot(ctx.profile_dir)
                probe.type_slow(f"/{form} ")
                typed_frames = probe.repaint_both()
                composer_exact = all(f"/{form}" in frame.text for frame in typed_frames)
                probe.enter()
                probe.settle(0.35)
                if form in {"quit", "exit"}:
                    clean, audit_form = probe.close()
                    text = probe.probe.plain(probe.sink[0]).decode("utf-8", "replace")
                    visible_effect = clean
                else:
                    after_frames = probe.repaint_both()
                    text = after_frames[0].text + "\n" + after_frames[1].text
                    body_wide = bool(changed_body(before_frames[0], after_frames[0]))
                    body_narrow = bool(changed_body(before_frames[1], after_frames[1]))
                    after_settings = (
                        before_settings_path.read_bytes() if before_settings_path.exists() else None
                    )
                    settings_delta = before_settings != after_settings
                    durable_delta = before_durable != durable_snapshot(ctx.profile_dir)
                    visible_effect = body_wide and body_narrow or settings_delta or durable_delta
                    clean, audit_form = probe.close()
                unknown = f"unknown command /{form}" in text.lower()
                source_dispatch = any(
                    f'"{form}"' in line and "=>" in line for line in dispatch_source.splitlines()
                )
                conditional = form == "retry" and any(
                    word in text.lower() for word in ("retry", "failed", "nothing")
                )
                requires_effect = form in {"notifications", "notify", "quit", "exit", "resume"}
                ok = (
                    clean
                    and composer_exact
                    and not unknown
                    and source_dispatch
                    and (visible_effect or conditional or not requires_effect)
                )
                if not ok:
                    broken.append(form)
                details.append(
                    f"{form}:{intent}:composer={composer_exact},dispatch={source_dispatch},effect={visible_effect},"
                    f"unknown={unknown},clean={clean},{audit_form}"
                )
            except Exception as error:
                probe.close()
                broken.append(form)
                details.append(f"{form}:{intent}:probe_error={type(error).__name__}:{error}")
        evidence.append(
            Evidence(
                "out_of_catalog_forms",
                PASS if not broken else FAIL,
                f"driven_real_tui=true broken={broken!r} actual={details!r}",
            )
        )
        if not help_clean:
            evidence.append(Evidence("pty_exit", FAIL, f"TUI clean exit expected=true actual=false {audit}"))
        return evidence
    finally:
        if tui is not None and not tui.closed:
            tui.close()
        rpc.close()
