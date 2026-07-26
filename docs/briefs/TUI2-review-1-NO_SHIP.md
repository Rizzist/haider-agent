codex
Review target: commit object `286ea13`. Post-freeze edits appeared in `app.rs` and `render.rs` during verification; I made no changes and excluded those edits. Line references below are against the frozen commit.

## Findings

- **P1 — Concurrent canned scripts can corrupt one projection.** `crates/haider-tui/src/app.rs:208`, `app.rs:281`, and `app.rs:434` do not mark sample/autoplay turns active until the delayed first `UserMessage`. Multiple digits, autoplay plus submit, or Esc→launcher→digit during an active turn therefore enqueue several requests. `crates/haider-tui/src/runtime.rs:224` spawns every script independently into the same channel, with no generation, serialization, or cancellation. Their identical item/menu IDs and interleaved terminal states produce duplicate users, merged deltas, premature `turn_active = false`, and stale envelopes after `/reset`.

- **P1 — Valid later turns silently lose response items through ID reuse.** `crates/haider-tui/src/mock.rs:314`, `mock.rs:342`, and `mock.rs:362` derive item IDs solely from prompt byte length; `turn_script` also reuses fixed IDs from `mock.rs:44`. A second same-length prompt or second sample replay collides with `finished_items` at `crates/haider-tui/src/projection.rs:220` and `projection.rs:243`, so starts/completions are discarded and deltas become orphans. IDs need a per-turn namespace.

- **P1 — User text is written into OSC 2 unsanitized.** Session titles originate from typed/envelope text at `crates/haider-tui/src/app.rs:323` and are embedded directly by `crates/haider-tui/src/runtime.rs:43`. A pasted BEL/ESC can terminate OSC 2 and inject another terminal control sequence. Strip all C0/C1 control characters before writing a title.

- **P2 — “Start fresh” and `/clear` do not start a fresh session.** The UI promises this at `crates/haider-tui/src/render.rs:143` and `commands.rs:53`, but `/clear`/`back` only change `screen` at `app.rs:350`; launcher submission retains the old projection and title at `app.rs:321`. Sample attachment likewise appends into the existing projection. The launcher is acting as navigation, not session selection/creation.

- **P2 — Palette navigation and execution disagree.** Down traverses every match at `crates/haider-tui/src/app.rs:248`, while rendering exposes only the first eight at `render.rs:419`; selection can disappear off-screen. Enter at `app.rs:265` executes the raw query rather than the highlighted command—e.g. `/t`, Down, Enter flashes unknown `/t` instead of running `/tree`. The registry at `commands.rs:16` also contains 24 entries, not the stated 23, while implemented `/quit` is absent from both palette and help.

- **P2 — Slash parsing misrepresents invalid input.** At `crates/haider-tui/src/app.rs:343`, `/theme nonsense` cycles the theme as if no argument was supplied. At `app.rs:363`, every unknown command is presented as “UI ready; lands with a later wave,” falsely treating typos as planned stubs. Known stubs and unknown commands need separate handling.

- **P2 — Boot accepts invisible composer input.** `crates/haider-tui/src/app.rs:272` routes ordinary characters and Enter without a Boot guard, although `render.rs:79` renders no composer. Hidden input can start a response script alongside boot and any boot key also spends autoplay.

- **P2 — Short layouts hide the controls that own input.** Launcher content is passed tail-first into the clipping behavior of `centered` at `crates/haider-tui/src/render.rs:54` and `render.rs:211`; at heights 8–12 the composer is entirely below the viewport. Session constraints at `render.rs:214` prioritize header/transcript/todos/palette before the menu/composer. A pinned plan, palette, or blocking menu can therefore reduce the active input region to zero rows. Arithmetic is panic-safe, but the UI becomes unanswerable.

- **P2 — Window-title push/pop is unbalanced and cannot restore the prior title.** Every title update pushes at `crates/haider-tui/src/runtime.rs:53`, while shutdown emits only one pop and then immediately sets the title empty at `runtime.rs:49`. On supporting terminals, the clear erases the popped title and earlier pushes remain stacked. Push once on entry, set without pushing thereafter, and pop once on restore.

- **P3 — Talk-chip alignment is not Unicode-column-aware.** `crates/haider-tui/src/render.rs:390`–`407` uses `chars().count()` for composer width. Wide CJK/emoji and combining characters cause the right-aligned chip to drift or clip; use terminal display width.

The reducer’s Ctrl/help/menu/palette precedence otherwise returns cleanly without double handling; help/palette/session Esc behavior is coherent, and blocking-menu Esc is deliberately swallowed. Theme synchronization and reliable outbox retry placement are sound. Sample rows are confined to the explicit `--demo` command, every rendered roster name includes its honorific, and no roster Debug/log path exists.

Verification: the baseline increase `193 → 198` matches five added tests and `xtask check` reports 198. Formatting and diff checks passed; 71 compiled TUI tests passed. Coverage remains happy-path only: no concurrent requests, repeated turns, multi-match Enter, off-screen selection, short palette/menu layouts, title sanitization/restoration, boot input, or launcher “fresh session” test.

VERDICT: NO_SHIP
hook: Stop
hook: Stop Completed
