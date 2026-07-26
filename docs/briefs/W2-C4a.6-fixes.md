# Patch brief W2a/C4a.6 — round-7: close the last two windows, say honest words

Worktree /Users/rizzist/Documents/CODING/haider-agent-c4a, branch w2-c4a. Round-6 findings
in docs/briefs/C4a-review-6-NO_SHIP.md (2×P1 narrow windows + 1×P2). Everything else was
trace-confirmed — do NOT restructure what the reviewer verified. Three surgical changes:

1. Uncancellable append dispatch + the stronger sink law (F1). Two layers:
   a. `TerminalClaim::append` must not be severable between sink-commit and settle: run
      the sink append + mark_outcome + settle as one unit on a broker-owned task (the
      existing finalizer JoinSet) or spawn_blocking, so caller cancellation cannot drop
      it mid-flight. Drop re-arm then only covers the pre-dispatch window, where nothing
      can be durable yet.
   b. Strengthen the `JournalSink::append` contract doc to the full law: an append future
      dropped or unwound before returning MUST NOT have committed — durability must be
      the final act before Ready. Our SQLite sink satisfies this naturally (blocking
      transactional commit); test doubles must too (audit them; the barrier double in
      filesystem_tools_tests.rs must not commit-then-yield).
   Test: a sink double that commits then yields once before returning is now ILLEGAL —
   instead test that caller cancellation racing a dispatched append cannot produce two
   outcomes: cancel the caller during the in-flight append and assert exactly one
   terminal, append still completes (broker-owned), no re-arm of a durable terminal.

2. Honest exclusivity contract (F2). Stop claiming type-level unrepresentability. State
   the real boundary in the module + trait docs: a `JournalSink` VALUE must be the sole
   handle to its underlying journal (implementor contract, like Send/Sync laws);
   constructing two sinks over one journal is a protocol violation whose durable defense
   is the store's single-writer worker_generation fencing (stale-generation commits
   rejected) — name that seam. Constructor discipline: production construction consumes
   the store handle by value. Update the test doubles' comments to state they honor the
   sole-handle law (the Arc inside a double is an implementation detail of ONE sink
   value, never boxed twice — assert with a debug guard where cheap, e.g. a taken-flag).

3. close() report keeps both halves (F3). Make close() return its report in BOTH cases:
   either `CloseReport { reconciled: Vec<EffectId>, errors: Vec<(EffectId, ...)> }` as a
   plain return (callers inspect errors), or `Result<CloseReport, CloseError>` where the
   error carries the full report including reconciled ids. Mixed-close test: one effect
   reconciles, one finalizer fails → caller can see BOTH.

Gate: cargo test -p haider-tools, workspace clippy -D warnings (all targets), cargo fmt
--all -- --check, cargo run -p xtask -- test-count --update, git diff --check.
Leave changes uncommitted.
