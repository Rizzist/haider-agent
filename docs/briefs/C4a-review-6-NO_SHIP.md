# C4a review round 6 — NO_SHIP (gpt-5.6, frozen fe9b6e6)

"Two exactly-once guarantees remain non-structural." Trace confirmed: drain-precedes-sweep,
TerminalJournalFailed terminal/keyed/sweep-excluded/no-reappend, forced race genuinely
forces (check→append→set would fail it), no delta regression in apply/ledger mechanics,
digest binding, fd anchoring, locked-fd identity, renameat, fail-closed answers, JoinSet
drain, one-shot close. Remaining:

1. **P1 — Cancellation or panic after durability can still re-arm a terminal claim.**
   Sink contract only says Err ⇒ nothing durable (broker.rs:58). `TerminalClaim::append`
   stays unsettled until the sink future returns (broker.rs:523) while Drop re-arms any
   unsettled claim (broker.rs:544). A conforming sink may COMMIT, then yield/panic before
   returning; cancellation/unwind re-arms a durable terminal → second outcome possible.
   Needs the stronger law (dropped/unwound incomplete append ⇒ nothing durable) and/or
   uncancellable append dispatch. Reviewer: "Once append actually returns Ok, the
   implementation is safe: mark_outcome runs before settled, with no intervening await."

2. **P1 — `Box<dyn JournalSink>` does not make shared underlying sinks unrepresentable.**
   Open public trait (broker.rs:63); an impl can wrap `Arc<Mutex<…>>` and box two clones
   (the test double demonstrates the pattern, filesystem_tools_tests.rs:30). Two brokers
   via `new_at` with identical identity inputs → identical effect ids, independent claims,
   double outcomes. Underlying exclusivity is not type-enforced; the claim language must
   become an honest implementor contract + constructor discipline.

3. **P2 — Reconciled IDs are discarded whenever close() also reports an error.**
   Sweep successes accumulate (broker.rs:680) but `surface_close_errors(...)?` runs before
   returning the report (broker.rs:690): mixed closes return only Err, losing the
   reconciled ids the report shape promises.

VERDICT: NO_SHIP
