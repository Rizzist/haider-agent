"""Exercise login palette paths and the masked-key boundary in a real PTY."""

from __future__ import annotations

from gate import (
    DAEMON_STARTUP,
    DAEMON_STOP,
    FAIL,
    PASS,
    PROCESS_EXIT_GRACE,
    STATUS_REQUEST,
    Evidence,
)
from gate.tui_probe import TUI_ACTION, TUI_BOOT, TUI_EXIT, TUI_REPAINT

id = "t0.tui.login_paths"
tier = "t0"
area = "tui"
needs = ("binary", "daemon", "pty", "network:none")
script = [{"step": "finish", "reason": "end_turn"}]
turns_expected = 0

# Registry #94: startup+status, five independent PTY paths, then the runner's
# status+stop+two process-exit observations. Each path owns boot/action/two
# repaints/reap; no timing result is published on this loaded host.
budget = (
    DAEMON_STARTUP
    + STATUS_REQUEST
    + TUI_BOOT + TUI_ACTION + TUI_REPAINT + TUI_EXIT
    + TUI_BOOT + TUI_ACTION + TUI_REPAINT + TUI_EXIT
    + TUI_BOOT + TUI_ACTION + TUI_REPAINT + TUI_EXIT
    + TUI_BOOT + TUI_ACTION + TUI_REPAINT + TUI_EXIT
    + TUI_BOOT + TUI_ACTION + TUI_REPAINT + TUI_EXIT
    + STATUS_REQUEST
    + DAEMON_STOP
    + PROCESS_EXIT_GRACE
    + PROCESS_EXIT_GRACE
)
timed = False

SENTINEL = "QA_GATE_KEY_SENTINEL_968_NEVER_RENDER"


def _exercise(ctx, command: str, *, trailing_space: bool = False):
    from gate.tui_probe import TuiProcess, durable_snapshot, snapshot_delta

    tui = TuiProcess(ctx)
    try:
        before = durable_snapshot(ctx.profile_dir)
        tui.type(command + (" " if trailing_space else ""))
        tui.enter()
        tui.settle(0.5)
        wide, narrow = tui.repaint_both()
        after = durable_snapshot(ctx.profile_dir)
        clean, audit = tui.close()
        return tui, wide, narrow, snapshot_delta(before, after), clean, audit
    except Exception:
        tui.close()
        raise


def run(ctx) -> list[Evidence]:
    from gate.tui_probe import TuiProcess, durable_snapshot, scan_tree_for_bytes, snapshot_delta, start_daemon

    start_daemon(ctx)
    evidence: list[Evidence] = []

    _tui, wide, narrow, delta, clean, audit = _exercise(
        ctx, "/login anthropic api", trailing_space=True
    )
    key_card = all(
        "anthropic · API key" in frame.text and "key is masked" in frame.text
        for frame in (wide, narrow)
    )
    evidence.append(
        Evidence(
            "anthropic_api_key_card",
            PASS if key_card and clean else FAIL,
            f"/login anthropic api key_card expected=true actual={str(key_card).lower()} "
            f"sizes=118x36,80x24 {delta} {audit}",
        )
    )

    _tui, wide, narrow, delta, clean, audit = _exercise(ctx, "/login anthropic")
    method_choice = all(
        "/login anthropic" in frame.text
        and "paste an API key" in frame.text
        and "browser sign-in" in frame.text
        for frame in (wide, narrow)
    )
    evidence.append(
        Evidence(
            "anthropic_method_choice",
            PASS if method_choice and clean else FAIL,
            f"/login anthropic method_choice expected=true actual={str(method_choice).lower()} "
            f"sizes=118x36,80x24 {delta} {audit}",
        )
    )

    _tui, wide, narrow, delta, clean, audit = _exercise(
        ctx, "/login kimi api", trailing_space=True
    )
    oauth_remedy = all(
        "no API-key flow" in frame.text and "/login kimi oauth" in frame.text
        for frame in (wide, narrow)
    )
    evidence.append(
        Evidence(
            "kimi_oauth_only_refusal",
            PASS if oauth_remedy and clean else FAIL,
            f"/login kimi api refusal_remedy expected='/login kimi oauth' actual={str(oauth_remedy).lower()} "
            f"sizes=118x36,80x24 {delta} {audit}",
        )
    )

    _tui, wide, narrow, delta, clean, audit = _exercise(
        ctx, "/login custom api", trailing_space=True
    )
    custom_fields = all(
        all(token in frame.text for token in ("add custom server", "alias", "base URL", "key"))
        for frame in (wide, narrow)
    )
    evidence.append(
        Evidence(
            "custom_fields",
            PASS if custom_fields and clean else FAIL,
            f"/login custom reaches name/base-URL/key expected=true actual={str(custom_fields).lower()} "
            f"sizes=118x36,80x24 {delta} {audit}",
        )
    )

    masked = TuiProcess(ctx)
    try:
        before = durable_snapshot(ctx.profile_dir)
        masked.type("/login anthropic api ")
        masked.enter()
        masked.settle(0.35)
        masked.tab()  # alias -> masked key field
        masked.paste(SENTINEL)
        masked.enter()  # submit the actual account-login action
        masked.settle(0.7)
        wide, narrow = masked.repaint_both()
        after = durable_snapshot(ctx.profile_dir)
        clean, audit = masked.close()
        frame_leaks = SENTINEL.encode() in masked.sink[0] or SENTINEL in wide.text or SENTINEL in narrow.text
        journal_matches = [
            match
            for match in scan_tree_for_bytes(ctx.profile_dir, SENTINEL.encode())
            if __import__("pathlib").Path(match).name
            in {"store.sqlite", "store.sqlite-wal", "store.sqlite-shm"}
        ]
        leaked = frame_leaks or bool(journal_matches)
        evidence.append(
            Evidence(
                "masked_key_non_disclosure",
                PASS if not leaked and clean else FAIL,
                f"sentinel submitted=true frame_flash_hits={int(frame_leaks)} "
                f"journal_store_hits={journal_matches!r} "
                f"masked_expected=true actual={str(not leaked).lower()} {snapshot_delta(before, after)} {audit}",
            )
        )
    finally:
        if not masked.closed:
            masked.close()
    return evidence
