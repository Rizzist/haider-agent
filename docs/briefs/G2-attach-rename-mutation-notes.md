# G2 — executed mutation rounds

Protocol per lane discipline: all work COMMITTED first (tree clean before
every round), one anchored mutation applied, the ONE named law run with
`--exact` and `running 1 test` observed, the failure recorded verbatim,
the mutation reverted via `git checkout --`, and the same law re-run
green. Seven rounds executed on 2026-08-05; six kills plus one
deliberately recorded survivor half inside round 5.

## M1 — File inlining loses its header (worker.rs)

- Anchor: `file_attachment_text` body →
  `let _ = (name, lines); text.to_owned()` (drops the
  `<file name=… lines=…>` envelope, keeps the raw text inline).
- Test: `cargo test -p haider-daemond --test live_turn_rpc_tests
  file_attachment_is_inlined_with_header_and_never_reaches_the_provider
  -- --exact` → `running 1 test`.
- Observed failure: panic at live_turn_rpc_tests.rs:977 — "the file text
  is inlined with its header" (exact-header block absent from the
  provider request).
- Reverted; re-run: `ok. 1 passed`.

## M2 — daemon UTF-8 re-gate dropped (session_hub/rpc.rs)

- Anchor: `if requires_utf8 && std::str::from_utf8(&bytes).is_err()` →
  `if requires_utf8 && false && …`.
- Test: `cargo test -p haider-daemon --test session_hub_tests
  file_attachment_utf8_and_name_sanity_enforced_at_acceptance -- --exact`
  → `running 1 test`.
- Observed failure: panic at session_hub_tests.rs:922 — the non-UTF-8
  submit was durably accepted instead of returning the typed
  `invalid_argument` ("not UTF-8") response.
- Reverted; re-run: `ok. 1 passed`.

## M3 — rename store txn skips the meta_json UPDATE (event_store.rs)

- Anchor (inside `rename_session` only): the UPDATE became the no-op
  `UPDATE sessions SET meta_json = meta_json WHERE id = ?1` (receipt and
  fact still commit — the sharpest half-transaction lie).
- Test: `cargo test -p haider-daemond --test session_rename_rpc_tests
  rename_is_receipted_published_listed_and_replayed -- --exact` →
  `running 1 test`.
- Observed failure: assertion at session_rename_rpc_tests.rs:357 —
  `left: None, right: Some("Parser rewrite")` (session.list carried no
  title because meta_json never changed).
- Reverted; re-run: `ok. 1 passed`.

## M4 — first_user_turn forced false (event_store.rs)

- Anchor: the `first_user_turn` conjunction in `accept_turn` gained
  `&& false` (auto-title can never fire).
- Test: `cargo test -p haider-daemond --test session_rename_rpc_tests
  auto_title_fires_once_on_first_accept_and_never_overwrites -- --exact`
  → `running 1 test`.
- Observed failure: assertion at session_rename_rpc_tests.rs:482 —
  `left: None, right: Some("fix-the-parser")` (no auto-title after the
  first accept).
- Reverted; re-run: `ok. 1 passed`.

## M5 — the never-overwrite guard PAIR (rpc.rs + event_store.rs)

The overwrite protection is deliberately redundant: the handler's
`metadata.title.is_some()` pre-flight (avoids pointless commands) AND the
store's `only_if_untitled` guard (the transaction-level law that closes
the pre-check→commit race). Both halves were executed:

- Half A (survivor, recorded on purpose): store guard alone neutralized —
  `if command.only_if_untitled && metadata.title.is_some() && false` —
  law re-run: `ok. 1 passed` (the rpc pre-flight still protected).
  This survivorship is the documented cost of intentional
  defense-in-depth, not an untested line: half B proves the law observes
  the pair.
- Half B (kill): with half A still applied, the rpc pre-flight also
  neutralized (`if metadata.title.is_some() && false`).
  Test: `auto_title_fires_once_on_first_accept_and_never_overwrites --
  --exact` → `running 1 test`.
  Observed failure: assertion at session_rename_rpc_tests.rs:567 —
  `left: Some("totally-unrelated-first"), right: Some("Named first")`
  (the auto-title OVERWROTE the pre-named session).
- Both reverted; re-run: `ok. 1 passed`.

## M6 — config-only-delta membership narrowed (session_hub/actor.rs)

- Anchor: the classifier's union decode
  (`from_value::<SessionConfigEventPayload>`) replaced with
  `ModelSelected::from_payload_value(...).is_some()` — model_selected
  stays tolerated, session_renamed does not.
- Test: `cargo test -p haider-daemon --lib
  session_hub::session_hub_private_tests::worker_head_cas_tolerates_a_rename_fact_delta
  -- --exact` → `running 1 test`.
- Observed failure: panic at session_hub_private_tests.rs:906 — "a
  rename-fact-only delta must not reject the batch: HaiderError { code:
  Busy, message: \"session history advanced from 1 to 2 during
  compaction\" }" (the exact mid-compaction wedge the F3 guard exists to
  prevent). The sibling model_selected tolerance law stays green under
  this mutation, proving the two laws pin different membership.
- Reverted; re-run: `ok. 1 passed`.

## M7 — TUI title hydration dropped (app.rs)

- Anchor: `if summary.title.is_some()` in `note_summary_counts` →
  `… && false`.
- Test: `cargo test -p haider-tui --test g2_rename_tests
  session_list_title_hydrates_launcher_rows_and_sessions_listing --
  --exact` → `running 1 test`.
- Observed failure: assertion at g2_rename_tests.rs:194 —
  `left: None, right: Some("Parser rewrite")` (the listed row stayed
  nameless).
- Reverted; re-run: `ok. 1 passed`.

Tree verified clean (`git status`) after the final revert.
