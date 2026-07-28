# TUI6 review round 3 — NO_SHIP (all seven r2 findings CLOSED; two new in the login transaction layer)

Reviewer: gpt-5.6 (codex), frozen a0b816d, scope c93e554..a0b816d (TUI6.2/2b/2c).

Closed and validated: mint-tagged sticky (exact repros land 22), universal budget publish, switch authority (operationally; `screen`/`login` pub = type-level note), title dignity + de-blinded sweep (mutation-proven load-bearing), one close path, band_rule_reserve as RELEASE runtime authority on all four surfaces (release-mode zeroing kills via real asserts), epoch click-drop documented. Modality attack: chokepoint sound pre-submit; hidden menus sound (no deadlock; two-menu behavior = the pre-existing single-menu projection limitation); s1-s6 run SHIPPING seams; same-key flip accepted as note. Law table: model-stores-no-wrap-state adjudicated as a scoped fail-safe exception (budget + mint-tag stored, no breakpoints).

Required fixes (TUI6.3):
1. **P1 — cancelled login attempts remain executable and uncorrelated.** Enter queues AppRequest::LoginApi (app.rs:2468); a close can land while it's queued; live_pass drains the still-live request AFTER an inbound surface-changing reply (runtime.rs:2145); a late Staged reply carries no card/attempt identity and mints the login command (live.rs:790); a late LoggedIn updates whichever card is open (live.rs:818). Probe-confirmed: abort→re-/login lets old and new Staged replies REUSE the same login command identity — credentials can commit after the UI said cancelled; an old result can mark a new card successful. Fix: attempt identity end-to-end (mint on card open, carried through Stage/LoginApi/replies), close retires queued pre-dispatch requests, driver in-flight state invalidated on retire, late replies for retired attempts ignored.
2. **P2 — pasted keys transit a Debug-printable String** (AppEvent::Paste(String), app.rs:1170; ingress copy drops unwiped at :1826). Fix: redacted Debug on the event + zeroizing ownership from paste receipt through the transfer into the card buffer.

P3 notes (queue, not in-round): pub `screen`/`login` type-enforcement, subagent-login+question regression pin, single-menu projection (pre-existing ledger row).

The seven Round 2 UI findings close under their exact repros, but the new login modality introduces one P1 transaction race and one P2 secret-hygiene defect. The merge gate remains blocked.

## Round 2 closure

| # | Finding | Implementation and re-executed repro | Result |
|---|---|---|---|
| 1 | Sticky column survives resize | The cache is mint-tagged by wrap budget and re-derived on mismatch at [composer.rs:583](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/composer.rs:583). Exact 13→5 repro lands byte 22, parked draft also lands 22, and Shift-Down selects `(22,27)` at [tui6_softwrap_tests.rs:577](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/tests/tui6_softwrap_tests.rs:577) and [tui6_softwrap_tests.rs:612](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/tests/tui6_softwrap_tests.rs:612). Resize publishes before queued input at [runtime.rs:429](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/runtime.rs:429); stash/restore carries the current budget at [app.rs:1545](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/app.rs:1545). Login-band borrowing does not bypass re-minting. | Closed |
| 2 | Empty render does not publish width | Width is published before the empty/login returns at [render.rs:2662](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/render.rs:2662). Fresh empty width 18, then type and queued navigation, lands byte 17 at [tui6_softwrap_tests.rs:644](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/tests/tui6_softwrap_tests.rs:644). | Closed |
| 3 | Surface-switch authority | `close_chip_state` now uses the atomic authority at [app.rs:2690](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/app.rs:2690); the aura-draft repro passes at [tui6_softwrap_tests.rs:677](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/tests/tui6_softwrap_tests.rs:677). Production direct assignments are limited to the authority plus documented founding, reset, and identity-flip seams. However, `screen` remains public at [app.rs:1222](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/app.rs:1222): recurrence is convention-prevented, not type-unrepresentable. | Operationally closed; type-level note |
| 4 | Question title dignity and blind sweep | Subagent floor now reserves title plus options at [render.rs:1623](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/render.rs:1623). The exact 90×12 frame contains title, four options, and both rules at [tui6_softwrap_tests.rs:1944](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/tests/tui6_softwrap_tests.rs:1944). Missing top-band content now asserts at [tui6_softwrap_tests.rs:1725](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/tests/tui6_softwrap_tests.rs:1725). | Closed |
| 5 | Login close strands composer/history | `close_login_card` pairs `login.take()` with the private restore at [app.rs:2521](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/app.rs:2521). Esc/Ctrl-C and the demo driver route through it; composer and history repros pass at [tui6_softwrap_tests.rs:730](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/tests/tui6_softwrap_tests.rs:730) and [tui6_softwrap_tests.rs:1013](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/tests/tui6_softwrap_tests.rs:1013). No production `login = None` clearing write remains. Transaction cancellation is a separate new P1 below. | Draft/history closure closed |
| 6 | Split/debug-only band authority | `band_rule_reserve` is the runtime function at [render.rs:2448](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/render.rs:2448), used by launcher at [render.rs:350](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/render.rs:350), session at [render.rs:880](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/render.rs:880), subagent at [render.rs:1670](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/render.rs:1670), and aura at [render.rs:2003](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/render.rs:2003). Release-mode zeroing killed every surface pin. | Closed |
| 7 | Epoch-bump click drop | The one-frame, fail-closed click-drop window is explicitly documented at [runtime.rs:2040](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/runtime.rs:2040). | Closed as accepted transient |

