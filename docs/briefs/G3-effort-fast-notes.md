# G3 — real /effort + /fast per pair + anthropic thinking replay: notes

Branch `g3-effort-fast` (from main @ v0.0.72). Brief:
`docs/briefs/G3-effort-fast-brief.md`; API truth:
`docs/research/g-wave-external-api-research.md` (Anthropic — WINS over the
seam map) + `g-wave-external-api-research-2.md`; seams:
`docs/research/g3-effort-fast-seam-map.md`. Live probes were impossible
(owner OAuth creds expired) — everything here is law/golden-verified.

## What shipped

### Persistence + RPC (decisions 1-3, laws LE1/LE2/LE5)

- `SessionMetadataV1` grows `effort: Option<String>` + `fast: bool`, both
  serde-defaulted and SKIPPED when unset, so pre-G3 metadata rows stay
  byte-identical (pinned in protocol goldens).
- Two new config facts join `SessionConfigEventPayload`:
  `effort_selected { effort? }` (absent = revert to provider default) and
  `fast_mode_selected { enabled }`. Membership is what makes LE5
  structural: the F3 head-CAS classifier (`session_config_only_delta`)
  decodes the UNION, so the new variants are tolerated without touching the
  actor — pinned by `worker_head_cas_tolerates_an_effort_and_fast_fact_delta`
  plus the runtime `effort_select_during_manual_compaction_lands_after_it`.
- Store: `session.select_effort` / `session.select_fast` clone the
  `select_session_model` transaction EXACTLY (the F1 lane learning):
  receipt replay inside the txn, worker-generation fence, typed-metadata
  mutation, one published fact, finalized receipt — shared core
  `select_session_config`, two thin public wrappers. Actor arms + hub
  methods are literal clones of the SelectModel arm, so "commit here IS
  next-turn pickup" holds and the actor head advances with the fact.
- RPC: `RequestBody/ResponseBody::SessionSelectEffort/SessionSelectFast`,
  features `session_effort_select_v1` / `session_fast_select_v1`, error
  codes `effort_unsupported` (typed data carries the exact ladder — empty
  means "pair declares none") / `fast_unsupported`. Receipt replay precedes
  validation (R2), mirroring select_model's handler line by line.

### Validation authority (decision 1, LE2/LE4)

`crate::model_select::ModelSelectionAuthority` gains `effort_ladder` /
`validate_effort` / `validate_fast` (typed `TuningRefusal`). Ladder truth
order: the management projection's per-model `ModelDetailWire`
(daemon-enriched), then — for anthropic/gemini only — the pinned static
tables in `haider-provider/src/effort.rs`
(5-family+opus-4-8+opus-4-7 → low/medium/high/xhigh/max; opus-4-6 +
sonnet-4-6 → max-not-xhigh; gemini 3.x thinkingLevel, 3.1-pro without
minimal; date-suffix slugs normalized). Unknown pair → EMPTY ladder →
honest refusal. Fast gate = {claude-opus-5, claude-opus-4-8} statically;
disabling is ALWAYS accepted (recovery is never gated).

### Wire projection (decision 3)

`ModelDetailWire` grows `supported_efforts`, `default_effort`,
`supported_speeds`, `supports_thinking_type` — all serde-skipped (old
bytes unchanged, pinned). The daemon registry enriches anthropic/gemini
rows from the static tables at projection; the TUI holds no tables.
DEVIATION (documented): the brief named two additive fields; two more were
required — `supports_thinking_type` to satisfy decision 4's kimi
shape-gate at the factory, and `supported_speeds` to satisfy decision 6's
"client refuses AND daemon refuses" without a client-side table.

### Injection per family (decision 4, LE3/LE4)

Threading: `ProviderTuning::from_metadata` →
`AccountProviderBuilder::build_tuned` (NEW default method — injected
builders stay source-compatible) → `build_account_provider`:

- anthropic (both auth modes): `output_config.effort` VERBATIM when set —
  NEVER `thinking.budget_tokens`, no `thinking` field otherwise, and the
  decision-10 pin holds (no temperature/top_p/top_k, asserted as an exact
  top-level key set). Fast: body `speed: "fast"` + `fast-mode-2026-02-01`
  on `anthropic-beta` — ONE comma-joined value AFTER `oauth-2025-04-20` on
  OAuth, alone on api-key. Construction gate `anthropic_fast_for` filters
  a STALE fast flag after a model switch off the gate (4.7 hard-errors,
  4.6 silently bills standard); a provider 400/403 on a gated pair
  surfaces verbatim — nothing retry-strips.
