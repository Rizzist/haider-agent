# TUI4 arc dual review round 1 — NO_SHIP

Frozen HEAD 6dd0a34, scope f8d23f3..6dd0a34 (TUI4a/a.1/b/c/13b/d — the 14-item owner wave).

- **gpt-5.6 correctness (mutation rights, haider-tui2): NO_SHIP** — 2 P1 (permissive hydration replacing seeds; /reset identity reuse vs surviving auto-title arm, both reproduced over a real PTY), 2 P2 (probes not enforceable gates + NO_COLOR sensitivity; W3c seam wider than one source), 1 P3 (clipboard success before pbcopy exit). Animation adversarial audit CLEAN — all sim pulse sites registered, exact badge set, no eternal wakeups. Keymap + teardown audit clean except the P1 arm. Tests monotonic 477→523, no deletions, static gates green.
- **Fable design (read-only worktree): FIX_ITEMS, zero D1** — D2-1 session_epoch is a hand-maintained twin of active_session (same root as gpt P1-2), D2-2 roster counter optional at DemoDriver constructor (split-brain in 4 test files), D2-3 three falsified ledger rows unretired, D2-4 TUI3's tests/common MUST fired unpaid (helpers copied across 6-14 files), D2-5 W3c touch-list unledgered. D3: lockstep phasing undocumented, serialize-per-frame hash-skip trade unledgered, hydrate accepts id 0 (scratch-sentinel collision), StopScripts over-broad cancel, animated() allocation note.

Fix round TUI4.1 addresses gpt P1-1/P1-2/P2-3/P3-5 + Fable D2-1..D2-5 + D3-3; P2-4/D2-5 close as a ledger row; remaining D3s ledgered.

---

## gpt-5.6 correctness report

Release gate is blocked. Frozen HEAD verified as `6dd0a346532f6f964cdb6fccb76c1a585a72d09e`; final worktree, diff, and stash list are clean.

## 1. Item parity table

| # | Item | Simulator evidence | Port evidence | Result |
|---:|---|---|---|---|
| 1 | Seeded attach; no launcher autoplay | Seeded sessions and attach-only `openSession`: [tui.js:629](</Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:629>), [tui.js:1606](</Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:1606>) | Seeds materialized; attach starts nothing: [mock.rs:477](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/mock.rs:477), [app.rs:2158](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/app.rs:2158), [runtime.rs:249](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/runtime.rs:249) | MATCHES |
| 2 | Composer band closed | Input background and border: [tui.js:5395](</Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:5395>) | Grounded composer, padding row, closing rule: [render.rs:864](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/render.rs:864), [render.rs:1262](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/render.rs:1262), [render.rs:2303](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/render.rs:2303) | MATCHES |
| 3 | SubTree `⌂` always | Sim shows home only in subagent view: [tui.js:2908](</Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:2908>) | Home row on both session and subagent surfaces: [render.rs:1405](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/render.rs:1405) | DEVIATES — documented owner override |
| 4 | حيدر half-block mark and dignity gate | Sim uses shaped text: [tui.js:3161](</Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:3161>), [tui.js:3222](</Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:3222>) | Half-block maps and whole-or-nothing gates: [mark.rs:26](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/mark.rs:26), [mark.rs:131](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/mark.rs:131), [render.rs:231](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/render.rs:231) | DEVIATES — documented terminal/owner override |
| 5 | Launcher 70-cell cap | `.recent { width: min(560px,92%) }`: [tui.js:4331](</Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:4331>) | Centered `LAUNCHER_COLS` cap and span-aware ellipsis: [render.rs:442](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/render.rs:442), [render.rs:607](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/render.rs:607) | MATCHES |
| 6 | Visual/dump-frame coverage | Reference boot, launcher, and session surfaces: [tui.js:3157](</Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:3157>) | Wide, dignity-yield, deep-shed, todo and waiting frames: [dump_screens.rs:45](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/examples/dump_screens.rs:45), [dump_screens.rs:51](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/examples/dump_screens.rs:51), [dump_screens.rs:237](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/examples/dump_screens.rs:237) | MATCHES — verification-only item |
| 7 | Todo collapse, hover, spacing | Collapse button and current-item summary: [tui.js:2861](</Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:2861>), styling [tui.js:4627](</Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:4627>) | Collapse summary, full-row hover hits and processing pulse: [render.rs:1093](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/render.rs:1093), [render.rs:2913](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/render.rs:2913) | MATCHES |
| 8 | Breathing rows and `✳ Waiting…` | Sim has block spacing and derived WAITING badge, but no `✳` line: [tui.js:2810](</Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:2810>), [tui.js:3357](</Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:3357>) | Shed-first breathing rows and tree-derived waiting line: [render.rs:848](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/render.rs:848), [render.rs:1296](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/render.rs:1296) | DEVIATES — documented owner addition |
| 9 | Drag-select and auto-copy | Browser transcript retains native DOM selection: [tui.js:4451](</Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:4451>) | Captured-mouse linear selection, rendered extraction, `pbcopy` then OSC 52: [runtime.rs:407](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/runtime.rs:407), [select.rs:21](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/select.rs:21), [clipboard.rs:22](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/clipboard.rs:22) | DEVIATES — documented terminal mechanism; functional behavior matches |
| 10 | `⌃C` navigation, not immediate exit | Sim owns navigation through Esc; browser retains native Ctrl-C: [tui.js:2507](</Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:2507>) | Ctrl-C navigates off non-launcher surfaces; launcher/boot quit: [app.rs:1251](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/app.rs:1251) | DEVIATES — documented owner keymap |
| 11 | Sticky origin band, hover, click | Compute/jump: [tui.js:2620](</Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:2620>); background, border, hover: [tui.js:4597](</Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:4597>) | Pinned band, ellipsis, hover, jump suppression: [render.rs:1040](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/render.rs:1040), [app.rs:2453](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/app.rs:2453) | DEVIATES — documented alpha/blur-to-terminal adaptation |
| 12 | Per-surface status derivation | Per-session `runStates`; launcher derives IDLE: [tui.js:633](</Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:633), [tui.js:782](</Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:782>) | Check-in leaves a neutral launcher; busy state remains on its row: [app.rs:2200](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/app.rs:2200), [render.rs:455](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/render.rs:455) | MATCHES |
| 13 | Per-session map and persistence | State/persistence: [tui.js:629](</Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:629>), [tui.js:698](</Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:698>) | Session ownership: [session.rs:23](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/session.rs:23); DTO/load/hydration: [demo_store.rs:102](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/demo_store.rs:102), [demo_store.rs:477](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/demo_store.rs:477) | **BROKEN** — permissive hydration plus reset ID reuse |
| 14 | Animations and efficiency | Pulse/keyframe users: [tui.js:3943](</Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:3943>) through [tui.js:5563](</Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:5563>) | Predicate and select-gated 600ms phase: [app.rs:1071](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/app.rs:1071), [runtime.rs:254](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/runtime.rs:254), [style.rs:173](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/style.rs:173) | DEVIATES — only documented/accepted taste and aura-orb residuals |

