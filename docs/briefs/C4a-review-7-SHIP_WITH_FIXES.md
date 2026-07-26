# C4a review round 7 — SHIP_WITH_FIXES (gpt-5.6, frozen 9f05f6c)

Fix completed same-commit: broker docs reworded to identity fencing + single-opener
exclusion (no commit-time stale-generation rejection claim).

codex
One documentation fix remains; the three runtime fixes themselves trace correctly.

Findings:

- **P2 — [broker.rs:22](/Users/rizzist/Documents/CODING/haider-agent-c4a/crates/haider-tools/src/broker.rs:22)** and [broker.rs:64](/Users/rizzist/Documents/CODING/haider-agent-c4a/crates/haider-tools/src/broker.rs:64) say `worker_generation` fencing rejects stale-generation commits. The current `EventStore::append` never compares an envelope’s generation with the store generation; cross-process exclusion is provided by the profile OS lock. Reword this as identity fencing plus single-opener exclusion, or add actual commit-time validation.

Verified:

- `dispatch_terminal()` claims and registers synchronously, with no cancellation point between them. The claim moves into the `JoinSet` task; `dispatched` prevents Drop re-arm after its first poll.
- The oneshot returns the result to a live caller; after caller cancellation, `close()` consumes it through `drain_finalizers()` before sweeping. No double-execution gap exists.
- The cancellation test cancels after the sink’s pre-commit gate is reached and proves one attempt, one completion, and one terminal outcome. Round 6 would make two attempts and fail it.
- Shared test doubles enforce sole-sink use with debug taken-flags, and all gates occur before recording. The contract honestly disclaims type-level unrepresentability.
- `EffectBrokerCloseError` retains the report on every error return. The mixed-close test asserts both the reconciled effect ID and the finalizer error.
- Drain-before-sweep, terminal-journal-failure terminality, forced-race coverage, apply/ledger mechanics, digest binding, FD discipline, fail-closed answers, JoinSet draining, and consuming close remain intact.
- Baseline 107 is legitimate: one test added, none deleted. Formatting, compile-only tests, `diff --check`, and test-count pass. Runtime tempfile tests could not execute under the read-only sandbox; non-tempfile broker tests passed.

VERDICT: SHIP_WITH_FIXES
hook: Stop
hook: Stop Completed
tokens used
152,751
One documentation fix remains; the three runtime fixes themselves trace correctly.

Findings:

- **P2 — [broker.rs:22](/Users/rizzist/Documents/CODING/haider-agent-c4a/crates/haider-tools/src/broker.rs:22)** and [broker.rs:64](/Users/rizzist/Documents/CODING/haider-agent-c4a/crates/haider-tools/src/broker.rs:64) say `worker_generation` fencing rejects stale-generation commits. The current `EventStore::append` never compares an envelope’s generation with the store generation; cross-process exclusion is provided by the profile OS lock. Reword this as identity fencing plus single-opener exclusion, or add actual commit-time validation.

Verified:
