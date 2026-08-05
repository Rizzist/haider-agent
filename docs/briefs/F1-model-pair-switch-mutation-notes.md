# F1 — provider-agnostic model selection — mutation notes

Implementer: Fable 5. Branch `f1-model-pair-switch` @ `8b46f87`.
Owner contract baked in: sessions are provider-agnostic — the user selects a
MODEL and the provider rides along as an attribute of the selected row; the
stored pair is the current selection, never session identity.

All mutations were EXECUTED post-commit against `8b46f87`: apply the
mutation, run the named suite, record the observed runtime failure, revert
with `git checkout`, re-run green. No kill below is speculative.

## Executed runtime kills

| # | Mutation (seam) | Expected law | Observed kill |
|---|---|---|---|
| M1 | Pair switch downgraded to no-pickup: `run_supervisor` drops `fresh_turn_metadata` and starts turns from the supervisor's spawn snapshot (`worker.rs`) | `pair_switch_is_receipted_and_next_turn_resolves_the_new_provider` | KILLED — turn 2 resolves the stale (fake-a, model-a) pair; the run never reaches `Done` on provider B and the 10s `await_done` deadline fails. `spawn_after_pair_switch_inherits_the_new_pair` fails with it (2 failed) |
| M2 | Validation dropped: `validate_selection` no longer refuses an uncreatable provider (`model_select.rs`) | `unavailable_provider_refused_typed` | KILLED at BOTH levels — unit: `uncreatable_provider_is_refused_typed` + `explicit_pair_validates_like_a_live_selection` fail; wire: daemond `absent_provider_selects_in_place_and_unavailable_provider_refuses_typed` panics "expected typed refusal" (the daemon happily committed the imaginary provider) |
| M3 | Receipt dropped: `select_session_model` loses its in-transaction receipt replay lookup (`event_store.rs`) | `pair_switch_is_receipted_…` (R2 half) | KILLED — the same-command retry re-executes instead of replaying and dies on `UNIQUE constraint failed: events.event_id`; the test's `IdempotentReplay` expectation fails |
| M4 | Page echoes request data: the `CallbackResult::Code` arm serves `format!("{SUCCESS_HTML}<!-- code: {code} -->")` (`oauth.rs`) | no-echo law over a REAL loopback fetch | KILLED — `fake_browser_success_proves_s256_exact_redirect_and_one_exchange` equality-to-static-constant fails and the diff visibly contains `AUTH_CODE_SENTINEL_51d2`, proving the law observes real request bytes, not vacuous fixtures |
| M5 | Legacy-bytes golden broken: `provider: Option<String>` loses `skip_serializing_if` and serializes `"provider":null` (`frame.rs`) | `absent_provider_keeps_legacy_bytes_and_behavior` (bytes half) | KILLED — `session_select_model_absent_provider_keeps_legacy_bytes` exact-string golden fails on the extra `"provider":null` key |
| M6 | Inherit broken: `establish` hardcodes the child's provider to `"fake-a"` instead of the coordinates' metadata (`delegation.rs`) | `child_inherits_the_parents_current_pair_by_default` | KILLED — `spawn_after_pair_switch_inherits_the_new_pair` fails: provider B never serves the child (parent+child+resume count wrong) and the child's metadata is the hardcoded provider |
| M7 | Preference order inverted: the bare-model arm consults the candidate scan before the parent's own inventory (`model_select.rs`) | parent-first preference | KILLED — `bare_model_prefers_the_parents_provider` fails with `Err(ModelNotResolvable { candidates: ["openai", "other"] })` where the parent's own row was the required answer |
| M8 | Ambiguity guessed: `candidates.first()` wins instead of the exactly-one rule (`model_select.rs`) | `ambiguous_model_is_typed_with_candidates` | KILLED — `ambiguous_bare_model_is_typed_with_candidates` fails: the guess returns `Ok(("kimi", …))` where the typed refusal naming both candidates is the law |

