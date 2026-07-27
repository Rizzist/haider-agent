# TUI5 review round 1 — SHIP_WITH_FIXES

Reviewer: gpt-5.6 (codex, xhigh), frozen 00d7dd0, scope 77dbe3e..00d7dd0. Independent of the implementer's internal verification pass; probed the internally-patched areas specifically and found the sibling class.

Required fixes (TUI5.1): (1) normalize cursor/anchor to grapheme boundaries after every cluster-changing edit; (2) grapheme-based tail-windowing; (3) bind mouse hits/drags to surface + text revision, cancel on transitions; (4) scope selection keys to a mounted composer (composer_owns_input()); (5) Shift-Up/Down edge extension; (6) a regression per reproduction. P3-6: record the corrective test rewrite in the release record (done here).

## 1. FINDINGS

### P1

1. Grapheme-boundary invariants fail after edits. [composer.rs:187](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/composer.rs:187), [composer.rs:196](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/composer.rs:196), [render.rs:2622](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/render.rs:2622)

   Inserting `ZWJ` between `👩👩` produces one grapheme, but leaves cursor byte `7` inside it. Deleting `x` from `🇦x🇧` similarly joins the flags while retaining an interior cursor. Rendering only styles grapheme starts, so the cursor cell disappears and subsequent editing can split the cluster. Direct reproduction failed on the frozen code.

2. Stale composer hits can mutate a fresh draft. [app.rs:1414](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/app.rs:1414), [runtime.rs:466](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/runtime.rs:466)

   The guard only rejects `start > current_len`; it does not validate content, surface, or revision. Against `"fresh text"`, a stale hit carrying `"stale text"` moved the cursor from `10` to `3` and armed a drag. Fresh typing then inserts at the unintended location. A held drag can also survive a surface transition and act on another draft. This is a direct sibling of the internal phantom-anchor P1.

### P2

3. Hidden composer selections still preempt key meanings after the composer is replaced. [app.rs:1327](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/app.rs:1327), [app.rs:1390](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/app.rs:1390), [app.rs:1746](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/app.rs:1746)

   After an inbound menu replaces a selected composer, first-press Ctrl-C copies the hidden selection instead of navigating. A direct regression probe expected Launcher but remained in Session. Subagent-question Esc can likewise clear an invisible selection instead of returning to Session. Gate selection keys on `composer_owns_input()`.

4. Shift-Up/Shift-Down are no-ops at outer row edges. [composer.rs:365](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/composer.rs:365), [composer.rs:394](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/composer.rs:394)

   On one-line `abc`, Shift-Up from the end and Shift-Down from the start return before creating an anchor. Item 4 explicitly requires both gestures; the direct probe produced no selection.

5. Non-cursor tail windows split graphemes. [render.rs:2307](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/render.rs:2307)

   `tail_window` walks `chars()` instead of graphemes. A clipped combining sequence can render as `…◌́x`, dropping the base while retaining its mark; ZWJ emoji can split similarly.

### P3

6. The “zero test deletions” claim is not literally true. [tui5_cursor_tests.rs:497](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/tests/tui5_cursor_tests.rs:497)

   `00d7dd0` deletes `ctrl_c_with_transcript_selection_recopies_the_frame` and replaces it with the correct opposite-law regression. Counts remain monotonic and no baseline test disappeared, so this is a documented corrective rewrite—not weakening—but the release record should say so.

Verified good: composer rendering has no `▮` path; dawn/ivory/dark, placeholder, launcher, session, Aura, arg-slot and tail-window cursor coverage passes. The remaining `▮` paths are streaming transcript cursors only.

## 2. KEYMAP/GATE ARBITRATION TABLE

