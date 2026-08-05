# G3 mutation notes — executed kills

Discipline: working tree committed clean before every mutation (base
c7ed07e), ONE anchored mutation at a time, the named test run alone
("running 1 test" observed unless stated), failure recorded, `git checkout`
revert, green re-run observed. 13 kills executed; the six brief-mandated
seams (reasoning merge, fast gate, ladder validation, config-delta
membership, thinking capture, thinking replay) are M1-M6.

## M1 — reasoning-merge seam (openai.rs)

- Mutation: in `responses_request_json`, when an effort is set, REPLACE the
  reasoning object (`reasoning = Map::new()` before the effort insert)
  instead of merging.
- Run: `cargo test -p haider-provider --lib
  effort_merges_into_reasoning_preserving_the_lite_contract` → running 1
  test → FAILED: `summary must survive the effort merge:
  {..."reasoning":{"effort":"xhigh"}...}` (context gone too — exactly the
  live-400 shape the lite contract forbids).
- Reverted; green (1 passed).

## M2 — fast gate (model_select.rs validate_fast)

- Mutation: drop `&& anthropic_fast_mode_supported(model)` — every
  anthropic model passes the gate.
- Run: daemond `select_fast_gates_statically_and_empty_ladders_refuse` →
  running 1 test → FAILED: "expected typed fast refusal" (claude-sonnet-5
  enable was accepted).
- Reverted; suite green (2 passed post-revert re-run).

## M3 — ladder validation (model_select.rs validate_effort)

- Mutation: membership check `supported.iter().any(== effort)` →
  `!supported.is_empty()` (any value accepted when a ladder exists).
- Run: daemond `select_effort_is_receipted_validated_and_replays` →
  running 1 test → FAILED: "expected typed effort refusal" (the
  out-of-ladder "ultra" committed).
- Reverted; green.

## M4 — config-delta membership (actor.rs session_config_only_delta)

- Mutation: classifier decodes `ModelSelected::from_payload_value` ONLY
  instead of the `SessionConfigEventPayload` union.
- Run: `worker_head_cas_tolerates` filter → running 2 tests →
  `worker_head_cas_tolerates_an_effort_and_fast_fact_delta` FAILED
  ("effort+fast fact delta must not reject the batch: Busy … advanced from
  1 to 3") while the pre-G3
  `worker_head_cas_tolerates_a_config_fact_delta` stayed ok — the kill
  isolates exactly the NEW membership.
- Reverted; green.

## M5 — thinking capture (wire/mod.rs signature accumulation)

- Mutation: `signature.push_str(&delta)` → `*signature = delta` (keep only
  the last fragment).
- Run: `signed_thinking_and_redacted_blocks_are_captured_for_replay` →
  running 1 test → FAILED: expected signature `"sig-first-sig-second"`,
  capture carried only the second fragment.
- Reverted; green.

## M6 — thinking replay (wire/mod.rs opaque passthrough)

- Mutation: the anthropic ProviderOpaque content arm additionally requires
  `data.type != "thinking"` — signed thinking facts fall through to the
  foreign-rejection arm (the classic filter bug, inverted).
- Run:
  `thinking_facts_replay_verbatim_in_order_and_normalized_reasoning_stays_rejected`
  → running 1 test → FAILED at the request_payload expect (the tool-loop
  payload errored instead of replaying the block).
- Reverted; green.

## M7 — LT3 strip (worker.rs start_turn)

- Mutation: comment out the `strip_foreign_provider_opaque` call.
- Run: `cross_provider_switch_strips_foreign_opaque_facts` → running 1
  test → FAILED: "no anthropic thinking fact may reach the openai-family
  request" with the anthropic opaque visible in provider B's TurnRequest.
- Reverted; green.

## M8 — LE6 inheritance (delegation.rs resolve_child_metadata)

- First anchor attempt hit model_select.rs (no such clone there) — the
  python guard refused to apply and the test stayed green: recorded as an
  anchor lesson, no mutation landed.
- Mutation (real anchor): after the clone, `child.effort = None;
  child.fast = false;`.
- Run: `spawned_child_inherits_parent_effort_and_fast` → running 1 test →
  FAILED: `left: None / right: Some("xhigh")` on the child metadata.
- Reverted; green.

## M9 — LE1 persistence (event_store.rs select_session_config)

- Mutation: delete the `UPDATE sessions SET meta_json` statement (receipt
  + fact still commit).
- Run: `effort_and_fast_select_are_receipted_and_persist` → running 1 test
  → FAILED: metadata read back `None` vs `Some("xhigh")`.
- Reverted; green.

## M10 — anthropic effort route (wire/mod.rs)

- Mutation: effort emitted as `thinking: {type: enabled, budget_tokens}`
  instead of `output_config.effort` (the 400-on-4.7+/5s shape the brief
  bans).
- Run: `effort_rides_output_config_and_never_thinking_or_sampling_params`
  → running 1 test → FAILED: payload shows
  `"thinking":{"budget_tokens":"xhigh"...}` and no output_config.
- Reverted; green.

## M11 — OAuth beta comma-join (anthropic.rs request headers)

- Mutation: OAuth + fast sends the fast beta ALONE (drops
  `oauth-2025-04-20,` prefix).
- Run: `fast_mode_sets_speed_body_and_comma_joined_beta_header` → running
  1 test → FAILED at the oauth-header assertion (line 581) — live, that
  request loses the subscription identity beta and 400s.
- Reverted; green.

## M12 — gemini gate (gemini.rs thinking_level filter)

- Mutation: drop the name/ladder filter — `thinking_level = effort` for
  every model.
- Run: `effort_injects_thinking_level_for_3x_models_only` → running 1 test
  → FAILED: "an out-of-ladder value injects nothing" (3.1-pro got
  `thinkingLevel: "minimal"`; the 2.5-era assertion would fail next).
- Reverted; green.

## M13 — LE7 no-optimism (app.rs request_effort)

- First anchor attempt used the pre-fmt text — guard refused, test stayed
  green (anchor lesson recorded).
- Mutation (formatted anchor): write `self.identity.reasoning =
  effort.clone()` at REQUEST time, before the reply.
- Run: `effort_and_fast_replies_write_the_identity_line` → running 1 test
  → FAILED: "no optimism: the identity holds until the reply".
- Reverted; green.

## Post-battery

Tree clean after the final revert (`git status` empty); daemon lib 307
passed, daemond effort wire laws 2 passed, provider + tui law files green.
