# v0.0.15 swallowed-first-key fix — review of record — SHIP

Implementer AND reviewer: Fable 5. Branch `w5-wordmark-fix` @ `f5937b6`.

## The field bug (found by the v0.0.15 install probe, not by any test)

`Wordmark::detect()` ran `Picker::from_query_stdio()` unconditionally after
raw mode. On a terminal that never answers the graphics-capability query —
plain xterm, the probe's pty, most non-graphics emulators — the query's
stdio reader consumed the NEXT stdin byte. The user's first keystroke after
launch silently vanished: the probe's leading `/` died, so `/accounts`
became a demo SESSION named "accounts" (`❯ accounts · ◌ thinking…`).

Diagnosis chain, for the record: probe FAIL on /accounts but PASS on
/providers → headless key-path repro PASSED (the tree was right; the binary
wasn't) → single-key pty probe showed `/` producing zero cell diffs while
`t` rendered → sacrificial-byte probe opened the screen perfectly → exactly
one byte eaten, always the first.

## The fix

`graphics_terminal_likely()` — a pure function over an env lookup — gates
the query on evidence of a terminal that ANSWERS: kitty/ghostty TERM,
KITTY_WINDOW_ID, TERM_PROGRAM ∈ {iTerm.app, WezTerm, ghostty, rio, vscode},
WEZTERM_EXECUTABLE, KONSOLE_VERSION. Everything else skips straight to the
half-block art without touching stdin.

Trade-off, stated: an unlisted graphics-capable terminal now gets the
half-block mark instead of the PNG. Correct side of the trade — a less
pretty mark on exotic terminals versus every plain-terminal user losing
their first keystroke.

## Mutation check (executed post-commit)

Collapse the gate to `true` (the v0.0.15 behavior) →
`plain_terminals_never_query_graphics_capability` FAILED at runtime. KILLED.

## Gate

tui 481 green; clippy clean; ledger 1045 → 1047. daemond flaked once under
full-gate contention (76+1) — 5 consecutive clean runs isolated AND under
concurrent tui load; the flake predates this branch, which touches only
haider-tui. gate.sh now persists per-crate logs so the next occurrence
names its test. Tracked, non-blocking.

## Verdict

**SHIP** into v0.0.16 immediately — v0.0.15's binary eats the first
keystroke on non-graphics terminals, which is a first-run-experience bug of
the highest visibility.
