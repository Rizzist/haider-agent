# Patch brief W2a/C4a.5 — round-6: make exactly-once structural, by subtraction

Worktree /Users/rizzist/Documents/CODING/haider-agent-c4a, branch w2-c4a. Round-5 findings
in docs/briefs/C4a-review-5-NO_SHIP.md (3×P1, 2×P2). The fix SIMPLIFIES: one escalation
path is deleted outright. Do not add new shields.

1. Abandoned claims recover (r5 F1). `TerminalClaim` gains Drop recovery: if the claim is
   dropped before its append completed (cancellation or panic), Drop reverts the lifecycle
   Terminalizing → Dispatched so a later writer (finalizer, `close()`) can re-claim.
   `close()` gains a SWEEP after draining finalizers: any effect still Dispatched or
   Terminalizing gets one final claim + `Unknown` append (orderly-shutdown reconciliation),
   and appears in the close report. Result: while the process lives, no effect ends
   claimed-but-silent — abandonment is always re-armed or swept.

2. Sink exclusivity is type-level (r5 F2). The broker takes EXCLUSIVE ownership of its
   journal sink: constructor consumes `Box<dyn JournalSink>` (no Arc, no Clone, no shared
   handle constructor). Remove/privatize any path that lets two brokers share one sink.
   Two-brokers-one-journal becomes unrepresentable in safe code. Cross-PROCESS exclusion is
   the store's single-writer law (worker_generation fencing at commit) — state that in the
   module contract header as the delegated boundary; do not reimplement it here.

3. Delete the Unknown-escalation double append (r5 F3+F4). Amend the `JournalSink` trait
   contract: `append` is TRANSACTIONAL — `Err` means nothing durable was committed. This is
   an obligation on implementors (the real sink is the SQLite event store's transactional
   commit; test doubles must honor it — fix the failing double to fail BEFORE committing,
   which it already does, and document the law on the trait). On append `Err`, the claim
   winner does NOT append anything else: it records the error in a PER-EFFECT-KEYED
   terminal-error map, marks the lifecycle `TerminalJournalFailed`, and `close()` surfaces
   the keyed errors. Durable recovery for that window is the already-durable Dispatched
   phase + the named startup-reconciliation seam (EffectOutcomeUnknown at reopen). The
   protocol Unknown variant stays reserved for reconciliation, not transport errors.

4. Force the race in the duplicate-claim test (r5 F5). Instrument the sink so the FIRST
   append blocks until the SECOND writer has attempted (and lost) the claim — i.e. both
   writers demonstrably pass their precheck window before any append lands. Assert: the
   loser's claim attempt returns None/no-op, exactly one Outcome phase exists, and the
   winner's append completes after the loser lost. A serialized non-atomic implementation
   must FAIL this test.

5. New/updated tests (tests/ files, exact counts as before):
   a. Claim abandoned by cancellation → lifecycle back to Dispatched; a finalizer then
      claims and journals; exactly one Outcome.
   b. Claim abandoned + nothing else running → close() sweep journals Unknown; close
      report names the effect; exactly one terminal phase.
   c. Append failure → NO second append; keyed error in close() report; journal contains
      the phases up to Dispatched only.
   d. Forced-interleaving duplicate-claim race (per item 4).
   e. Keep all round-4 arms green and counts exact.

Gate: cargo test -p haider-tools, workspace clippy -D warnings (all targets), cargo fmt
--all -- --check, cargo run -p xtask -- test-count --update, git diff --check.
Leave changes uncommitted.
