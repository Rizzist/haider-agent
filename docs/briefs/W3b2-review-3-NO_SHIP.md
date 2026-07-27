# W3b2 review round 3 — NO_SHIP (converging)

- Reviewer: gpt-5.6 (codex exec, xhigh), delta review of 584d7de..138c59e, frozen HEAD 138c59e.
- Result: **NO_SHIP** — but P1: none found. All round-2 P1s CLOSED or PARTIAL-with-residual; 5 P2 + 1 P3 remain, all judged localized. W3c seam judged ready without remodeling.
- Mutation audit: 4 documented reverts executed, all failed as claimed, worktree restored byte-identical (hashes verified).
- Gate at frozen SHA: clippy/fmt/test-count PASS; cargo test FAILS reproducibly on the unsynchronized unknown-ID pressure test (finding 5); 20-run hub soak failed run 2 same timeout.

## 1. CLOSURE TABLE

| Round‑2 finding | Status | Old → current evidence |
|---|---|---|
| P1‑1 drain × pending catch-up lag loses suffix | PARTIAL | `584d7de:session_hub.rs:1931,2015,2081` exited when the actor vanished. Current code invokes `final_suffix_resume` from replay, buffered, and live terminal paths at [session_hub.rs:1979](/Users/rizzist/haider-run/haider-w3b2/crates/haider-daemon/src/session_hub.rs:1979), [2076](/Users/rizzist/haider-run/haider-w3b2/crates/haider-daemon/src/session_hub.rs:2076), [2122](/Users/rizzist/haider-run/haider-w3b2/crates/haider-daemon/src/session_hub.rs:2122), and [2169](/Users/rizzist/haider-run/haider-w3b2/crates/haider-daemon/src/session_hub.rs:2169). The original race is closed, but resume/read/final-marker failures are silently discarded. |
| P1‑2 forced-stop fence raised after abort | CLOSED | `584d7de:session_hub.rs:394-397,1002-1007` relied on reverse local-drop order. [OwnedTasks::drop](/Users/rizzist/haider-run/haider-w3b2/crates/haider-daemon/src/session_hub.rs:415) now performs the Release fence store at line 422 before every abort at line 424 in the same body. |
| P1‑3 detached `Lagged` recreates ownerless lane | CLOSED | `584d7de:connection.rs:439-445` keyed `Lagged` by attachment after purge. Current routing makes only `Event`/`AttachCaughtUp` attachment-keyed at [connection.rs:620](/Users/rizzist/haider-run/haider-w3b2/crates/haider-daemon/src/connection.rs:620); `Lagged` uses System. Ownership-locked admission at [session_hub.rs:991](/Users/rizzist/haider-run/haider-w3b2/crates/haider-daemon/src/session_hub.rs:991) prevents all post-purge keyed enqueue. |
| P2‑4 `Lagged` precedes/replaces staged attach response | CLOSED | `584d7de:connection.rs:409-421` staged the response in the attachment lane and purge returned no request ID. Current response is System-lane tagged at [connection.rs:576](/Users/rizzist/haider-run/haider-w3b2/crates/haider-daemon/src/connection.rs:576); purge returns the request ID at [connection.rs:453](/Users/rizzist/haider-run/haider-w3b2/crates/haider-daemon/src/connection.rs:453), and [lag_and_detach](/Users/rizzist/haider-run/haider-w3b2/crates/haider-daemon/src/session_hub.rs:2510) sends the correlated error instead. |
| P2‑5 Busy admission has no starvation discipline | PARTIAL | `584d7de:connection.rs:267-275` used a broadcast wake. FIFO oneshots now exist at [connection.rs:185](/Users/rizzist/haider-run/haider-w3b2/crates/haider-daemon/src/connection.rs:185), but firing a ticket grants no reservation and fresh offers can barge at [session_hub.rs:2241](/Users/rizzist/haider-run/haider-w3b2/crates/haider-daemon/src/session_hub.rs:2241). Starvation remains possible. |
| P2‑6 ¼ reply headroom cannot protect a legal large reply | CLOSED | `584d7de:connection.rs:222-249` imposed the ¾ event share. The one-frame `L+4` reply floor is implemented at [connection.rs:347](/Users/rizzist/haider-run/haider-w3b2/crates/haider-daemon/src/connection.rs:347) and priority-popped at [connection.rs:397](/Users/rizzist/haider-run/haider-w3b2/crates/haider-daemon/src/connection.rs:397). A maximum reply fits when event traffic alone camps ordinary capacity. |
| P2‑7 valid configurations exceeded the stated byte ceiling | PARTIAL | `584d7de:connection.rs:243-264` admitted one over-budget empty-queue event; that exception is removed and ordinary charges cannot exceed `B`. However [config.rs:21](/Users/rizzist/haider-run/haider-w3b2/crates/haider-daemon/src/config.rs:21) still documents the removed split/1 GiB ceiling, and validation at line 109 permits `B=L`, which cannot admit an encoded `L+4` Event. |
| P2‑8 catch-up aggregate was not a hard bound | PARTIAL | `584d7de:session_hub.rs:1818-1828` admitted an oversized envelope into an empty channel. Current [publish](/Users/rizzist/haider-run/haider-w3b2/crates/haider-daemon/src/session_hub.rs:1847) always takes the lag/store-resume path. But the claimed physical 2 GiB ceiling remains false because its estimator omits unbounded owned fields and JSON allocation overhead. |
| P3‑9 uncancellable-store exception called read-only | CLOSED | `584d7de:session_hub.rs:994-996` named only reads. Current shutdown law at [session_hub.rs:1022](/Users/rizzist/haider-run/haider-w3b2/crates/haider-daemon/src/session_hub.rs:1022) explicitly names read, append, and menu CAS and documents forced-path commit-without-publication recovery. |