- openai Responses: effort MERGES into the one `reasoning` object for
  reasoning models; the live-confirmed lite contract is re-proven WITH the
  merge (summary + context kept, no max_output_tokens,
  parallel_tool_calls=false, include unchanged).
- kimi: factory-gated on the catalog — `supports_thinking_type` pairs get
  `thinking {type: enabled, effort}` via the existing `with_kimi_thinking`
  seam; declared-ladder pairs WITHOUT the toggle (k3-style) get top-level
  `reasoning_effort` (new Kimi-only seam). Only catalog-declared values
  inject.
- gemini: `generationConfig.thinkingConfig.thinkingLevel` for 3.x names
  whose static ladder declares the value; never `thinkingBudget`, never
  both; 2.5-era generationConfig byte-stable.

### Part 2 — thinking capture/replay (decisions 8-9, LT1-LT3)

- The anthropic SSE decoder now CAPTURES signed thinking blocks
  (accumulating `thinking_delta` text AND concatenating `signature_delta`
  fragments) and `redacted_thinking` payloads, emitting each as a
  `ProviderOpaque("anthropic", …)` stream event at the block's stop —
  before the tool events — riding the EXACT mechanism openai's
  `reasoning.encrypted_content` already rides (core journals the fact,
  in-run follow-ups carry it in `assistant_blocks`, prompt_history replays
  it across turns). Unsigned display-only thinking captures nothing.
- Replay is the existing verbatim ProviderOpaque passthrough in
  wire/mod.rs; LT2 pins thinking + redacted_thinking VERBATIM, in order,
  before `tool_use`. `Block::Reasoning` (the normalized display summary)
  stays REJECTED on purpose — it has no provider-valid signature, and
  replaying it as a thinking block is the live 400 the old comment warned
  about. Cross-turn behavior is unchanged: the API auto-filters stale
  thinking across turns, so no cross-turn strip is needed for same-family.
- Base-state finding (why wire/mod.rs rejected Block::Reasoning): the
  rejection was CORRECT — normalized summaries must never replay. The
  latent 400 was that signed blocks were never CAPTURED at all, so tool
  loops on thinking-default Claude 5 replayed an assistant message missing
  its thinking prefix. The fix adds the capture channel; the rejection
  stays.
- Cross-provider strip (LT3): `start_turn` drops provider-opaque blocks
  whose tag differs from the ONE tag the resolved provider's wire accepts
  (anthropic/openai/gemini native; everything else speaks chat-completions
  → `openai-compatible`), sweeping messages left empty. NOTE: the seam
  map called this "existing behavior" — it was NOT; every adapter REJECTS
  foreign opaque (pinned tests), so before G3 a cross-family switch after
  any reasoning turn poisoned every later turn. The strip is new code,
  law-pinned runtime + unit.

### Subagents (LE6) — real finding

The metadata clone in `resolve_child_metadata` was NOT sufficient: the
child create path re-derives `SessionCreateCommand` fields and the store
builds fresh metadata, so tuning silently dropped (the law test caught it
red). `SessionCreateCommand` now carries `effort`/`fast`; the wire
`session.create` passes defaults (bytes unchanged), delegation passes the
parent's CURRENT tuning and adds the keys to the child create's semantic
digest (a same-identity respawn under different tuning is a different
command — pre-G3 pending child receipts straddling an upgrade would
refuse rather than replay; accepted, vanishingly rare).

### TUI (decision 7, LE7)

- `/effort [level|default]`: argument validates against the pair's
  daemon-projected ladder then issues the receipted selection; bare opens
  a composer-slot ladder picker (menu_block anatomy, /theme
  key-ownership: ⏎/digits/click commit, esc closes, daemon cards
  outrank), default row + provider-default/current markers; empty ladder
  refuses honestly. Palette completes `/effort` from the live ladder.
- `/fast` toggles; enabling refuses client-side when the pair detail
  declares no `fast` speed AND daemon-side (decision 6); disabling always
  goes through.