## 2. Findings

1. **P1 — Persistence accepts incompatible and structurally partial state, silently replacing the seeds.** [demo_store.rs:102](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/demo_store.rs:102), [demo_store.rs:155](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/demo_store.rs:155), [demo_store.rs:173](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/demo_store.rs:173)

   `StateDto` has no version discriminator and serde ignores unknown fields. A real PTY boot with a valid payload carrying `"version":999` reopened its marker session and rewrote the file without the version. A second real PTY fixture containing only `{"sessions":[{"id":99}]}` replaced all three seeds with one blank session. In the sim, `s.branches.map(...)` throws before `setSessions`, so the catch preserves seeds. Truncated JSON correctly fell back to three seeds. Guard ordering 2–5 is correct; guard 1 is insufficient.

2. **P1 — `/reset` reuses a session identity while an uncancelled auto-title callback still owns it.** [app.rs:1806](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/app.rs:1806), [app.rs:2106](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/app.rs:2106), [runtime.rs:872](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/runtime.rs:872), [runtime.rs:1344](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/runtime.rs:1344)

   `fresh_session` no longer bumps a generation; it sets `session_epoch = 0`, while `/reset` resets `next_session_id = 4`. The control-tagged title timer survives and is keyed only by ID. Real PTY sequence:

   `zzz old epoch leak` → immediate `/reset` → `fresh replacement`

   produced `· session titled — “Zzz old epoch leak”` in the replacement and showed the replacement launcher row with the old blurb. The sim uses `s-${Date.now()}` IDs at [tui.js:1617](</Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:1617>), so the old callback cannot find a replacement after reset.