## 2. STRUCTURAL VERIFICATION

### (a) Ownerless-lane class

The ~154k detached-lane state is unrepresentable.

Every production attachment frame goes through `deliver_frame → offer_attachment → FrameSink::offer`. `offer_attachment` holds the attachment-owner mutex while admitting; detach removes ownership under that same mutex before purging. Therefore:

- enqueue first → detach waits, then purge removes it;
- detach first → subsequent offer sees no owner and is refused;
- `lag_and_detach` emits only System-lane `Response` or `Lagged`.

No code path enqueues an attachment-keyed frame after purge.

### (b) Reply floor and exact bound

The encoded-payload ceiling is:

```text
B + (L + 4) reply floor + (L + 4) drain notice
= B + 2(L + 4)
```

At defaults:

```text
16,777,216 + 2 × 8,388,612 = 33,554,440 bytes/connection
64 connections = 2,147,484,160 bytes
```

Ordinary accounting includes the in-flight frame until write settlement. The floor remains occupied through its write. The drain notice is a separate `mpsc(1)` reservation. Attachment traffic answers `Busy`; another reply while ordinary and floor are both occupied is terminal. Nothing else can queue in this outbox.

Thus one maximum `SessionRead` reply is always admissible against event-only camping when the floor is free. The implementation does not guarantee a second pipelined maximum reply while the first floor reply remains in flight.

### (c) Oversized-envelope removal

The catch-up empty-channel exception is gone. `publish` checks the budget before its only `events.try_send`; an oversized envelope marks lag and is retrieved through `read_page`.

`read_page` returns at least one row even when that row exceeds its page budget. `deliver_event` advances `last_sent_seq` only after sink admission, while re-registration captures the already-advanced durable actor head. Consequently the oversized sequence either advances exactly once or sink refusal detaches; it cannot self-loop.

The control-flow ceiling is hard in estimator units, but not in actual memory. `envelope_weight_bytes` counts only fixed overhead, event ID, session ID, and payload. It omits unbounded branch/run/agent/device/causation/correlation strings, and the one-row `read_page` progress exception also makes the documented 256 MiB aggregate replay-page transient non-hard.

### (d) `final_suffix_resume`

All normal actor-gone + pending-lag terminal shapes now call it, and actor shutdown precedes replay-task joining, so its captured durable head is stable. Runtime wraps hub shutdown in the one barrier deadline; a stall becomes `Forced`.

It can nevertheless lose the graceful-broadcast outcome silently:

- `latest_seq` error returns;
- replay read error/empty page becomes `Cancelled` and returns;
- failure to enqueue final `AttachCaughtUp` is ignored;
- replay handles return `()` and shutdown reports success after joining them.

Durability remains recoverable by R9, but “delivered here or Forced, never silent” is not true.

### (e) Fence before abort

The declaration-order defect is closed. The fence store and handle aborts are in one `Drop` body in the required order; field destruction cannot reverse them.

The claimed abort-vs-recv microrace is honestly not black-box discriminable. The actor’s Acquire check is immediately after receive, and a command that linearized before the Release falls into the already-started uncancellable-operation residual. I accept that test limitation.

### (f) FIFO tickets

FIFO notification order is real; FIFO service and starvation freedom are not.

At capacity one:

1. A’s head ticket fires.
2. Before A runs, a fresh B offer fills the freed capacity.
3. A reoffers, receives `Busy`, and queues at the tail.
4. The next cycle repeats with C.

There is no reservation or no-barging check between ticket firing and admission. The existing ticket test proves only that receivers fire in order.

### Deliberate test calls

The hub seam is a legitimate location for live-commit pressure because v0.1 exposes no append RPC and real commits enter through workers. The current `BoundedReaderSink`, however, is not faithful to the real outbox’s critical timing: it releases frame and byte capacity atomically on pop, while the real outbox frees the frame slot on pop and bytes later on write credit. That difference hides the FIFO/starvation defect. The boundary is acceptable; the claimed faithful model is not.

The lane-staging test replacement is legitimate and stronger. Exactly one test was deleted: `attach_response_is_staged_in_the_attachment_lane_before_its_first_event`. The new in-outbox test camps an earlier System reply and proves an event remains `Busy` until the specifically tagged attach response pops. No ordering coverage was lost.

### Regression and W3c readiness

