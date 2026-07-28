# W3c3 review round 2 — NO_SHIP (4 enumerated findings, field otherwise clear)

Reviewer: gpt-5.6 (codex), frozen 371bfbe, scope 8dd8044..371bfbe (W3c3.1 4b280de + W3c3.2 371bfbe).

Closure: 17/22 CLOSED, 3 PARTIAL (P1-2 via the new overflow finding; D2-1/charter via NF-4), 2 NOT_FIXED-as-ledgered (D2-3 attachment cap, D2-4 outbox retry — both accepted residuals with triggers). Both r1 law violations fixed at the reducer (strict-gap seed + /reset generations mutation-verified); identity law HOLDS; strict-gap re-marked VIOLATED solely through NF-1's overflow path. live_pass verified as the complete shipping tail. Silent-discard backstop exhaustive by construction (new variant = compile error). The request_input probe judged genuine (structure verified; execution environmental-PARTIAL — this runner denies UDS binds; the orchestrator's machine runs 16/16 + 3 live-row repeats).

Five mutations executed, all killed, all restored. Baseline 768→798→810 monotonic, zero deletions. Matrix: 6 PROVEN / 5 PARTIAL(-environmental) / 1 UNPROVEN(owner-run). Branch fast-forwardable from c74c409.

Required fixes (W3c3.3):
1. **P1 NF-1** — attach-barrier overflow (513th reply) clears held replay + AttachCaughtUp and publishes EventsLost BEFORE Attached installs, so no attachment exists to repair; later Attached leaves a silently stale surface (link.rs:57/:259, live.rs:860). Fix: retain/coalesce loss behind the barrier until the attach outcome installs, or bind the loss to the pending attach so Attached immediately reattaches. + overflow mutation test.
2. **P2 NF-2** — local attach ENCODE failure claims the slot then reports Failed{command_id:None}, which cannot release it: a permanent false latch (live.rs:606, link.rs:316, live.rs:937). Reachable with a small negotiated frame limit + long server session ID.
3. **P2 NF-3** — w3c3_ordered_send_tests.rs:229 accepts only PeerClosed where reader-EOF vs writer-failure legitimately race under first-reason-wins; accept either typed disconnect or force one edge.
4. **P3 NF-4** — RuntimeMode's mechanical-audit sentence remains literally false (positive-polarity sites at app.rs:2066/:3749; grep counts docs). Behavior correct; fix the claimed procedure.

---

HEAD verified as `371bfbea1d7ce00879741c5829fe0c52b6401705`. Final worktree is byte-clean: empty `git status --porcelain`, clean `git diff --exit-code`, and empty stash list.

## 1. Closure table

