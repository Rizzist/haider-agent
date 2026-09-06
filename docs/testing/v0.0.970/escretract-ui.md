# escretract-ui — v0.0.970 client/TUI evidence

Branch: `lane-970-escretract-ui`, cut from `wave-970` @ `b8635f88` (the merge
that landed the daemon/contract half, `9dc967b8`). Executed host: macOS arm64.

Owner QoL (2026-09-03): the user sends a prompt and presses Esc before any
response token arrives. Today that only cancels. Wanted: the turn is cancelled
**and** the message leaves the transcript and reappears in the composer — text
and attachments — to edit and resend. Once a token has arrived, Esc stays a
plain cancel and both the message and the partial reply remain.

The daemon half already shipped `turn.retract`, the durable `prompt_retracted`
fact, the typed `too_late` arbitration, and the transcript projection that
hides the exact retracted node. This lane is the keyboard binding, the composer
application of the returned draft, and the race UX. **No daemon, store,
protocol or RPC source changed** — the wire already existed.

## Implemented behaviour

**Which command Esc issues.** `AppModel::can_retract_prompt` gates the Esc
mid-turn branch on three necessary conditions: the mode does not fabricate its
own turns (a demo turn has no durable prompt for a daemon to hand back), the
daemon advertised `turn_retract_v1`, and this run has committed no first
response. It emits `AppRequest::RetractPrompt`, which the live driver turns
into `LiveCommand::Retract` pinned to the run the *committed stream* named —
the same authority the existing Interrupt arm uses, so no run id is ever
invented. Any condition failing leaves the existing `AppRequest::Interrupt`
path byte-for-byte unchanged, which is what keeps an older daemon, the demo
twin and every existing Esc test working.

**How the window is known to be open.** Not by counting rendered rows. The
daemon commits `response_started` — the same fact its session writer arbitrates
retraction against — and the client records it through
`session::route_response_boundary`. That envelope is deliberately
`render.ui == false`, so it is admitted as `Skip` and no display surface moves
for it; the router is therefore hooked in `absorb_raw_active` beside the branch
registry and hook-fact recorders, which the file's own doc comment already
establishes as the place for `ui == false` **command-state** truth. The flag
resets at the same turn-opening edge that resets the throughput tally, so the
window is genuinely per-run.

This read is optimistic by construction — it can only be as fresh as the last
envelope received — so it is an honest UI decision, never the correctness
authority. The daemon's typed `too_late` is.

**What happens on each outcome.** On success the receipt seeds the composer
with the exact returned text, cursor at the end, and rebuilds the attachment
chips from the receipt's CAS blocks via `PendingAttachment::carrying` — the
same constructor the `session.fork` draft uses, so the chips carry the daemon's
own verified blocks and **no upload is issued**. The transcript row is removed
by the daemon's `prompt_retracted` fact arriving on the stream, not by the
receipt, so the live view and a replay reach the identical entry list by
construction rather than by a second code path. On `too_late` — and on *any*
other failure code — the driver issues exactly one ordinary `turn.cancel` for
the same run and the transcript keeps the prompt and any partial reply; the
status line separates the two cases ("too late to retract — cancelled" versus
"retract failed — cancelled · <reason>").

**Race UX.** `retract_pending` refuses submit between Esc and the daemon's
answer, because in that window the prompt may still be accepted and the bytes
exist only on the wire. Every terminal path clears it — the receipt, the
`too_late`/error fallback, a driver that found no run to retract, a receipt for
a session the user has since left, and a disconnect — so Enter can never be
left permanently dead. The refusal happens *before* the composer take, so the
draft survives; and anything typed into the guarded window is kept and follows
the restored prompt rather than being overwritten by it.

## Adjacent surface this lane had to fix

`apply_prompt_retraction` hides the transcript row, but **prompt history is a
separate surface** with its own durable coordinates. Left alone, the Esc-Esc
chooser kept offering a prompt the daemon says never happened — and a
`session.fork` at its hidden cut is refused by the daemon half, so the entry
could only disappoint. `AppModel::forget_retracted_prompt` drops it, keyed on
the exact `prompt_seq` and never on text, so an equal-text prompt in another
turn survives. It is driven from the durable fact, so it holds on replay too.
Mutation check M4 below confirms the two surfaces are genuinely independent.

