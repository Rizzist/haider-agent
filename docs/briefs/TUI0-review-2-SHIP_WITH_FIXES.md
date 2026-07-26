codex
No P1 hot-loop or transactional-restoration blocker remains, but four P2 defects leave the r1 fixes incomplete.

## Findings

- **P2 — Plan `Started` events bypass lifecycle idempotency.** The duplicate check searches `finished_items` and streaming transcript blocks at [projection.rs:211](/Users/rizzist/Documents/CODING/haider-agent-tui0/crates/haider-tui/src/projection.rs:211), but active plans are stored only in `todos` at [projection.rs:218](/Users/rizzist/Documents/CODING/haider-agent-tui0/crates/haider-tui/src/projection.rs:218). A double plan `Started` is therefore not counted, and a stale `Started` after a partial `Completed` overwrites the progressed plan. Partial completions themselves correctly remain open until all-done. The new test covers message starts and completed-plan duplication, but not active-plan starts ([projection_tests.rs:579](/Users/rizzist/Documents/CODING/haider-agent-tui0/crates/haider-tui/tests/projection_tests.rs:579)).

- **P2 — A full answer-echo channel silently loses the selected menu answer.** [runtime.rs:163](/Users/rizzist/Documents/CODING/haider-agent-tui0/crates/haider-tui/src/runtime.rs:163) drains the outbox and discards `try_send` failures. On `Full`, the answer is removed permanently without backpressure or retry, and the menu remains open until another answer arrives. This is a new regression in the r1-confirmed bounded-channel/backpressure property. It does not spin, but it is not a reliable outbox drain.

- **P2 — Bracketed paste still mutates the composer behind a blocking menu.** Key events respect the replacement rule at [app.rs:132](/Users/rizzist/Documents/CODING/haider-agent-tui0/crates/haider-tui/src/app.rs:132), but `AppEvent::Paste` always appends to `composer` at [app.rs:107](/Users/rizzist/Documents/CODING/haider-agent-tui0/crates/haider-tui/src/app.rs:107). Pasting while the menu is visible produces hidden composer text that appears after the menu closes. The menu test covers ordinary character keys only.

- **P2 — Long command output scrolls both honesty notices out of the only visible viewport.** Truncation and decode warnings are inserted before the retained output at [render.rs:320](/Users/rizzist/Documents/CODING/haider-agent-tui0/crates/haider-tui/src/render.rs:320), while the transcript always scrolls to the bottom at [render.rs:136](/Users/rizzist/Documents/CODING/haider-agent-tui0/crates/haider-tui/src/render.rs:136) and has no manual scroll state. A multiline 8 KiB tail therefore leaves only output visible, with no indication that earlier bytes were discarded or undecodable. The new TUI test uses a decode error with no following output, so it cannot catch this; the plain renderer is sound.

- **P3 — `TerminalGuard` still permits panic-hook stacking and its active invariant is externally bypassable.** Every successful `enter` installs another chaining hook at [runtime.rs:57](/Users/rizzist/Documents/CODING/haider-agent-tui0/crates/haider-tui/src/runtime.rs:57), while `Drop` merely clears `GUARD_ACTIVE` at [runtime.rs:66](/Users/rizzist/Documents/CODING/haider-agent-tui0/crates/haider-tui/src/runtime.rs:66). Sequential enter/drop cycles therefore stack hooks. Additionally, the public unit constructor at [runtime.rs:30](/Users/rizzist/Documents/CODING/haider-agent-tui0/crates/haider-tui/src/runtime.rs:30) lets callers construct and drop an unentered guard, restoring the terminal and clearing the flag beneath a live guard.

Traced as sound:

- `run_demo` no longer spins: closed input exits; a closed envelope receiver is fused after one `None`; a clean model disables ticks; an open menu at stream end leaves only pending/disabled branches. Full answer echo causes the loss above, not a hot loop.
- All returned I/O failure paths in `TerminalGuard::enter` roll back raw/alternate-screen state and clear `GUARD_ACTIVE`; normal `Drop` also restores and clears.
- Menu submission rechecks the current menu without an interleaving await, and every `MenuOpened` resets selection to zero, safely handling smaller option sets.
- `apply_raw` advances sequence state while skipping all display mutation for `ui=false`.
- Repeated partial plan completions update correctly; only all-done closes the ID.
- Narrow-meter policy, badge tones, documented token rounding, plain BrokenPipe handling, and both plain-renderer honesty notices are sound.

Verification: 41 relevant frozen test executables passed; `xtask check` reports 139/139 tests, formatting and diff checks passed, and the worktree remained clean. The CLI integration binary could not create its sandboxed temporary profile, so its two TUI cases were not counted; a direct closed-pipe binary run exited successfully.

VERDICT: SHIP_WITH_FIXES
hook: Stop
hook: Stop Completed
tokens used
164,956
No P1 hot-loop or transactional-restoration blocker remains, but four P2 defects leave the r1 fixes incomplete.

## Findings

- **P2 — Plan `Started` events bypass lifecycle idempotency.** The duplicate check searches `finished_items` and streaming transcript blocks at [projection.rs:211](/Users/rizzist/Documents/CODING/haider-agent-tui0/crates/haider-tui/src/projection.rs:211), but active plans are stored only in `todos` at [projection.rs:218](/Users/rizzist/Documents/CODING/haider-agent-tui0/crates/haider-tui/src/projection.rs:218). A double plan `Started` is therefore not counted, and a stale `Started` after a partial `Completed` overwrites the progressed plan. Partial completions themselves correctly remain open until all-done. The new test covers message starts and completed-plan duplication, but not active-plan starts ([projection_tests.rs:579](/Users/rizzist/Documents/CODING/haider-agent-tui0/crates/haider-tui/tests/projection_tests.rs:579)).

- **P2 — A full answer-echo channel silently loses the selected menu answer.** [runtime.rs:163](/Users/rizzist/Documents/CODING/haider-agent-tui0/crates/haider-tui/src/runtime.rs:163) drains the outbox and discards `try_send` failures. On `Full`, the answer is removed permanently without backpressure or retry, and the menu remains open until another answer arrives. This is a new regression in the r1-confirmed bounded-channel/backpressure property. It does not spin, but it is not a reliable outbox drain.

- **P2 — Bracketed paste still mutates the composer behind a blocking menu.** Key events respect the replacement rule at [app.rs:132](/Users/rizzist/Documents/CODING/haider-agent-tui0/crates/haider-tui/src/app.rs:132), but `AppEvent::Paste` always appends to `composer` at [app.rs:107](/Users/rizzist/Documents/CODING/haider-agent-tui0/crates/haider-tui/src/app.rs:107). Pasting while the menu is visible produces hidden composer text that appears after the menu closes. The menu test covers ordinary character keys only.