| Finding | Status | Before → current evidence |
|---|---|---|
| P1-1 fresh replay gap accepted | CLOSED | r1 `projection.rs:271-277` → attach cursor is latched at [live.rs:606](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/live.rs:606), seeded before route installation at [live.rs:703](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/live.rs:703), and never rewinds at [app.rs:3578](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/app.rs:3578). Mutation failed correctly. |
| P1-2 replay reorder/silent loss | PARTIAL | Previous response/event race, unused `lost_events`, and discarded `AttachCaughtUp` are repaired at [link.rs:112](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/link.rs:112) and [link.rs:613](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/link.rs:613). However, the new overflow path at [link.rs:259](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/link.rs:259) can still silently lose a replay; NF-1. |
| P1-3 double attach / send ordering | CLOSED | r1 latch in caller and independently spawned commands → latch now belongs to the sole emitter at [live.rs:565](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/live.rs:565); `begin_request` establishes wire order before spawning its waiter. Mutation produced two attaches and failed. |
| P1-4 login retry/disconnect wedge | CLOSED | Same-command recovery and deadline retirement are implemented at [live.rs:790](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/live.rs:790) and [live.rs:1030](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/live.rs:1030); the shipping loop wakes for it at [runtime.rs:2223](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/runtime.rs:2223). |
| P1-5 reset reuses generations | CLOSED | r1 hardcoded `1..3` → reset draws and advances the monotonic allocator at [app.rs:2792](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/app.rs:2792); stable IDs and fresh generations are separated at [mock.rs:496](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/mock.rs:496). Mutation failed correctly. |
| P1-6 unreachable live/cold sessions | CLOSED | Display-name hit authority was replaced by session identity; real `/sessions`, ordinal/ID opening, and cold rows are pinned in [w3c31_fix_tests.rs:105](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/tests/w3c31_fix_tests.rs:105). |
| P2-7 render gate AND/debug-only | CLOSED | Gate now uses OR at [w3c3_render_bench_tests.rs:150](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/tests/w3c3_render_bench_tests.rs:150); release CI step is at [.github/workflows/ci.yml:48](/Users/rizzist/haider-run/haider-tui2/.github/workflows/ci.yml:48). |
| P3-8 widened bounds undocumented | CLOSED | Ratio is deliberately `8x + 20ms`, with five-run measurements and rationale at [w3c3_render_bench_tests.rs:135](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/tests/w3c3_render_bench_tests.rs:135); cold ceiling returned to 250ms at line 216. Tagged W3c3.1 in commit dated 2026-07-28. |
| D1-1 second attach authority | CLOSED | `claim_slot` and the latch are centralized at [live.rs:642](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/live.rs:642); production `live_pass` contains `sync_selection`. |
| D1-2 local unanswerable cards | CLOSED | r1 local `/voice`/`tools` cards → live refusal before fabrication; production-path coverage in [w3c31_fix_tests.rs:981](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/tests/w3c31_fix_tests.rs:981). |
| D1-3 hardcoded probe success | CLOSED | Literal `True` was replaced by real `emit_request_input`, cold-terminal reconstruction, answer, continuation, and journal-CAS checks at [pty-probe-live.py:92](/Users/rizzist/haider-run/haider-tui2/scripts/tui-probes/pty-probe-live.py:92) and [pty-probe-live.py:366](/Users/rizzist/haider-run/haider-tui2/scripts/tui-probes/pty-probe-live.py:366). Execution remained environmental-PARTIAL. |
| D2-1 RuntimeMode charter | PARTIAL | The table and common predicate at [app.rs:821](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/app.rs:821) describe behavior correctly, but its literal grep/polarity claim remains false; NF-4. |
| D2-2 session voice fabrication | CLOSED | All live screens branch before local voice painting; coverage in [w3c31_fix_tests.rs:1029](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/tests/w3c31_fix_tests.rs:1029). |
| D2-3 hardcoded attachment cap | NOT_FIXED | Still `16`; accepted residual with exact trigger and prohibited substitutes at [OPTIMIZATIONS.md:279](/Users/rizzist/haider-run/haider-tui2/docs/OPTIMIZATIONS.md:279). |
| D2-4 reconnect-only mutation retry | NOT_FIXED | Retryable `Failed` still waits for reconnect; precisely ledgered at [OPTIMIZATIONS.md:280](/Users/rizzist/haider-run/haider-tui2/docs/OPTIMIZATIONS.md:280). |
| D2-5 login charter/timeout | CLOSED | Charter now matches the actual re-stage/retry/deadline path at [live.rs:114](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/live.rs:114). |
| D2-6 stale ledger/module claims | CLOSED | Demo-store charter corrected at [demo_store.rs:1](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/demo_store.rs:1); residuals and triggers corrected at [OPTIMIZATIONS.md:265](/Users/rizzist/haider-run/haider-tui2/docs/OPTIMIZATIONS.md:265). |
| D2-7 copied loop/tests | PARTIAL | Correctness-critical tail is production `live_pass` at [runtime.rs:2081](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/runtime.rs:2081); duplicated terminal shell remains deliberately ledgered at [OPTIMIZATIONS.md:277](/Users/rizzist/haider-run/haider-tui2/docs/OPTIMIZATIONS.md:277). |
| Folded P1-A silent discard | CLOSED | `handle_request` exhaustively matches `AppRequest`; demo-only arms audibly unwind at [live.rs:1223](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/live.rs:1223). A new enum variant causes a compile error. |
| Folded P1-B ghost slot | CLOSED | Paired claim/release at [live.rs:642](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/live.rs:642), including every `AttachFailed`; mutation reproduced the ghost and failed. |
| Folded login-timeout inertness | CLOSED | Real deadline plus loop wakeup at [live.rs:1052](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/live.rs:1052) and [runtime.rs:2227](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/runtime.rs:2227). |
| Folded charter drift | PARTIAL | Behavioral enumeration is corrected, but the claimed mechanical audit rule is not literally true; NF-4. |
| Permanent AttachFailed ping-pong | CLOSED | Permanent active failure deselects at [live.rs:921](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/live.rs:921); mutation immediately reproduced the next attach. |

## 2. Law table