3. **P2 — The committed probe ladder is not an enforceable, hermetic release gate.** [pty-probe.py:70](/Users/rizzist/haider-run/haider-tui2/scripts/tui-probes/pty-probe.py:70), [pty-probe-ml.py:78](/Users/rizzist/haider-run/haider-tui2/scripts/tui-probes/pty-probe-ml.py:78), [pty-probe-sub.py:112](/Users/rizzist/haider-run/haider-tui2/scripts/tui-probes/pty-probe-sub.py:112), [pty-probe-anim.py:108](/Users/rizzist/haider-run/haider-tui2/scripts/tui-probes/pty-probe-anim.py:108)

   The base, multiline and subagent probes only print observations and always exit zero. Persistence excludes panic text and child status from `ok`. Animation inherits `NO_COLOR`, so its ink assertion false-fails in the present environment; when the row is invisible it bypasses the alive and ink checks, and `0 alt-enter == 0 alt-leave` can pass a dead pre-alt-screen process. The 40KB ceiling catches a full repaint storm, but is too loose to catch a high-rate diff/reset storm.

4. **P2 — W3c is no longer a literal one-source seam.** [runtime.rs:229](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/runtime.rs:229), [runtime.rs:313](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/runtime.rs:313), [runtime.rs:557](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/runtime.rs:557), [runtime.rs:1262](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/runtime.rs:1262)

   W3c must replace/remove more than the canned beat source: CLI hydration, the `run_demo` store parameter/save/purge paths, DemoDriver-owned per-session meters, arm ownership, and background-session routing. The UI session map and animations are reusable; the demo store is clearly isolated and deletable, but background routing presently lives inside the demo driver instead of a common envelope router.

5. **P3 — Local clipboard success is reported before `pbcopy` exit is known.** [clipboard.rs:22](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/clipboard.rs:22), [runtime.rs:1790](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/runtime.rs:1790)

   `copy_local` returns success after spawn plus stdin write, then reaps asynchronously. On this headless host, `pbcopy` spawned but `pbpaste` remained empty. If the terminal also rejects OSC 52, the UI can say `· copied` with no clipboard updated. The PTY wire path itself worked: one OSC 52 sequence was emitted and decoded to the selected text `ecent sessions`.

### Animation adversarial audit

Every sim pulse site is registered:

- Launcher rail/dot → busy launcher session.
- Transcript thinking → `projection.is_thinking()`.
- Running tool glyph → active and viewed-chip `streaming_tool_live`.
- Processing todo → pinned current todo.
- Running/tool/input-required chip glyphs and chip-view badge → recursive `chips_animated`.
- Session mic and Aura hold → `listening`/Aura listening.
- Aura running roster → running roster predicate.
- Boot `.sub` → Boot always animated.
- Badge → exact WAITING/STARTING/PERMISSION/EFFECT_UNKNOWN set.

No missed or eternal state was found. `IDLE_I` is still, matching [tui.js:5559](</Users/rizzist/Documents/CODING/next-diffforge/src/pages/tui.js:5559>). The `● ↔ ◌` thinking glyph and boot current-check pulse are documented taste extensions. Aura orb breathe/ring remains the explicitly ledgered residual at [OPTIMIZATIONS.md:34](/Users/rizzist/haider-run/haider-tui2/docs/OPTIMIZATIONS.md:34). Phase is absent from the persistence snapshot, and the hash-invariance test passes.

### Teardown/keymap audit

ArmOwner interrupt correctly cancels only the attached parent session; children and Aura outlive it as the sim requires. Navigation preserves session work intentionally. Reset cancels Session/Chip arms and resets Aura. The exception is the P1 auto-title control callback above.

Keymap verified:

- Ctrl-C: Session/Subagent/Aura/overlay → launcher; launcher/boot → quit.
- `/quit` and `/exit` also quit.
- Session Esc: interrupt when running; detach when idle.
- Subagent Esc → session.
- Aura Esc → attached session or launcher.
- Help Esc/Enter/q closes.
- Non-blocking menu Esc dismisses; blocking menu Esc is swallowed.
- Launcher/boot Esc is a no-op, matching the sim.
- Input-channel closure exits the runtime.

### Test integrity and full gates

No test function or marker was deleted or renamed in `git log -p f8d23f3..6dd0a34 -- '**/tests/**'`.

Test-count progression is monotonic:

`477 → 489 → 489 → 503 → 508 → 508 → 517 → 523`

The rail-aware edge tests still assert the shared text edge, 70-cell cap, centering, and Aura rule at `left - 1`: [tui3_visual_hover_tests.rs:171](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/tests/tui3_visual_hover_tests.rs:171), [tui4_owner_wave_tests.rs:353](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/tests/tui4_owner_wave_tests.rs:353).

Static gates all pass:

