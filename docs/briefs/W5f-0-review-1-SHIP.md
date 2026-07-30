# W5f-0 — review of record #1 — SHIP

Implementer AND reviewer: Fable 5. Branch `w5-f0` @ `4706e78` (+ `e028d00`
fmt). Authority: the owner's three-screenshot bug report (2026-07-30).

## Bug 1 — the browser that never opened

The OAuth card said "your browser opened auth.openai.com". The model layer
did everything right: the driver received the authorize URL, moved the card
to `WaitingBrowser`, and pushed `AppRequest::OpenUrl`. `run_live`'s shell
executor then swallowed it in a `_ => {}` catch-all. `[1] open the link
again` re-pushed into the same void.

The fix is structural, not vigilant: the shell channel is now the CLOSED
`ShellRequest` enum (CopySelection / CopyText / OpenUrl / Quit), so the
executor's match has no catch-all to hide in — an unhandled effect is a
compile error. The hop itself:

- `browser.rs` — `$BROWSER` wins (POSIX convention, and the probe seam);
  platform defaults `/usr/bin/open` / `xdg-open` / `cmd /C start`;
  schemes allow-listed to http(s) BEFORE anything spawns; detached, stdio
  null.
- Failure is honest: the URL lands on the clipboard (pbcopy + OSC 52) and
  the flash says so. Success stays quiet — the card already narrates.
- Demo keeps its `· browser (demo): …` flash; nothing real spawns there.

## Bug 2 — the corpse dressed as a runner

`SessionState::busy()` treated every non-"IDLE" badge as busy. `✗ ERRORED`
is TERMINAL — but the demo script only ever holds ERRORED for 1.8s
(`ERRORED_HOLD_MS`), so no golden ever saw the permanent state a live
errored turn produces: gold pulsing dot, `running… ·`, counted in the
header's `N running`. Forever.

Errored is now the row's third honest state: `busy()` carves out
`run_errored()`, the launcher paints `✗` (warn, still — nothing pulses for
a corpse) with `errored ·`, `/sessions` says `errored`, and the running
counter excludes it. Live chips or a new turn outrank the corpse state by
construction (`errored()` requires no live chips AND no active turn).

## Bug 3 — ci red on main

`cargo fmt --check` drift in two W5e-3 test files. `e028d00`.

## Mutations (executed post-commit, runtime kills)

| # | Mutation | Result |
|---|---|---|
| M1 | `live_pass` drops the OpenUrl arm (falls to the driver, which discards it as runtime-owned — the shipped bug one seam earlier) | KILLED — 1 test |
| M2 | `open_url_effects` ignores the opener's error | KILLED — 1 test |
| M3 | `busy()` reverts to the plain badge comparison | KILLED — 3 tests |
| M4 | render drops both errored arms | KILLED — 1 test (+ unused-var warning) |

## Gate

haider-tui suite green (497 → 503). clippy `-D warnings` clean. Ledger
1078 → 1084. Full per-crate gate: `gate12.out`.

## Verdict

**SHIP.** The residual honesty note: `run_live`'s OpenUrl arm body calling
`open_url_effects` is itself unpinned (the arm is compile-enforced, the
call inside it is not) — the installed-binary probe with `$BROWSER` pointed
at a recorder covers that last inch at W5f ship time.