| Law | Result | Evidence |
|---|---|---|
| Strict-gap | VIOLATED | The fresh-seed reducer path now holds, but barrier overflow can discard replay plus boundary and publish its gap before the new attachment exists, leaving a quiescent surface silently stale. |
| Sole cursor authority | HOLDS | Projection admission and monotone attach seeding are the only writers; reattach re-reads the projection cursor at [live.rs:1267](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/live.rs:1267). |
| Identity never reused | HOLDS | Double reset draws disjoint generations; live reset is refused; v1 hydration raises the allocator before the production demo loop and cannot asynchronously land after reset. |
| Stays-put | HOLDS | `select.rs`, `clipboard.rs`, `mark.rs`, `session.rs`, and `projection.rs` have zero two-commit delta; runtime-shell duplication is ledgered. |
| Demo determinism | HOLDS | All 14 demo ladder rows, snapshots, persistence, and v1 upcaster tests passed. |
| Secret hygiene | HOLDS | Login/secret suites passed; fake-provider seam does not touch credential paths; no wire/probe key exposure found. |
| Additive wire | HOLDS | No protocol/RPC production-source delta in these commits; all 20 wire goldens passed byte-identically. |

## 3. New-mechanism attack report

### 3(a) Attach state machine

- In flight: `ensure_attached` rejects attached, latched, and disconnected states, then `claim_slot` atomically installs LRU membership plus the latch.
- Attached: `Attached` removes the latch, seeds the cursor, installs both maps, and touches LRU.
- Retryable failure: releases both slot and latch. Active surfaces retry once through the sole emitter; background rows go cold.
- Permanent failure: releases, goes cold, flashes the refusal, and deselects the active surface. It is not stranded: the row remains listed and can be selected again.
- Eviction: route, attachment, and LRU membership are removed before replacement claim.
- Gap/Lagged/CaughtUp/EventsLost: old attachment is removed before reattach.
- Disconnect/reconnect: attachment maps and latches clear; desired LRU survives; resume reclaims once. Old response tasks are inert because the attach-response channel is replaced at [link.rs:177](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/link.rs:177).

An ordinary LRU-without-attachment-or-latch ghost is now unrepresentable through reply transitions. A new mirror state remains possible: LRU plus a false latch when local attach encoding fails before anything reaches the wire; NF-2. Retryable failure plus reconnect does not double-claim, although a daemon that returns retryable failure forever can still drive a response-paced loop—the configurable-cap trigger is ledgered.

### 3(b) `live_pass`

The complete r1 tail—request drain, demo clearing, `sync_selection`, and answer drain—is present at [runtime.rs:2103](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/runtime.rs:2103), with reply reduction and deadline expiry added ahead of it. Shipping calls it once at line 2254. Only terminal I/O, drawing, command-channel flushing, clipboard effects, and theme/title synchronization remain outside; no driver/reducer sequencing was retyped.

### 3(c) Attach barrier

Normal daemon ordering sends the correlated attach response before replay, but reader/task scheduling can still let replay frames outrun the response waiter. With 512 held replies, the 513th attachment-scoped reply clears the queue and sends `EventsLost` immediately. Because attachment installation is still pending, the driver has no new attachment to reattach. `Attached` can then arrive with the replay and high-water boundary gone.

Disconnect-channel replacement correctly makes orphaned responses inert. The overflow path does not.

### 3(d) Silent-discard backstop

The dispatch match is exhaustive with no wildcard. A new `AppRequest` variant breaks compilation until it is handled. Shell-owned requests are explicitly returned by `live_pass`; demo-only requests get an audible refusal and optimistic-state unwind.

### 3(e) Real request-input probe

`EmitRequestInput` is real provider vocabulary. Daemon injection occurs only when `HAIDER_TEST_FAKE_PROVIDER` is present; absent it, [main.rs:74](/Users/rizzist/haider-run/haider-tui2/crates/haider-daemond/src/main.rs:74) returns production dependencies.

The probe starts a cold third terminal after `MenuOpened`, reconstructs the card solely from committed history, answers with committed sequence/generation coordinates, requires both terminals to see the continuation, and checks one journal resolution. The implementation is genuine; this sandbox could not execute that live PTY row.

## 4. §6.4 matrix walk