- `cargo build --workspace`
- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo fmt --all -- --check`
- `cargo run -p xtask -- test-count` — `523 tests (baseline 523)`

## 3. Mutation-check audit

| Source commit | Mutation | Required failure | Restoration |
|---|---|---|---|
| `6dd0a34` | Removed thinking from `AppModel::animated()` | `thinking pulses` failed | `app.rs` restored to blob `a2c34fe4934b7eae63a5844df2e9c381d8e0c7e0`; test passed |
| `009831a` | Removed empty-session guard | `EMPTY session array → seeds` failed | `demo_store.rs` restored to blob `ce6707dece101f8ba1ca7d234cd224ff5fa08e20`; test passed |
| `6dd0a34` | Added `IDLE_I` to badge pulse set | `⏸ IDLE (i) is still` failed | `projection.rs` restored to blob `d4021f5ff21f0f242b495fb883c3789385a35242`; test passed |
| `3da820d` | Disabled background-session routing | Driver went silent in `background_events_land_in_the_slot_and_reopen_restores_exactly` | `runtime.rs` restored SHA-256 `5675d0a13874dc66f5428c642efb2425b8134ff30b9ff43119aec9ad39ebe9e8`; test passed |

Final HEAD is unchanged; status, diff, and stash list are empty.

## 4. Probe results

| Probe | Size | Result | Evidence |
|---|---:|---|---|
| `pty-probe` | 118×36 | PASS | 9,791 B; alt 1/1; max row 36; rule 118 |
| `pty-probe` | 90×10 | PASS | 4,535 B; alt 1/1; max row 10; rule 90 |
| `pty-probe` | 90×7 | PASS | 3,481 B; alt 1/1; max row 7; rule 90 |
| `pty-probe` | 90×5 | PASS | 2,797 B; alt 1/1; max row 5; rule 90 |
| `pty-probe` | 90×1 | PASS | 944 B; alt 1/1; max row 1 |
| `pty-probe-ml` | 118×36 | PASS | 27,370 B; alt 1/1; composer lines present |
| `pty-probe-ml` | 90×10 | PASS | 12,148 B; alt 1/1; composer lines present |
| `pty-probe-sub` | 118×36 | PASS | 85,614 B; alt 1/1; subtree/Aura/back path painted |
| `pty-probe-sub` | 90×10 | **FAIL literal environment** | Sticky prompt painted, but `sticky_band_ground=False` under inherited `NO_COLOR=1`; passes after clearing `NO_COLOR` |
| `pty-probe-persist` | 118×36 | PASS | Run 1: 19,697 B; run 2: 15,576 B; file and restored row present; alt 1/1 both |
| `pty-probe-anim` | 118×36 | **FAIL literal environment** | 391 B/6s; alive/bounded and alt 1/1, but both ink assertions false under `NO_COLOR=1` |
| `pty-probe-anim` | 118×36, colors normalized | PASS | 911 B/6s; gold and maroon observed; alt 1/1 |
| `pty-probe-anim` | 90×10, colors normalized | PASS, weak | 250 B/6s; row shed, so alive/ink assertions are bypassed |

An extra 90×10 persistence run failed only its launcher-row visibility assertion because that row is shed at the compact size; 118×36 is the declared persistence gate.

VERDICT: NO_SHIP

---

## Fable design report

DESIGN FINDINGS

**D1 — must fix before merge: none found.** Core design calls sound: pure state in the model, effects in the runtime (CopySelection/PurgeDemoStore reducer-requested, runtime-executed); the anim clock is correctly NOT an arm (buffers no events, carries no identity, dies with the select loop); anim_phase excluded from persistence by DTO construction and pinned by `phase_ticks_never_touch_the_persistence_snapshot`. Suite green at 6dd0a34 (522 passed in this environment); clippy clean.

**D2 — fix or ledger before merge:**

- **D2-1. `session_epoch` is now a stored twin of `active_session`.** TUI4c redefined it (app.rs:2110-2113) and maintains `session_epoch == active_session.unwrap_or(0)` by hand at three sites (app.rs:2191, 2206/2228, 2113). The field doc (app.rs:895-898) still describes the old bump-on-fresh_session semantics; epochs now legitimately recur on reattach. Same twinned-state class the arc fixed for the roster counter. Fix: derive it or rewrite doc + assert invariant; "epoch" (implying monotonicity) is now wrong vocabulary.
- **D2-2. One-honour-roll law optional at the driver constructor.** `DemoDriver::new` mints a private roster counter (runtime.rs:686-693) that `adopt_roster` (runtime.rs:716) optionally replaces. Production wires it (runtime.rs:236) but only 2 of 6 driver-driving test files do — tui4c_session_map, turn_engine, subagent_aura, tui31_lifecycle run split-brain. Make the constructor take the counter so the law is unrepresentable to break.
- **D2-3. Three ledger rows falsified by this arc, never retired** (docs/OPTIMIZATIONS.md): (a) in-app drag-select "left to native ⇧-drag" — TUI4b shipped it (select.rs:7-9 says it replaces the row); (b) todos collapse "planned → W3" — TUI4a shipped the toggle (app.rs:853-856); only the multi-menu-queue half still true; (c) حيدر mark "planned → graphics tier" — mark.rs half-block art killed the premise; graphics tier now residual polish.
- **D2-4. TUI3 review's MUST-trigger fired unpaid.** Ledger row says the paused-time driver harness extraction to tests/common/ "MUST land before the next TUI test file is created." TUI4 created five; `pump_until`/`drain` copied in 6 files, `launcher_model` in 14; no tests/common exists. Pay it or amend honestly.
- **D2-5. W3c touch-list not written where an implementer will find it.** demo_store.rs documents its own death; the other seam touches (absorb's DemoEvent coupling, u64 ids vs protocol SessionId, AppRequest::PurgeDemoStore) exist only in this review. One ledger row closes it.

**D3 — notes:**

- **D3-1.** Period fold honestly documented (600ms quantum; 1.1/1.3/1.4/1.5s sim pulses → 1.2s; shimmer 1.8s exact) but LOCKSTEP PHASING divergence is not: sim elements drift on independent clocks; the port pulses in sync. Arguably better in a terminal; needs one clause in the TUI4d row.
- **D3-2.** `DemoStore::save` (demo_store.rs:119-131) serializes the full StateDto on every dirty frame to compute the hash-skip; the skip avoids disk, not serialize. The 13b brief specified store_dirty + debounce; shipped design traded it for serialize+hash-per-frame. Right call at demo scale, but efficiency-law-adjacent with no ledger line.
- **D3-3.** `hydrate` (demo_store.rs:480-535) accepts a persisted session id 0, colliding with the scratch-lineage sentinel (runtime.rs:1272-1278 drops Session(0)-owned events when a session is attached): corrupt/hand-edited file with id 0 yields a session whose turns silently vanish. One skip-or-bump guard.
- **D3-4.** `StopScripts` doc (runtime.rs:896-908) still lists `/clear` as a caller; its global cancel (`Session(_) | Chip` + all meters) is only correct because the caller set shrank. Rename/re-scope (ResetAll) on next runtime touch.
- **D3-5.** `animated()` runs on every loop wake and allocates (status_badge String, chip-tree walk). Bounded, demo-scale fine; note only.

**Gate quality:** probes assert laws, not absence-of-crash; headless tests pin armed-iff-animated, exact badge vocabulary, guard-by-guard hydration; probe scripts self-isolate profiles.

W3C SEAM REPORT — what the live-attach swap touches:

1. The seam itself (runtime.rs): DemoDriver + `(arm, DemoEvent)` channel → attach stream; ArmTable/ArmOwner, SessionMeters + prime_meter, spawn_boot, respond_*/AutoTitle/finish_turn engine, has_session_arms stale-menu gate all die with the driver (daemon owns run identity, usage, titling, menu-resolver liveness — each must arrive as events).
2. run_demo wiring (runtime.rs:198-392): drop Option<DemoStore> param, frame-cadence + quit-path saves, PurgeDemoStore intercept, adopt_roster, meter-priming; outbox drain re-targets to RPC send.
3. main.rs (~151-170): DemoStore load/hydrate block; theme precedence re-lands on the real store.
4. app.rs: AppRequest::PurgeDemoStore (app.rs:655) + /reset arm (app.rs:1821); handle_request's demo vocabulary is the reverse half of the seam.
5. session.rs:121 `SessionState::absorb(DemoEvent)` — envelope half already factored (absorb_envelope); chip state arrives only via DemoEvent chip variants; live subagent state must come as envelopes.
6. Session identity keyed u64 throughout (SessionState.id, active_session, session_epoch, ArmOwner::Session/Chip, meters, next_session_id, mock seeds) vs haider-protocol's opaque String SessionId (ids.rs:30-32). Retype or map at attach.
7. demo_store.rs: deleted wholesale, per its own module docs.
8. Stays put, verified source-agnostic: reducer, projections, per-surface status derivation, animated()/anim_phase, render, select/clipboard/mark, hit map, sticky band, input pump, frame + anim ticks.

Seam is narrow but is three small cuts plus the driver — items 4, 5, 6 are where TUI4 let demo identity into layers that survive the swap. Each is a typedef/one-variant/one-signature fix; they need naming in the ledger, not redesign.

VERDICT — DESIGN: FIX_ITEMS — D2-1..D2-5. No D1s; none invalidates the arc's architecture.
