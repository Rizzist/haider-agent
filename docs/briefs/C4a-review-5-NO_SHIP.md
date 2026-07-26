# C4a review round 5 — NO_SHIP (gpt-5.6, frozen 34abe11)

"Terminal writes are centralized, but ownership and exactly-once behavior are not yet
structural."

### Findings

1. **P1 — A terminal claim can be permanently abandoned by cancellation or panic.**
   `claim_terminal` flips Dispatched→Terminalizing before any async append (broker.rs:374);
   `TerminalClaim::append` awaits the sink (broker.rs:470) with no Drop recovery. Cancel or
   panic there → later writers treat Terminalizing as a successful no-op (broker.rs:381) and
   `close()` neither retries nor reports (broker.rs:618). Normal read completion and
   `journal_unknown` remain caller-owned/cancellable; a racing `journal_unknown` can win,
   die inside append, and no-op the broker-owned finalizer.

2. **P1 — The claim is broker-local, not universal across brokers sharing one journal.**
   Each `BrokerJournal::new` has an independent lifecycle map (broker.rs:251); `new_at`
   permits multiple brokers over cloned handles to one sink (broker.rs:556); effect ids
   derive from caller-supplied identity + broker-local counter (broker.rs:643) → two brokers
   with identical inputs can double-terminalize one shared journal. Needs journal-level
   exclusion or enforced single-broker/fencing precondition.

3. **P1 — Append-failure→Unknown can itself create two durable outcomes.**
   `JournalSink::append` Err does not guarantee nothing committed (broker.rs:52); on Err the
   claim appends a second Unknown (broker.rs:480). Committed-but-unacked first append →
   both outcomes land. The test double fails before recording, so it cannot expose this.

4. **P2 — Unknown escalation does not carry the original error durably** (in-memory unkeyed
   vector only; protocol Unknown has no error field). Replay cannot associate cause.

5. **P2 — The duplicate-claim race test does not force the vulnerable interleaving** —
   nothing forces both writers past a lifecycle precheck before either appends; a
   non-atomic check→append→set could serialize and still pass `== 1`.

### Trace results (reviewer)
Terminal construction centralized (only `TerminalClaim::append` constructs Outcome); all six
tests use exact counts; close() drains its fixed JoinSet, one-shot by `close(self)`; no new
regression in digest binding, fail-closed answers, fd-anchoring, locked-fd identity,
renameat, apply/ledger mechanics. 31 targeted tests + fmt + diff --check passed
(read-only sandbox blocked full clippy run).

VERDICT: NO_SHIP