## Citation audit

Every construct was re-grepped rather than trusted from the brief. **The line
numbers below are coordinates in the baseline, `b8635f88`** — this lane's own
diff moves `app.rs` and `link.rs`, so worktree lines differ (the Esc arm is
8673, the cancel map 1072, `open_forked_session` 16917 as committed here). The
construct, not the number, is the claim.

| Claim | Audit |
| --- | --- |
| Esc mid-turn arm, `app.rs:8650` | **Correct** — `KeyCode::Esc if self.screen == Screen::Session`, `turn_active` branch pushing `Interrupt`. |
| `LiveCommand::Cancel` wire map, `link.rs:1071` | **Correct** — the new `Retract` arm sits beside it and reuses its branch-capture comment. |
| Fork draft restores text + attachments, `app.rs:16852` | **Correct** — `open_forked_session`; its chip loop is the template the restore copies. |
| `turn.retract` needs the `TurnRetraction` client loop | **Rejected as unsuitable.** `haider-client::TurnRetraction::execute` is a blocking `async` loop over `&RpcClient`; the TUI's driver is a synchronous outbox state machine. Using it would have required a second command path outside the outbox, losing reconnect replay and receipt retirement. The `too_late` fallback is instead expressed as one enqueued `Cancel`, which is the same one-extra-round-trip the client loop performs. The CLI keeps using the client loop. |
| "the TUI cannot locally know whether the window is open" | **Superseded.** The boundary envelope reaches `absorb_raw_active` as `Skip`; recording it there needs no `render.ui` change and no daemon edit. |
| `streamed_output_chars > 0` as the first-delta proxy | **Rejected.** It counts text/reasoning/tool-arg characters only — strictly NARROWER than `is_response_delta`, which also counts refusals, web sources, tool-call start/end, server tool use and provider-opaque events. A tool-call-first or web-sources-first response would have read as "no response yet" and offered a retraction the daemon had already closed. |

Territory: `crates/haider-tui` only — six source files, one of them a single
match arm in the demo driver that the exhaustiveness check required. No daemon,
store, core, protocol, RPC or client source changed. No dependency changed.
Windows
and Linux behaviour is **by inspection** — this is reducer/state-machine logic
with no platform surface; the executed host is macOS arm64.

## Named regressions

`crates/haider-tui/tests/w970_escretract_ui_tests.rs` — 13 tests.

| Behaviour | Test |
| --- | --- |
| Retract before the first token; node gone; text, cursor, chip and status restored; real wire shapes both ways | `esc_before_the_first_token_retracts_and_restores_text_and_attachments` |
| Typed `too_late` → exactly one plain cancel, prompt and reply retained | `too_late_falls_back_to_one_plain_cancel_and_keeps_the_prompt_on_screen` |
| Any other failure → plain cancel with the public reason | `an_unexpected_retract_failure_still_cancels_and_names_the_error` |
| Double-Enter guard; draft survives the refusal; typed text kept; Enter works after | `enter_cannot_resend_a_prompt_while_its_retraction_is_still_in_flight` |
| `ui == false` boundary closes the window; no display surface moves | `a_committed_response_boundary_makes_esc_a_plain_cancel` |
| The window is per-run, not per-session | `a_new_turn_reopens_the_retraction_window_the_previous_response_closed` |
| Replay parity + history, with an equal-text neighbour that must survive | `a_retracted_prompt_is_gone_from_replay_and_from_the_esc_esc_chooser` |
| A receipt for a session the user left PARKS the prompt rather than losing it | `a_receipt_for_a_session_the_user_left_parks_the_prompt_instead_of_losing_it` |
| No live run → no invented command, and no stranded composer | `esc_with_no_live_run_lifts_the_guard_instead_of_stranding_the_composer` |
| Demo twin never asks for a retraction it would have to invent | `a_demo_turn_never_asks_for_a_retraction_it_would_have_to_invent` |
| Feature-gated: an older daemon gets the cancel it understands | `an_older_daemon_that_cannot_retract_gets_the_cancel_it_understands` |
| A link-gate rejection is answered, not dropped, and still cancels | `a_daemon_that_drops_the_retract_frame_still_gets_one_cancel` |
| The background twin records the boundary and prunes its own chooser | `a_parked_session_records_the_boundary_and_forgets_its_retracted_prompt` |

