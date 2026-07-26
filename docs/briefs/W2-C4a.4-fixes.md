# Patch brief W2a/C4a.4 — round-5: terminalization ownership

Worktree /Users/rizzist/Documents/CODING/haider-agent-c4a, branch w2-c4a. Round-4 review
(docs/briefs/C4a-review-4-NO_SHIP.md) found the blocker is OWNERSHIP of terminalization,
not apply mechanics: (P1a) the detached finalizer dies with the runtime and its errors go
unobserved after caller cancellation; (P1b) check→append→set is three separate ops, so
finalizer / journal_unknown / reconciliation can race into duplicate or competing terminal
phases; (P2) the regression test asserts one arm only and uses `find`, not exactly-one.

Do NOT add another shield. Restructure so the properties are structural:

1. Atomic terminalization claim. Add a single lifecycle transition, e.g.
   `claim_terminal(effect_id) -> Option<TerminalClaim>`, that atomically moves
   Dispatched → Terminalizing under the one lifecycle lock and can succeed EXACTLY ONCE per
   effect. Every terminal writer — normal completion, the finalizer, `journal_unknown`,
   any future reconciliation — must first win the claim; losers are no-ops by construction.
   The claim winner appends the terminal phase and marks Outcome. One-outcome-per-effect
   stops being a convention and becomes unrepresentable to violate.

2. Broker-owned finalizers + drain-on-close. The apply finalizer must not be a free
   `tokio::spawn`: register it in a broker-owned set (JoinSet or handles under the state
   lock). Add async `EffectBroker::close()` that drains every pending finalizer to
   completion and surfaces their errors (same pattern as SqliteStoreHandle::close()).
   Contract header: orderly shutdown NEVER drops a finalizer; process death is covered by
   the durable Dispatched phase + startup reconciliation journaling EffectOutcomeUnknown
   for dispatched-without-terminal effects (W3 crash-recovery seam — name it, don't build
   it). The runtime obligation is: never silently drop while alive, drain on close.

3. Outcome-append failure is never silent. If the claim winner's terminal append fails,
   it still owns the claim: attempt one Unknown-escalation append carrying the original
   error; if that also fails, record the error in a broker terminal-error slot that
   `close()` returns/surfaces. Exactly one terminal phase may ever land.

4. Tests (tests/ files only; assert EXACTLY-ONE terminal phase by counting, never `find`):
   a. Cancel before the blocking worker starts → no rename, no ledger entry, no terminal
      phase, clean cancel.
   b. Round-3 schedule (keep existing): rename ∧ ledger failure → exactly one Failed
      terminal phase carrying the ledger error.
   c. Cancel after worker completion → exactly one terminal phase, no duplicates.
   d. Duplicate-claim race: drive the finalizer and `journal_unknown` at the same effect
      with a barrier → journal holds EXACTLY ONE terminal phase; the loser is a no-op.
   e. Drain-on-close: start an apply whose ledger blocks-then-fails, cancel the caller,
      call `broker.close()` → close returns only after the finalizer ran; exactly one
      terminal outcome exists. (In-process proxy for runtime shutdown; the true
      kill-mid-worker window is the documented reconciliation contract.)
   f. Outcome-append failure: rename ok, terminal append fails → the Unknown escalation
      is appended instead; exactly one terminal phase; the error surfaces at close().

Note: round-4 P3 (review-artifact hygiene) is already fixed in the freeze commit — do not
touch docs/briefs/*.

Gate: cargo test -p haider-tools, workspace clippy -D warnings (all targets), cargo fmt
--all -- --check, cargo run -p xtask -- test-count --update, git diff --check.
Leave changes uncommitted.
