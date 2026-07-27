# W3b2 review round 4 — SHIP_WITH_FIXES

- Reviewer: gpt-5.6 (codex exec, xhigh), delta review 138c59e..4af9456, frozen HEAD 4af9456.
- Result: **SHIP_WITH_FIXES** — all six round-3 findings CLOSED; P1 none; 1 P2 (dead head-ticket holder wedges attachment admission: abort-before-reoffer leaves pruned queue with unfired successor), 1 P3 (combined-pressure sink test does not discriminate pre-fix barging — passed 25/25 with the gate removed).
- Required fixes (W3b2.6): cancellation-safe RAII ticket cleanup (dropped head removed + successor fired) + deterministic abort-before-reoffer progress test; discriminating fired-head/fresh-offer barging test through BoundedReaderSink; full UDS gate rerun in a socket-capable environment (reviewer sandbox denied binds — 31 lifecycle + 5 session-RPC cases environmental only).
- Invariants: INV-1/INV-2/R9/R13/R14 HOLD; R12 PARTIAL only via the dead-head wedge. Mutation audit 4/4 executed, restored byte-identical. Baseline honest 523→528.
- W3c: API ready, no remodeling; hand live workers SessionHub as StoreHandle; quiescent stuck-client residual stays ledgered with W3c as policy trigger.

Frozen HEAD verified: `4af9456cc43c25d0d48f257215717a820dabfc58`.

## 1. Closure table

| Round-3 finding | Status | Old → current evidence |
|---|---|---|
| P2 FIFO tickets permit barging/starvation | **CLOSED for the original barging schedule** | `138c59e:connection.rs:283-317` admitted fresh offers without considering waiters; `session_hub.rs:2237-2256` reoffered without identity. Current admission checks capacity and `(queue empty OR caller is head)` under one mutex and consumes the head atomically at [connection.rs:309](/Users/rizzist/haider-run/haider-w3b2/crates/haider-daemon/src/connection.rs:309); one token is retained across reoffers at [session_hub.rs:2338](/Users/rizzist/haider-run/haider-w3b2/crates/haider-daemon/src/session_hub.rs:2338). A new holder-death risk remains below. |
| P2 `B=L` permits an unsendable `L+4` Event | **CLOSED** | `138c59e:config.rs:109-114` required only `B>=L`. Current validation computes checked `L+4` and rejects anything smaller at [config.rs:109](/Users/rizzist/haider-run/haider-w3b2/crates/haider-daemon/src/config.rs:109). The boundary test derives `L` from an actual Event, rejects `L+3`, accepts `L+4`, and admits it at [connection_tests.rs:328](/Users/rizzist/haider-run/haider-w3b2/crates/haider-daemon/src/connection_tests.rs:328). |
| P2 estimator omitted owned fields and claimed false physical ceilings | **CLOSED in the documented true-weight unit** | `138c59e:session_hub.rs:1897-1905` charged only event/session IDs and payload. The exhaustive, no-`..` destructure at [envelope.rs:77](/Users/rizzist/haider-run/haider-w3b2/crates/haider-protocol/src/envelope.rs:77) makes a new envelope field a compile error and charges event, session, branch, run, agent, device, causation, correlation, and recursive payload storage. Catch-up and replay bounds now explicitly include the one-row transient at [session_hub.rs:112](/Users/rizzist/haider-run/haider-w3b2/crates/haider-daemon/src/session_hub.rs:112) and [event_store.rs:305](/Users/rizzist/haider-run/haider-w3b2/crates/haider-store/src/event_store.rs:305). |
| P2 final-suffix failures reported graceful | **CLOSED** | `138c59e:session_hub.rs:2466-2499` silently returned on `latest_seq`, replay/read, and final-marker failures. Current [final_suffix_resume](/Users/rizzist/haider-run/haider-w3b2/crates/haider-daemon/src/session_hub.rs:2563) maps every outcome into `FinalSuffixFailure`; all callers return `ReplayCompletion::FinalSuffixFailed`, shutdown changes the result to `Forced` at [session_hub.rs:1141](/Users/rizzist/haider-run/haider-w3b2/crates/haider-daemon/src/session_hub.rs:1141), and runtime propagates it at [runtime.rs:293](/Users/rizzist/haider-run/haider-w3b2/crates/haider-daemon/src/runtime.rs:293). |
| P2 flaky unknown-ID pressure test/full gate | **CLOSED** | `138c59e:session_hub_tests.rs:2295-2302` appended immediately after attach. Current sink marks only a confirmed ticketed `Busy` at [session_hub_tests.rs:2281](/Users/rizzist/haider-run/haider-w3b2/crates/haider-daemon/tests/session_hub_tests.rs:2281); the test awaits it before appending at lines 2340-2352. No sleeps. Directed soak passed 25/25; full hub suite passed 15/15. |
| P3 config docs describe removed mechanics/wrong ceiling | **CLOSED** | `138c59e:config.rs:21-41` described class splitting, a ¾ event share, and a 1 GiB ceiling. Current [config.rs:21](/Users/rizzist/haider-run/haider-w3b2/crates/haider-daemon/src/config.rs:21) states unified admission, FIFO attachment service, reply floor, drain reservation, `B>=L+4`, and exact ceiling `B + 2(L+4)`, including correct default totals. |

