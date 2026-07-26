# C4a review round 4 — NO_SHIP (gpt-5.6, frozen 3fc4c01)

Reviewed clean `w2-c4a` at frozen commit `3fc4c0106b2ece4aca816348cecd582397d436b3`.

### Findings

1. **P1 — The finalizer is not runtime-shutdown-safe and becomes unobserved after caller cancellation.** `fs_patch` creates the blocking worker and an ordinary Tokio task as its shield (`filesystem.rs:364`, `filesystem.rs:379`). The caller alone awaits its handle at `filesystem.rs:388`; cancelling the caller drops that handle. A live runtime retains the detached task, fixing round 3's tested schedule, but runtime shutdown drops async tasks waiting at `worker.await` while the blocking worker can continue through rename and ledger failure. No owner drains finalizers before shutdown. Likewise, if the outcome append itself returns an error at `broker.rs:348`, the detached task returns that error through `broker.rs:364`, but nobody observes or retries it after caller cancellation. Thus `rename + ledger failure + runtime shutdown` — or `rename + ledger failure + outcome-append failure` — still permits no ledger and no outcome.

2. **P1 — Shared lifecycle transitions admit competing or duplicate outcomes.** `BrokerJournal::journal_outcome` checks `Dispatched`, asynchronously appends, then sets `Outcome` as three separate operations (`broker.rs:341-353`). The detached finalizer shares that state, while the original broker still exposes `journal_outcome`/`journal_unknown` (`broker.rs:635`). After dropping a cancelled `fs_patch` future releases the broker borrow, reconciliation can race the finalizer. Both can pass the `Dispatched` check before either commits; the sink mutex serializes the appends but does not recheck or atomically claim the lifecycle, allowing two terminal phases. Alternatively, an early `Unknown` can set `Outcome`, causing the finalizer carrying the real ledger error to fail its state check. This breaks one-outcome-per-effect.

3. **P2 — The regression test covers the original post-rename schedule, but not the required XOR.** The test deterministically reproduces round 3 (wait inside the ledger post-rename, abort the caller) but exercises only `rename ∧ Failed outcome`; it never exercises the no-rename arm, cancellation before worker start, cancellation after worker completion, or runtime shutdown. It uses `find` for any outcome rather than asserting exactly one (`filesystem_tools_tests.rs:480-501`), so a duplicate-outcome regression would escape it.

4. **P3 — The committed round 3 review artifact contained duplicated output and hook/token metadata** (fixed in this commit).

Reviewer traced `BrokerJournal::{append_phase,journal_outcome,finish}`, `EffectBroker::{normalize,authorize,journal_dispatched,journal_unknown,detached_finish}`, `fs_patch`, `apply_patch_and_record`, `apply_patch_at`, `open_locked_current_at`, `create_patch_temporary`, `selected_decision`. Canonical `args_digest` binding, generation-stamped ids, fd-anchored `O_NOFOLLOW` walk, locked-fd identity check, same-parent temp + `renameat`, and fail-closed malformed answers all intact. The blocker is ownership of terminalization, not the filesystem apply mechanics.

VERDICT: NO_SHIP
