# W3b2 review round 1 — NO_SHIP (dual review: gpt correctness + Fable design)

- Frozen SHA: e2aee54 (scope f8d23f3..e2aee54: 678f833 hub + 1ae32b4 clean-code + e2aee54 efficiency)
- gpt-5.6 correctness: NO_SHIP (3 P1, 4 P2, 1 P3). ALL SIX INVARIANTS HOLD (§5.5 INV-1/INV-2,
  R9 seq-only, R12 store-is-lag-buffer, R13 CAS, R14 capability centralization).
- Fable design: FIX_IN_MERGE (0 D1, 5 D2, 8 D3). Verdicts CONVERGED: gpt P1-2 ≡ Fable D2-4
  (capacity snapshot overbooks at ≥3 lanes) — found independently by both lenses.
- Process: the parallel reviews shared one worktree and Fable observed gpt's live mutation-check
  mid-flight (final state verified clean) — future mutating reviews get their own worktree.
- Full logs: ~/haider-run/w3b2-review-r1.log + the Fable report (task output).

## Fix round W3b2.3

P1 (gpt)
1. Pacing ignores the BYTE quota: capacity_for reports frame slots only while try_push rejects
   on bytes → several large replay envelopes fill the byte budget and the next send detaches a
   CONTINUOUSLY READING client via Lagged. (The long-replay test used small envelopes.)
2. Capacity is a snapshot, not an atomic reservation: ≥3 concurrent replay lanes jointly
   observe the same aggregate headroom and overbook; the losing replay's refusal detaches a
   reading peer. [= Fable D2-4; two lanes safe by half-cap, three are not, e2e only tested two.]
3. Graceful drain can commit WITHOUT broadcasting: drain rejects appends, removes owners, and
   aborts replay before joining actors — an append/CAS already inside its store await commits
   and "publishes" to orphaned senders, violating §6.6's bounded checkpoint grace + final-
   committed-envelope broadcast.

P2 (gpt)
4. Admission slot not cancellation-safe (no RAII guard; cancel during register_reserved().await
   skips the refund) + close-vs-registration race (actor registers before hub owner inserted;
   a concurrent close snapshots no owner → attachment installed on a closed connection).
5. An attachment's event can OVERTAKE its own attach response (response rides the shared system
   lane; round-robin can visit the new attachment lane first) — W3c would receive an event for
   an unknown attachment id.
6. Quiescent stuck client parks forever — acceptable v0.1 availability limitation but needs an
   EXPLICIT ledger row + future idle/dead-peer trigger. [= the implementer's own flagged residual]
7. Aborting replay doesn't cancel an in-flight blocking SQLite read (ledgered) — but comments
   claiming "no store handle survives hub shutdown" overclaim; soften them.

P3 (gpt)
8. MenuAnswer generation-law wording: idempotency deliberately precedes the fence; the comment
   should say fencing precedes disclosure of a DIFFERENT command's winner coordinate.

D2 (Fable)
9. Stale R12 law site: session_hub.rs:22-24 + lib.rs:36-38 state only the outbox response;
   internal catch-up overflow re-registers + store-resumes WITHOUT detaching — state both
   overflow responses at the authoritative site.
10. Wire-doc contradiction: frame.rs:473-474 "every later Event is live" is false — internal
    overflow recovery may repeat AttachCaughtUp with strictly increasing high_water_seq; add
    the additive doc sentence (a W3c client coded to the current doc would mis-handle).
11. Hub aggregate worst case undocumented: 256 × 8 MiB = 2 GiB retained catch-up (+ ~256 × 1 MiB
    transients), additive to the documented 1 GiB outbox aggregate — same standard applies.
12. FrameSink liveness obligation unstated: a future sink with capacity 0 that under-signals
    drain_progress wedges replay forever — trait doc must state the pairing/liveness contract.

D3 (Fable, fold-or-ledger): session_hub.rs split trigger ARRIVED (2254 lines; split as W3c's
first commit); duplicate law statements (empty-channel-admits ×2, ledger-reset rationale ×2);
two config dialects (hub knobs not operator-tunable + lumped validation); menu_answer existence
guard asymmetry mints permanent actors for nonexistent sessions; stalled-open parking (=P2-6);
9-positional-arg menu_answer seam → command struct; spawn/register pairing forgettable → fuse
in W3c; test-helper extraction trigger when the next daemon test file appears.

VERDICT: NO_SHIP — false detach of reading clients (bytes + overbooking) and the drain
commit-without-broadcast violate the §5.6/§6.6 contract W3c depends on.