## 2. No-barging attack report

### (a) Head-token admission

Holds. Queue eligibility, frame/lane/byte capacity, head verification, head consumption, and enqueue all occur under the single outbox lock at [connection.rs:315](/Users/rizzist/haider-run/haider-w3b2/crates/haider-daemon/src/connection.rs:315).

Tokens use `Arc` pointer identity against weak queue entries at [connection.rs:192](/Users/rizzist/haider-run/haider-w3b2/crates/haider-daemon/src/connection.rs:192). The production outbox is private, and no later offer can obtain another waiter’s token. No duplication, forgery, or ABA path was found.

### (b) Ticket-holder death

Orderly paths are correct:

- Ticketed refusal, detach/connection cancellation, and lag-under-stall call `cancel_ticket` at [session_hub.rs:2347](/Users/rizzist/haider-run/haider-w3b2/crates/haider-daemon/src/session_hub.rs:2347).
- Removing a head fires its successor at [connection.rs:387](/Users/rizzist/haider-run/haider-w3b2/crates/haider-daemon/src/connection.rs:387).
- Connection close wakes every live ticket at [connection.rs:536](/Users/rizzist/haider-run/haider-w3b2/crates/haider-daemon/src/connection.rs:536).

Raw replay-task abort/drop is not cancellation-safe. `deliver_frame` has no RAII guard; dropping the strong token leaves a dead weak head. Pruning removes it but does not fire the newly exposed successor.

### (c) Overall progress

Normal progress is sound:

- Head admission consumes its token and fires the next head if capacity remains.
- Pop frees the frame slot and fires the head at [connection.rs:450](/Users/rizzist/haider-run/haider-w3b2/crates/haider-daemon/src/connection.rs:450).
- Write credit later frees bytes and fires again at [connection.rs:491](/Users/rizzist/haider-run/haider-w3b2/crates/haider-daemon/src/connection.rs:491).
- Purge also fires after releasing capacity.

The holder-death sequence creates the forbidden state: capacity free, queue non-empty, and no live ticket fired.

### (d) Reserved classes

Holds.

Replies do not consult attachment tickets: they try ordinary admission and then the reserved floor at [connection.rs:400](/Users/rizzist/haider-run/haider-w3b2/crates/haider-daemon/src/connection.rs:400). The floor priority-pops at line 458, and `finish_pop` fires attachment tickets.

`ServerDraining` uses a separate `mpsc(1)` reservation outside ordinary accounting at [connection.rs:780](/Users/rizzist/haider-run/haider-w3b2/crates/haider-daemon/src/connection.rs:780).

### BoundedReaderSink fidelity