- IdentityLine writer lands: `reasoning`/`fast` written ONLY from
  correlated replies (no optimism — mutation-killed) and from replayed
  `effort_selected`/`fast_mode_selected` facts when the active session
  attaches. ONE render change beyond the F2c scaffolding: the tuning
  segment exists when EITHER knob is set, so `· fast` renders alone —
  LE7's second clause is unreachable otherwise (documented deviation from
  "zero render changes").
- Anthropic pairs flash "cache re-warm" on tuning changes (decision 5).

### Goldens

Protocol fixtures `session_config_effort_selected/fast_selected`; the rpc
wire transcript grew the two request/response pairs at the END
(regenerated with UPDATE_FIXTURES=1 — the fixture diff is 16 inserted
lines, zero pre-G3 bytes touched) with the D1/T1/U1 tail anchors
re-counted truthfully; exact-byte pairs incl. the absent-effort revert
shape; ModelDetailWire additive tolerance both directions;
usage_heavy.events.json re-anchored (the sanitized live capture carries a
signed thinking block the decoder now correctly surfaces).

## Laws by name

- LE1: `select_effort_is_receipted_validated_and_replays` (daemond wire) +
  `effort_and_fast_select_are_receipted_and_persist` (runtime).
- LE2: same two (static ladder + empty-ladder halves) +
  `anthropic_static_ladders_match_the_documented_families` +
  `gemini_static_ladders_are_name_gated_to_3x` (provider).
- LE3: `effort_rides_output_config_and_never_thinking_or_sampling_params`
  (anthropic, also the decision-10 pin),
  `effort_merges_into_reasoning_preserving_the_lite_contract` (openai),
  `kimi_reasoning_effort_is_top_level_and_kimi_only` + the extended
  `kimi_requests_use_bearer_and_max_completion_tokens` (kimi),
  `effort_injects_thinking_level_for_3x_models_only` (gemini).
- LE4: `fast_mode_sets_speed_body_and_comma_joined_beta_header` (wire),
  `select_fast_gates_statically_and_empty_ladders_refuse` (daemon
  refusal), `anthropic_fast_gate_is_opus5_and_opus48_only` (table),
  `provider_tuning_derives_from_metadata_and_fast_gate_filters_stale_pairs`
  (construction gate), `fast_gate_refuses_client_side_on_unsupported_pairs`
  (client).
- LE5: `worker_head_cas_tolerates_an_effort_and_fast_fact_delta` +
  `effort_select_during_manual_compaction_lands_after_it` +
  `golden_session_config_effort_and_fast_facts` (membership).
- LE6: `spawned_child_inherits_parent_effort_and_fast`.
- LE7: `effort_and_fast_replies_write_the_identity_line`.
- LT1: `signed_thinking_and_redacted_blocks_are_captured_for_replay`.
- LT2:
  `thinking_facts_replay_verbatim_in_order_and_normalized_reasoning_stays_rejected`.
- LT3: `cross_provider_switch_strips_foreign_opaque_facts` +
  `opaque_tag_table_and_strip_are_exact`.
- Goldens: `session_tuning_pairs_are_golden_and_revert_omits_the_effort_key`,
  `model_detail_tuning_fields_are_additive_and_skip_empty`,
  `session_metadata_tuning_fields_are_additive_and_skip_defaults`.

## Deviations from the brief

1. ModelDetailWire carries FOUR additive fields, not two (see above —
   required by decisions 4c and 6; all serde-skipped, bytes unchanged).
2. `composer_identity` has one deliberate render change (fast-alone
   segment) — LE7's `· fast` clause is otherwise unrenderable.
3. `SessionCreateCommand` grew effort/fast — not in the brief, but LE6 is
   false without it (the metadata clone alone does not survive the child
   create path; caught by the law test, not by inspection).
4. LT3's "existing behavior — pin it" was wrong on the ground: foreign
   opaque was REJECTED by every adapter, not skipped. The strip is new
   daemon code; the adapter-level rejections stay pinned as-is.
5. Fast injection is gated at CONSTRUCTION on the static table (stale-flag
   safety after a model switch); the /fast toggle itself refuses per
   decision 6, and gated-pair provider errors surface verbatim.