### Mutation checks — executed, not asserted

Each mutation was applied to the working tree, the suite run, and the tree
restored and re-verified green.

| # | Mutation | Result |
| --- | --- | --- |
| M1 | `can_retract_prompt` → `false` | **7 failed** |
| M2 | `route_response_boundary` call deleted from `absorb_raw_active` | **2 failed** |
| M3 | `retract_pending` guard deleted from `submit_composer` | **1 failed** (the double-Enter law) |
| M4 | `forget_retracted_prompt` call deleted | **1 failed** — the history test only, proving transcript and history are independent surfaces |
| M5 | `response_started = false` reset deleted from the turn-opening edge | **1 failed** (the per-run law) |
| M6 | the discard-on-left-session early exit restored in the `PromptRetracted` arm | **1 failed** (the parked-draft law) |
| M7 | the escretract hook deleted from `SessionState::absorb_raw` | **1 failed** (the background twin) |

M7 initially killed **nothing** — the background twin had no test at all. That
gap was found by review, not by the suite, and the test was written to close
it. Restored tree: **13 passed, 0 failed**.

## Gate results

Required ENV LAW throughout (`RUST_MIN_STACK=8388608`,
`HAIDER_DISCOVERY_DISABLED=1`, `HAIDER_TEST_DEVICE_NAME=test-mac`,
`CARGO_INCREMENTAL=0`, `CARGO_PROFILE_DEV_DEBUG=0`, `CARGO_BUILD_JOBS=4`),
`CARGO_TARGET_DIR=/private/tmp/haider-escretract-ui-target`. `haider` and
`haiderd` were prebuilt before the daemon-touching suites and
`HAIDER_TEST_SIBLINGS_PREBUILT=1` set; built `haiderd` is 193 MiB, over the
10 MiB floor (registry #64). Disk was checked before each build-capable gate.

### Executed

| Gate | Result |
| --- | --- |
| `cargo test -q -p haider-tui --no-fail-fast` | **1,468 passed, 0 failed** |
| `cargo test -q -p haider-cli --no-fail-fast` | **760 passed, 0 failed** |
| `cargo test -q -p haider-daemond --no-fail-fast` | **154 passed, 0 failed** |
| `cargo clippy --workspace --tests -- -D warnings` | **pass, 0 errors** |
| `cargo clippy -p haider-tui --all-targets -- -D warnings` | **pass** |
| `cargo fmt --all -- --check` | **pass** |
| `git diff --check` | **clean** |
| `cargo run -p xtask -- test-count --update` | **5,056 -> 5,069** (+13, exactly this lane's tests), re-verified `ok` |

Summed executed libtest: **2,382 passed, 0 failed** across the three crates.

`haider-cli` and `haider-daemond` are not incidental — they are the **only two
crates in the workspace that depend on `haider-tui`** (`grep haider-tui
crates/*/Cargo.toml`), so together with `haider-tui` itself they are the
complete blast radius of this change.

### Not executed, and why

`cargo test --workspace --no-fail-fast` was **not run to completion**. Free disk
on the shared host fell to **0 GB** mid-lane and stabilised at **4 GB** —
exactly the stop floor — with **27 GB held by two other lanes' target
directories** (`peermsg-target` 14 GB, `confbench-debug` 13 GB) that the
disk-guard law forbids this lane from touching, and a peer lane (`thinexe`)
actively building. Linking test binaries for the remaining thirteen crates would
have driven the host back to zero and taken the peer's build down with it, so
the builds were stopped rather than forced.

What that leaves uncovered is narrow and stated plainly: the **execution** of
the thirteen crates that do not depend on `haider-tui`. Their test code is
**not** unverified — `cargo clippy --workspace --tests` compiled and linted
every crate's tests against this change and passed with zero errors — and no
source outside `crates/haider-tui` was modified, so there is no path by which
this lane could move them. The orchestrator's landing gate remains the
independent authority.

Windows and Linux: **by inspection** (reducer/state-machine logic, no platform
surface). `xtask loc-lint` warns that `app.rs` exceeds the 10k soft cap — it was
already 19,369 lines at the baseline and this lane adds 204; the warning is
pre-existing and non-blocking.

## Independent verifier value

Two independent reviews ran against the finished diff: a code verifier and a
claim auditor. **Six findings changed the code, the tests or the doc**; four
were rejected with evidence.

Changed:

1. **P1 — a rejected `turn.retract` was dropped, not answered.** The command
   declares `turn_retract_v1` in `command_required_features`, but was missing
   from the allowlist in `link.rs` that turns a missing feature into a typed
   `LiveReply::Failed`. It fell through to a bare `return false`: no reply, no
   retire, so the durable command sat in the outbox forever and was re-sent and
   re-dropped on every reconnect while Esc appeared to do nothing. Reachable on
   a reconnect to a downgraded daemon. `Retract` added to the allowlist; the
   typed failure now reaches the fallback and becomes the plain cancel.
   Regression: `a_daemon_that_drops_the_retract_frame_still_gets_one_cancel`.
2. **P1 — a receipt for a session the user had left destroyed the prompt.** The
   original arm returned early when the receipt's session was not on screen.
   But by then the daemon has committed the retraction, so the transcript row
   AND the history entry are already gone — that early return was the only
   remaining copy being dropped. Now parked on the owning session's draft, the
   same place a surface switch parks it. Regression:
   `a_receipt_for_a_session_the_user_left_parks_the_prompt_instead_of_losing_it`.
3. **P2 — the background twin was not updated.** `SessionState::absorb_raw`
   already mirrors the other `render.ui == false` command-state hooks; it now
   mirrors these two. Without it, a boundary arriving while a session is parked
   left a stale open window (a spurious "too late" round trip on return), and a
   retraction arriving while parked left the prompt in that session's chooser
   for good. Regression: `a_parked_session_records_the_boundary_and_forgets_its_retracted_prompt`.
4. **P2 — the submit guard was unscoped.** `retract_pending` is model-global
   and outlives a surface switch by up to one round trip, so it refused Enter
   on the launcher and blocked menu answers. Now scoped to the session surface
   with no open menu, matching the adjacent B4b upload guard.
5. **P2 — the two raw hooks were not agent-scoped**, unlike the prompt-history
   recorder fifteen lines below, and the daemon stamps `agent_id` on both
   facts. A subagent's first response would have closed the parent's editing
   window, and a subagent retraction would have closed a chooser the user had
   just opened. Both now guarded on `agent_id.is_none()`.
6. **Doc — three false or drifted claims** (a test name missing a word; line
   numbers presented as worktree coordinates when they are baseline ones, which
   this lane's own diff shifts; and an inverted clause calling
   `streamed_output_chars` "wider" than `is_response_delta` when it is strictly
   narrower — the very reason it was rejected). All corrected above.

Rejected with evidence: that the `too_late` fallback could issue two cancels
(the fresh `mint()` cannot re-match `retract_flight`); that a non-retract
command could take the new failure arm (it matches on variant *and* exact id);
that the boundary reset could clobber itself (acceptance commits a non-terminal
`RunState::Queued` first, and the boundary envelope's payload no longer decodes
as `RunState`); and that the new `Failed` arm shadows the arms it precedes
(they key off `pending_workspace_set`, `retired_logins` and `oauth_flight`,
none of which can hold a retract id).

Two accepted limitations, both documented rather than fixed: a
`AttachmentBlock::Skill` cannot be carried back into a chip (identical to the
existing fork-draft behaviour, and counted in the visible notice), and a
retraction whose socket dies is re-sent from the outbox on reconnect while the
guard is already lifted — bounded, idempotent at the daemon, and the late
receipt still restores the text rather than dropping it.