| Key/context | Verified behavior |
|---|---|
| Ctrl-C + visible composer selection | Copy exact text, clear selection, stay on surface |
| Ctrl-C + transcript highlight only | Clear highlight and navigate/quit on first press |
| Ctrl-C without selection, non-launcher | Return to launcher |
| Ctrl-C on launcher/boot | Quit |
| Ctrl-C + hidden replaced-composer selection | **Bug:** copies and consumes instead of navigating |
| Esc + visible composer selection | Deselect only; next Esc gets normal meaning |
| Esc + transcript highlight only | Normal meaning fires immediately |
| Esc, running session | Interrupt and remain in Session |
| Esc, idle session | Detach to launcher |
| Esc, Aura/Subagent | Exit Aura / return to Session |
| Esc, palette/help/menu | Dismiss palette/help/nonblocking menu; blocking menu swallows |
| Esc + hidden replaced-composer selection | **Bug:** hidden selection can consume the first press |
| Up/Down, blocking or chip menu | Menu owns arrows |
| Up/Down, slash palette | Palette owns arrows |
| Up/Down, multiline composer | Sticky-column row movement |
| Up/Down at first/last row, no selection/Shift | History previous/next; recall cursor at end |
| Plain Up/Down with selection | Collapse to selection start/end first |
| Shift-Up/Down at outer edge | **Bug:** no-op instead of extending to buffer edge |

## 3. MUTATION-CHECK AUDIT

| Mutation | Result |
|---|---|
| Reintroduced literal composer `▮` and removed reverse-video caret styling | 5 tests failed: theme cursor, mid-text cursor, long-line caret, Aura/arg-slot, distinct selection endpoint |
| Removed `selection_key` dispatch | 2 tests failed: Ctrl-C selection gate and Esc deselect precedence; also produced dead-code warning. The claimed cardinality of 3 was not reproduced |
| Added serialized DTO field named `cursor` | DTO sweep failed exactly with `persisted key "cursor" leaks composer state` |
| Own mutation: collapsed every session draft onto `DraftKey::Session(0)` | `drafts_travel_per_surface_with_cursor_and_selection` failed when the second session inherited the first session’s draft |

All mutations were restored. Original SHA-256 hashes matched afterward; final diff, status, and stash list are empty.

## 4. PROBE RESULTS

Hostile caller environment: `NO_COLOR=1 CLICOLOR=0`. Fresh `target/release/haider`.

| Run | Size | Result |
|---|---:|---|
| `pty-probe` | 118×36 | PASS |
| `pty-probe` | 90×10 | PASS |
| `pty-probe` | 90×7 | PASS |
| `pty-probe` | 90×5 | PASS |
| `pty-probe` | 90×1 | PASS |
| `pty-probe-ml` | 118×36 | PASS |
| `pty-probe-ml` | 90×10 | PASS |
| `pty-probe-sub` | 118×36 | PASS |
| `pty-probe-sub` | 90×10 | PASS, documented compact-size checks skipped |
| `pty-probe-persist` | 118×36 | PASS |
| `pty-probe-anim` | 118×36 | PASS |
| `pty-probe-anim` | 90×10 | PASS, documented shed-row animation checks skipped |
| `pty-probe-cursor` | 118×36 | PASS |
| `pty-probe-cursor` | 90×10 | PASS |

Enforceability: cursor probe against `/usr/bin/false` exited `1`.

Additional gates:

- `cargo test --workspace`: PASS
- Clippy `--all-targets -- -D warnings`: PASS
- `cargo fmt --check`: PASS
- Test-count: `581 → 598 → 606 → 614 → 619`; `619/619`: PASS
- Final TUI5 suite after restoration: `38/38`: PASS
- P3-1 `u64::MAX`, card sequence, and P3-3 duplicate-ID rejection are implemented and tested.
- Scratch-0, anim-probe SKIP-at-gate, and the 300 ms clipboard poll remain ledgered.
- Against `main` `e6eb4a0`, the branch is a fast-forward: zero conflicts. Thus no unexpected conflict exists, though the anticipated `test-baseline.txt` conflict does not materialize in the actual topology.

Required before merge: normalize cursor/anchor boundaries after every cluster-changing edit; make tail-windowing grapheme-based; bind mouse hits/drags to surface plus text revision and cancel them on transitions; scope selection gates to a mounted composer; implement Shift-Up/Down edge extension; add regressions for each reproduction.

VERDICT: SHIP_WITH_FIXES