## Login-modality attack

### (a) Chokepoint, races, re-entry, wiping

Pre-submit switches are sound: `stash_draft` returns the borrowed band before parking the real draft at [app.rs:1522](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/app.rs:1522). Back-to-back key-changing switches before redraw close only once, preserve the ring, and do not double-restore.

The mechanism is not transaction-safe after submit:

1. Enter removes the secret and queues `AppRequest::LoginApi` at [app.rs:2468](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/app.rs:2468).
2. Esc or a key-changing switch can close the card while that request is queued.
3. `live_pass` applies an inbound surface-changing reply first, then drains the still-live request at [runtime.rs:2145](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/runtime.rs:2145).
4. A late `Staged` reply has no card/attempt identity and immediately creates the login command at [live.rs:790](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/live.rs:790).
5. A late `LoggedIn` updates whichever card is currently open at [live.rs:818](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/live.rs:818).

A temporary executable probe confirmed abort→immediate `/login` re-entry allows the old and new `Staged` replies to reuse the same login command identity. Credentials can therefore commit after the UI reported cancellation, and an old result can mark a new provider/alias card successful. The independent verifier reached the same result.

Secret handling is mixed:

- The persistent card buffer is correctly `Zeroizing`, Debug-redacted, and moved into `SecretWire` at [app.rs:710](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/app.rs:710); `SecretWire` zeroizes on drop at [frame.rs:249](/Users/rizzist/haider-run/haider-tui2/crates/haider-rpc/src/frame.rs:249).
- Pasted secrets first exist in Debug-printable `AppEvent::Paste(String)` at [app.rs:1170](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/app.rs:1170). The handler copies from that ordinary string into the protected buffer, after which the original drops unwiped at [app.rs:1826](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/app.rs:1826).

### (b) Band authority and hidden menus

Session and subagent branches suppress menus whenever the login card owns the band at [render.rs:751](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/render.rs:751) and [render.rs:1609](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/render.rs:1609). The menu is unrendered, has no hits, and takes the band on the first frame after close; the shipping session test proves this at [tui6_softwrap_tests.rs:944](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/tests/tui6_softwrap_tests.rs:944).

A two-menu probe showed the second `MenuOpened` replaces the first rather than queues it because projection stores `Option<Menu>` at [projection.rs:187](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/projection.rs:187). This is the already-documented multi-menu limitation in [OPTIMIZATIONS.md:34](/Users/rizzist/haider-run/haider-tui2/docs/OPTIMIZATIONS.md:34), not introduced by login modality.

`ttl_ms` is not locally scheduled; a daemon `MenuClosed`/`MenuAnswered` clears the hidden menu. Entry/Failed/Done cards are user-lived and dismissible by Esc; Submitting has the driver’s 30-second deadline at [live.rs:1052](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/live.rs:1052). I found no additional hidden-menu deadlock. The subagent suppression branch is production code, though it lacks a dedicated login-plus-question regression pin.

### (c) One close path

Production grep found initialization to `None`, opening to `Some`, and one clearing operation: `self.login.take()` inside `close_login_card`. The demo driver calls that method at [runtime.rs:1218](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/runtime.rs:1218). `restore_draft` is private.

As with `screen`, `AppModel.login` itself remains public at [app.rs:1292](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/app.rs:1292), so the invariant is not type-enforced outside current production call sites.

### (d) Same-key flip

Accepted as a note. Launcher-scratch→session-scratch returns before stash when the surface key is unchanged at [app.rs:1590](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/app.rs:1590). The card remains coherent and no draft, history, band, or secret corruption was reproduced.

