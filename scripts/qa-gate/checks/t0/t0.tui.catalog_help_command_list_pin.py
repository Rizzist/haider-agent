"""Pin the terminal's rendered help command list to the daemon RPC catalog."""

from __future__ import annotations

from collections import Counter
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

# Registry #94: status wraps startup+request; one live help probe owns one
# boot/action/repaint/reap envelope; request allowances cover status,
# command.list, and cleanup. Total =
# 30+3*60+25+12+4+2.5+20+2+2 = 277.5s.
from gate.tui_probe import TUI_ACTION, TUI_BOOT, TUI_EXIT, TUI_REPAINT

budget = BudgetSum(
    (
        DAEMON_STARTUP,
        STATUS_REQUEST,
        STATUS_REQUEST,
        TUI_BOOT,
        TUI_ACTION,
        TUI_REPAINT,
        TUI_EXIT,
        STATUS_REQUEST,
        DAEMON_STOP,
        PROCESS_EXIT_GRACE,
        PROCESS_EXIT_GRACE,
    )
)
timed = False


_COMMAND_ROW = re.compile(r"^\s*/([a-z][a-z0-9_-]*)\b")
_JOINED_ALIAS = re.compile(r"\s·\s*/([a-z][a-z0-9_-]*)\b")


def _rendered_help_commands(panel_text: str) -> list[str]:
    """Read command labels from painted help rows, including legacy joined aliases."""

    commands: list[str] = []
    for line in panel_text.splitlines():
        match = _COMMAND_ROW.match(line)
        if match is None:
            continue
        commands.append(match.group(1))
        commands.extend(_JOINED_ALIAS.findall(line))
    return commands


def run(ctx) -> list[Evidence]:
    from gate.tui_probe import RpcClient, TuiProcess, start_daemon

    status = start_daemon(ctx)
    rpc = RpcClient(status["daemon"]["socket_path"])
    tui = None
    try:
        rpc_items = rpc.command_list("", in_session=True)
        invalid_rpc_rows: list[object] = []
        rpc_names: list[str] = []
        for item in rpc_items:
            if item.get("kind") != "built_in":
                continue
            name = item.get("name")
            if isinstance(name, str) and name:
                rpc_names.append(name)
            else:
                invalid_rpc_rows.append(item)

        # The help surface does not repeat its own /help action as a command
        # row. Every other built-in name comes exclusively from command.list.
        expected_names = [name for name in rpc_names if name != "help"]

        tui = TuiProcess(ctx)
        tui.type_slow("/help")
        tui.enter()
        help_opened = tui.wait_for(
            lambda raw: b"esc closes" in tui.probe.plain(raw),
        )

        # Make the PTY tall enough to render the complete overlay in one
        # frame. Parsing starts at the painted overlay heading so uncovered
        # launcher rows above the panel can never masquerade as help rows.
        frame = tui.repaint(180, max(60, len(expected_names) + 12))
        panel_start = next(
            (
                row
                for row in range(1, frame.rows_count + 1)
                if frame.rows.get(row, "").strip().startswith("help  esc closes")
            ),
            None,
        )
        panel_text = (
            "\n".join(
                frame.rows.get(row, "")
                for row in range(panel_start, frame.rows_count + 1)
            )
            if panel_start is not None
            else ""
        )
        help_names = _rendered_help_commands(panel_text)
        help_clean, audit = tui.close()

        missing_help = sorted((Counter(expected_names) - Counter(help_names)).elements())
        extra_help = sorted((Counter(help_names) - Counter(expected_names)).elements())
        rpc_duplicates = sorted(
            name for name, count in Counter(rpc_names).items() if count > 1
        )
        passed = (
            bool(expected_names)
            and not invalid_rpc_rows
            and not rpc_duplicates
            and help_opened
            and panel_start is not None
            and not missing_help
            and not extra_help
            and help_clean
        )
        detail = (
            f"authority=command.list built_in_count={len(rpc_names)} "
            f"rendered_help_count={len(help_names)} self_command_excluded=help "
            f"missing_from_help={missing_help!r} extra_in_help={extra_help!r} "
            f"rpc_duplicates={rpc_duplicates!r} invalid_rpc_rows={invalid_rpc_rows!r} "
            f"help_opened={str(help_opened).lower()} "
            f"panel_captured={str(panel_start is not None).lower()} {audit}"
        )
        if not passed:
            detail += " expected_fail_until=0.0.968"

        evidence = [
            Evidence(
                "rendered_help_equals_command_list",
                PASS if passed else FAIL,
                detail,
            )
        ]
        if not help_clean:
            evidence.append(
                Evidence(
                    "pty_exit",
                    FAIL,
                    f"TUI clean exit expected=true actual=false {audit}",
                )
            )
        return evidence
    finally:
        if tui is not None and not tui.closed:
            tui.close()
        rpc.close()