| Acceptance row | Result | Evidence |
|---|---|---|
| Clean profile leaves one daemon and enters live TUI | PARTIAL-environmental | Official live probe failed before alternate-screen entry/daemon creation; socket bind reported `EPERM`. |
| Second terminal sees same contiguous session | PARTIAL-environmental | Driver/replay tests passed; live PTY row could not start. |
| Real Anthropic after `/login` | UNPROVEN | Correctly remains optional owner-run evidence. |
| Deterministic FakeProvider/no network | PARTIAL-environmental | Provider/daemon UDS scenarios passed; complete live PTY path could not start. |
| Either control attachment resumes exactly once | PARTIAL-environmental | Real daemon CAS scenarios and probe structure pass inspection, but mandated cold-third-terminal probe could not execute here. |
| Cancellation terminalizes supervised work | PROVEN | Scenario 8 and core cancellation suites passed. |
| Restart ambiguity/queued/input reconstruction | PROVEN | Scenarios 9–11 and graceful request-input recovery passed. |
| Lost mutation response causes no duplicate | PROVEN | Create, submit, cancel/menu, and login receipt/retry coverage passed. |
| Old daemon/version mismatch explicit | PROVEN | Client lifecycle and wire mismatch suites passed. |
| Demo remains runnable/pinned | PROVEN | 14/14 demo ladder plus snapshots, persistence, and upcaster coverage. |
| fmt/clippy/workspace tests | PARTIAL | fmt and clippy passed. One full workspace run failed an intermittent over-specific disconnect assertion; NF-3. |
| Anthropic smoke optional only | PROVEN | Live Anthropic tests remain explicitly ignored and are not merge gates. |

Result: **6 PROVEN / 5 PARTIAL / 1 UNPROVEN**.

## 5. Mutation-check audit

| Mutation | Observed failure |
|---|---|
| Delete fresh-attach `seed_cursor` | First gapped envelope painted one row instead of zero. |
| Reset seeds from `UiGeneration::FIRST` | Replacement generations repeated `[1,2,3]`. |
| Delete `ensure_attached` latch guard | One gap emitted two attaches. |
| Replace `release_slot` with latch-only removal | Ghost slot `s-16` was detected. |
| Delete permanent-failure deselection | Loop tail immediately emitted the next attach. |

All five were restored immediately. Restored `w3c31_fix_tests` passed 18/18 and `w3c31_r2_tests` passed 12/12.

Gate/test integrity:

- Baseline is exactly 768 → 798 → 810; `xtask check` reports 810/810.
- No test file or test function/attribute was deleted. Existing edits are identity-coordinate retypes or directed strengthening.
- All 12 r2 tests call production `runtime::live_pass` through [w3c31_r2_tests.rs:83](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/tests/w3c31_r2_tests.rs:83).
- fmt, workspace clippy `-D warnings`, release build, release render benchmark, directed TUI suites, and wire goldens passed.
- Official ladder: demo 14/14; live 0/2 because the managed environment denied startup/socket creation.
- The branch is fast-forwardable from `c74c409` (`0 behind / 10 ahead`). `Cargo.lock` and `test-baseline.txt` are touched, but there is no actual or unexpected conflict surface.

## 6. New findings

1. **P1 — attach-barrier overflow silently loses a quiescent replay.** [link.rs:57](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/link.rs:57), [link.rs:259](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/link.rs:259), [live.rs:860](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/live.rs:860). Hold 512 replay events while the attach waiter is scheduler-delayed; `AttachCaughtUp` becomes reply 513, clears everything, and publishes `EventsLost` before `Attached`. The driver has no installed attachment to repair; later `Attached` leaves a silently stale surface. Required correction: retain/coalesce loss behind the barrier until attach outcomes are installed, or associate loss with pending attaches; add an overflow mutation test.

2. **P2 — local attach encoding failure creates a permanent false in-flight latch.** [live.rs:606](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/live.rs:606), [link.rs:316](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/link.rs:316), [live.rs:937](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/live.rs:937). The slot is claimed before `begin_request`; encode failure becomes generic `Failed { command_id: None }`, which cannot identify or release the session. Later selection exits on the stale latch. Reachable with a small negotiated outbound frame limit and a server-returned long session ID.

3. **P2 — workspace gate has a real disconnect-race flake.** [w3c3_ordered_send_tests.rs:229](/Users/rizzist/haider-run/haider-tui2/crates/haider-client/tests/w3c3_ordered_send_tests.rs:229). One full run received typed `Disconnected(Io("Broken pipe"))` where the test accepts only `PeerClosed`; exact reruns passed. Reader EOF and writer failure legitimately race under the client’s documented first-reason-wins rule. Accept either typed disconnect or synchronize the fake peer to force one deterministic edge.

4. **P3 — RuntimeMode’s mechanical-audit charter remains literally false.** [app.rs:874](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/app.rs:874). It says every fabrication branch reads `!self.mode.fabricates_locally()` and raw grep counts the table. Esc and chip-close use positive polarity at lines 2066 and 3749, and grep also counts documentation/helper references. Behavior is correct; the claimed audit procedure is not.

VERDICT: NO_SHIP