The timing is now faithful: [session_hub_tests.rs:2748](/Users/rizzist/haider-run/haider-w3b2/crates/haider-daemon/tests/session_hub_tests.rs:2748) pops and frees the frame slot, yields while bytes remain charged, then credits bytes.

The decisive discrimination check fails: with the head-token gate removed from `BoundedReaderSink`, the combined-pressure test still passed 25/25. The direct production barging test fails correctly, but the combined sink test does not force the fired-head/fresh-offer schedule.

## 3. Mutation-check audit

| Mutation | Result |
|---|---|
| Removed the no-barging gate from production and `BoundedReaderSink` | Direct barging test failed at `connection_tests.rs:276`; combined-pressure test incorrectly passed 25/25. |
| Reverted validation from `B>=L+4` to `B>=L` | Boundary test failed because `L+3` was accepted. |
| Removed the `branch_id` estimator charge | Large-owned-ID test failed. |
| Downgraded `FinalSuffixFailed` to graceful | Forced-outcome test failed, observing `Graceful` instead of `Forced`. |

All mutations were reversed byte-identically. Final hashes match the pre-mutation values, `git status --porcelain` and `git diff --exit-code` are clean, and the stash list is empty.

Regression evidence:

- INV-1, INV-2, R9, R13, and R14 hold.
- R12’s normal bounded/store-resume and round-robin behavior holds, but unconditional cancellation-safe progress is **PARTIAL** because of the dead-head wedge.
- Baseline is honestly `523→528`; no test function or test annotation was deleted.
- Clippy `-D warnings`, formatting, test-count `528/528`, all non-`haider-daemond` workspace tests, directed soak 25/25, and hub-suite soak 15/15 passed.
- This sandbox denied Unix socket binds with `EPERM`: 31 lifecycle and five session-RPC cases could not execute. Their failures were environmental at bind/readiness, not assertion regressions. The complete UDS gate must be rerun in a socket-capable environment.

## 4. New findings

P1: none found.

1. **P2 — dead head-ticket holder can wedge all attachment admission on the connection.** [connection.rs:197](/Users/rizzist/haider-run/haider-w3b2/crates/haider-daemon/src/connection.rs:197), [connection.rs:322](/Users/rizzist/haider-run/haider-w3b2/crates/haider-daemon/src/connection.rs:322), [session_hub.rs:2338](/Users/rizzist/haider-run/haider-w3b2/crates/haider-daemon/src/session_hub.rs:2338). Capacity one; A and B wait; pop fires A; A is aborted before reoffer/cancellation; a fresh C prunes dead A but parks behind unfired B. With capacity already free, no later pop/credit exists, so B and C wait indefinitely.

2. **P3 — combined-pressure sink test does not discriminate pre-fix barging.** [session_hub_tests.rs:2715](/Users/rizzist/haider-run/haider-w3b2/crates/haider-daemon/tests/session_hub_tests.rs:2715), [session_hub_tests.rs:2830](/Users/rizzist/haider-run/haider-w3b2/crates/haider-daemon/tests/session_hub_tests.rs:2830). Removing the sink’s head-token gate passed the test 25/25.

Before W3c puts real clients on this:

- Add cancellation-safe RAII ticket cleanup that removes a dropped head and fires its successor; add a deterministic abort-before-reoffer progress test.
- Add a deterministic fired-head/fresh-offer mutation test through `BoundedReaderSink`.
- Rerun the complete UDS gate where socket binds are permitted.
- W3c must hand live workers `SessionHub` as their `StoreHandle`; the API is ready and needs no remodeling.
- The ledgered quiescent stuck-client/heartbeat residual remains bounded and may stay as the documented v0.1 availability limit, but W3c is its stated policy trigger.

## 5. Verdict

Required fixes are the cancellation-safe ticket guard plus its death test, and a discriminating `BoundedReaderSink` barging test; then rerun the socket-capable full gate.

VERDICT: SHIP_WITH_FIXES