INV‑1, INV‑2, R9, R13, and R14 still hold. R12’s store-resume control flow holds, but its fairness and physical resource-bound claims do not.

Test accounting is honest: eight tests added, one mechanism-specific test replaced, net `516 → 523`; `xtask test-count` reports 523/523.

Gate results:

- `cargo clippy --workspace --all-targets -- -D warnings`: pass
- `cargo fmt --all -- --check`: pass
- `cargo run -p xtask -- test-count`: pass
- `cargo test --workspace`: fail, reproducibly, on the unsynchronized unknown-ID test
- 20-run hub-suite soak: failed on run 2 with the same timeout

The W3c APIs do not require remodeling: [SessionHub::open_connection](/Users/rizzist/haider-run/haider-w3b2/crates/haider-daemon/src/session_hub.rs:622) already returns `HubConnection`, and [SessionHub’s StoreHandle implementation](/Users/rizzist/haider-run/haider-w3b2/crates/haider-daemon/src/session_hub.rs:1086) already routes worker appends through the actor. W3c still must inject the hub as the live workers’ `StoreHandle`; the blockers below are localized fixes.

## 3. MUTATION-CHECK AUDIT

I executed four documented reversions:

1. Routed `Lagged` back to the attachment lane: zero-residual-lanes test failed at `connection_tests.rs:117`.
2. Disabled reply-floor fallback: reply-floor test failed at `connection_tests.rs:182`.
3. Fired newest ticket first: FIFO test timed out at `connection_tests.rs:208`.
4. Removed live closed-channel `final_suffix_resume`: drain-suffix test timed out at `session_hub_tests.rs:111`.

Every mutation was reversed. Final hashes match the pre-mutation hashes:

```text
connection.rs  9ed7f1ab738498256c189ddbc58fccb151e044924071a2dc8de04a8951e9ec91
session_hub.rs 314cf999fbc4b532bc9392ab5d3ad4a22369f4c5f52c4d787feed2bff3af7b7a
```

Final state: `HEAD=138c59e8…`, empty porcelain status, `git diff --exit-code` clean, stash list empty.

## 4. NEW FINDINGS

P1: none found.

1. **P2 — FIFO tickets permit barging and starvation.** [connection.rs:197](/Users/rizzist/haider-run/haider-w3b2/crates/haider-daemon/src/connection.rs:197), [session_hub.rs:2237](/Users/rizzist/haider-run/haider-w3b2/crates/haider-daemon/src/session_hub.rs:2237). Fired tickets grant no capacity reservation; fresh hot offers can repeatedly consume each unit before a cold waiter runs.

2. **P2 — Valid `B=L` configuration permanently parks a legal maximum Event.** [config.rs:103](/Users/rizzist/haider-run/haider-w3b2/crates/haider-daemon/src/config.rs:103), [connection.rs:297](/Users/rizzist/haider-run/haider-w3b2/crates/haider-daemon/src/connection.rs:297). Validation ignores the four-byte UDS prefix. An encoded Event of `L+1..L+4` is legal but always `Busy`; with an empty outbox, no ticket can ever fire.

3. **P2 — The 2 GiB catch-up and 256 MiB replay-transient physical ceilings are false.** [session_hub.rs:114](/Users/rizzist/haider-run/haider-w3b2/crates/haider-daemon/src/session_hub.rs:114), [session_hub.rs:1897](/Users/rizzist/haider-run/haider-w3b2/crates/haider-daemon/src/session_hub.rs:1897), [envelope.rs:37](/Users/rizzist/haider-run/haider-w3b2/crates/haider-protocol/src/envelope.rs:37). Unbounded omitted ID strings can be cloned while charged only hundreds of bytes; `read_page` deliberately materializes one arbitrarily over-budget row.

4. **P2 — Final-suffix failures are downgraded to graceful shutdown.** [session_hub.rs:2475](/Users/rizzist/haider-run/haider-w3b2/crates/haider-daemon/src/session_hub.rs:2475), [session_hub.rs:2491](/Users/rizzist/haider-run/haider-w3b2/crates/haider-daemon/src/session_hub.rs:2491), [session_hub.rs:1063](/Users/rizzist/haider-run/haider-w3b2/crates/haider-daemon/src/session_hub.rs:1063). Store/read/final-marker failures return `()` and are joined as success, contradicting the promised delivered-or-Forced outcome.

5. **P2 — The frozen full gate is flaky/failing.** [session_hub_tests.rs:2270](/Users/rizzist/haider-run/haider-w3b2/crates/haider-daemon/tests/session_hub_tests.rs:2270). The pressure append is not synchronized with the replay parking. If it wins, replay re-registers at head 2 and then parks forever with no later lag, timing out at line 111.

6. **P3 — Public configuration documentation describes removed mechanics and the wrong aggregate ceiling.** [config.rs:21](/Users/rizzist/haider-run/haider-w3b2/crates/haider-daemon/src/config.rs:21). It still states class-split admission, a ¾ event share, and a 1 GiB aggregate while the real maximum includes the reply floor and drain notice.

## 5. VERDICT

VERDICT: NO_SHIP