After the final revert: `pair_switch` 4/4 and `model_select` 15/15 green.

## Law → test map

- `pair_switch_is_receipted_and_next_turn_resolves_the_new_provider` —
  in-crate `pair_switch_runtime_tests.rs` (two recording fakes; provider B's
  request asserted) + daemond
  `select_model_is_receipted_published_and_next_turn_lands_on_the_new_pair`
  (UDS wire: receipt replay byte-equality, published `model_selected` fact,
  turn routing).
- `absent_provider_keeps_legacy_bytes_and_behavior` — haider-rpc golden
  (exact bytes, no `provider` key; tolerant decode) + unit
  `absent_provider_selects_within_the_current_provider` + daemond wire
  behavior half.
- `unavailable_provider_refused_typed` — unit + wire (`provider_unavailable`
  code with typed `ErrorData`, refusal mutates nothing).
- `unknown_model_with_known_inventory_refused_typed` — unit
  (`model_unknown`); unknown inventory accepts honestly by companion law.
- `child_inherits_the_parents_current_pair_by_default` — unit (verbatim
  inherit) + runtime `spawn_after_pair_switch_inherits_the_new_pair`
  (inheritance AFTER a mid-session switch, manifest `model_profile`, child
  metadata, child request routing).
- `explicit_model_resolves_cross_provider_and_the_child_runs_it` — runtime
  `explicit_selector_spawns_the_child_cross_provider` (parent stays on A,
  child's one request lands on B).
- `ambiguous_model_is_typed_with_candidates` / `unavailable_is_typed` —
  unit candidate laws + runtime
  `unavailable_spawn_selector_is_a_typed_continuable_rejection` (typed tool
  result, turn continues, no child exists).
- Branded pages — `callback_pages_are_branded_and_fully_self_contained`
  (text wordmark, gold accent, inline-only, no-JS substring bans, referrer
  suppression, owner copy), existing 4-reason/retry law unchanged, live
  fetch now also pins `Content-Type: text/html; charset=utf-8`.

## Gate

`cargo fmt --all --check` clean; `cargo clippy --workspace --all-targets
-- -D warnings` clean; full `haider-daemon` (326 incl. ignored=2 skip),
`haider-daemond`, `haider-rpc`, `haider-protocol`, `haider-store`,
`haider-core`, `haider-tools`, `haider-client`, `haider-accounts`,
`haider-cli`, `haider-verify` suites green. Ledger 1618 → 1643 (+25).

## Review of record (post-lane, executed)

Two seams the lane's kill table did not cover were mutation-probed against
the FULL relevant suites (store, daemon `model_select` + `pair_switch` +
`compact`, daemond wire). Both mutations SURVIVED — real production gates
with zero observation (the structurally-unobserved-second-gate class):

| # | Mutation (seam) | Survived | Pin added | Kill after pin |
|---|---|---|---|---|
| RM1 | Generation fence deleted from `Store::select_session_model` (`event_store.rs`) — a stale-generation selection commits instead of refusing `SingleWriterViolation` | every suite green | `stale_generation_select_is_refused_and_mutates_nothing` (refusal code + no receipt + no fact + unchanged metadata + next turn still lands on A) | running 1 test → FAILED at the `expect_err` (the stale selection committed) |
| RM2 | Manual compaction fed the supervisor's spawn snapshot instead of `fresh_turn_metadata` (`worker.rs` Compact arm, `&fresh` → `&metadata`) — summarization lands on the OLD provider after a pair switch | every suite green incl. all 5 compaction tests | `manual_compaction_follows_the_current_selection` (post-switch summarization request asserted on provider B's recording fake, model-b; A saw only turn 1) | running 1 test → FAILED (compaction work driven to the stale pair) |

Both mutations reverted; 6/6 `pair_switch` laws green on clean code.
Ledger 1643 → 1645 (+2 review pins).