### (e) Promoted s1–s6 seams

They exercise shipping code, not retyped algorithms. The fixtures use the production `render`, production `dispatch_input`, and real model switch/restore paths at [tui6_softwrap_tests.rs:1131](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/tests/tui6_softwrap_tests.rs:1131). All six passed.

## Mutation audit

| Mutation | Observed failure | Restoration |
|---|---|---|
| Remove the subagent title floor, with de-blinded sweep intact | `reserved_rule_sweeps_subagent_and_question_card` failed on options without title. Re-blinding the `None` arm made the same broken frame pass, proving the new assertion is load-bearing. | Restored |
| Remove the card-over-menu band gate | `login_card_outranks_an_arriving_menu_on_the_band` failed: menu face appeared while login retained keyboard ownership. | Restored |
| Remove the login abort from `stash_draft` | `async_session_open_under_the_login_card_aborts_it_and_keeps_the_ring` failed at the card-aborted assertion. | Restored |
| Force `band_rule_reserve` to return zero | Under `cargo test --release`, all five launcher/session/menu/subagent/aura reserve tests failed through ordinary `assert!` failures—not compiled-out `debug_assert!` checks. Restored run passed 5/5. | Restored |

All mutations were temporary patch/revert cycles. Final HEAD, tracked worktree, index, diff, and stash are byte-clean.

## Integrity, gate, and merge readiness

- Frozen revision verified: `a0b816dc31da8c07ccb62865b38357686485c67a`.
- Test ledger is exactly `842 → 854 → 855 → 861`.
- `git log -p -- tests/` shows 19 added tests and no removed test body or weakened assertion. The two deleted test-file lines were blind control flow replaced by the new assertion. Directed tests carry repro and mutation reasoning.
- `cargo test -p haider-tui --test tui6_softwrap_tests`: 49/49.
- `cargo test --workspace --no-fail-fast`: exit 0; no `FAILED` or `could not compile`.
- `cargo run -p xtask -- test-count`: 861 tests, baseline 861.
- `cargo clippy --workspace --all-targets -- -D warnings`: pass.
- `cargo fmt --all -- --check`: pass.
- Release CLI/daemon build: pass.
- PTY ladder: all 14/14 demo rows passed locally. The live harness could not enter the alternate screen or discover/start its detached daemon because process discovery is sandbox-denied; the supplied post-`a0b816d` orchestrator result remains 16/16. Workspace live/UDS driver tests passed locally.
- Read-only synthetic merge against `d9d66b4` has no conflict markers; `git diff --check` passes. The merge base is `19cad5151e9aedfdf268eab8506f22cfe216cb6c`.

## Law table

| Law | Adjudication |
|---|---|
| Grapheme-wrap | Pass |
| No ellipsis | Pass |
| Caret visible | Pass at supported geometry; documented physical tiny-frame degenerates retained |
| Render = click = navigation across resize/swap/switch | Pass on all promoted seams |
| Model stores no wrap state | Qualified, not literally true: it stores the current wrap budget and mint-tagged sticky column, but no wrapped rows/breakpoints. The tag makes stale geometry fail-safe; this is an explicit scoped exception to the literal law. |
| Two-rule-reserved, runtime-unified | Pass on all four surfaces under release semantics |
| Dignity/title restored | Pass, including 90×12 |
| Login modality: secret hygiene + no deadlock | Fail: hidden-menu behavior is sound, but cancellation/re-entry correlation is P1 and paste ingress hygiene is P2 |
| Zero idle wakeup | Pass: animation and frame ticks remain state-gated at [runtime.rs:291](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/runtime.rs:291); the login deadline wake is intentional, bounded work rather than polling |

## New findings

| Tier | Finding |
|---|---|
| P0 | None found |
| P1 | Cancelled login attempts remain executable and uncorrelated. A queued or staged attempt can commit after card close and can deliver its result into a newly opened card. Required closure: mint an attempt identity end-to-end, retire queued pre-dispatch requests on close, invalidate/cancel in-flight driver state, and ignore late replies for retired attempts. |
| P2 | Pasted API keys transit a Debug-printable ordinary `String` and its ingress copy is not zeroized on drop. Required closure: use redacted event Debug and zeroizing ownership from paste receipt through transfer into the card. |
| P3 | None found. Public `screen`/`login`, same-key retention, the known single-menu projection, and the missing subagent-login-specific pin remain notes rather than reproduced failures. |

VERDICT: NO_SHIP
