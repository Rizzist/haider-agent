# G3 seam map — effort + fast (Explore agent, 2026-08-05, repo @ v0.0.71)

Companion to `g-wave-external-api-research.md` (API truth). Where the two
disagree on Anthropic mechanics, the API doc wins: effort rides
`output_config.effort` (NOT `thinking.budget_tokens`, which 400s on 4.7+),
fast rides `speed: "fast"` + `anthropic-beta: fast-mode-2026-02-01`.

## 1. Effort today is fake

- Catalog capability fields EXIST and are populated:
  `DiscoveredModel.default_effort` (catalog.rs:56-57),
  `supported_efforts` (catalog.rs:58-60); codex parse :466-474 + :542-544;
  kimi `think_efforts` :519-532 + `supports_reasoning`/`supports_thinking_type`
  :502-517 (struct :75-81). Anthropic sources set None/empty (:418-419,
  :435-436).
- The ladder DIES at the rpc projection: `ModelDetailWire` is only
  `{name, context_window}` (haider-rpc frame.rs:597-601; built at
  daemon provider_registry.rs:519-527). No non-test consumer.
- OpenAI Responses: `reasoning` object gets only `summary:"auto"`
  (openai.rs:1949-1951) and lite `context:"all_turns"` (:1952-1954) +
  `include: ["reasoning.encrypted_content"]` (:1956-1961). NO effort key.
  "Reasoning model" = name heuristic `model_has_reasoning` (:2244-2250).
- Kimi: real seam, dead code — `KimiThinkingConfig {type, effort, keep}`
  (openai.rs:441-455) injected as top-level `thinking` in chat_request_json
  (:2149-2154) via `with_kimi_thinking` (:542-553); only caller is a test
  (openai_tests.rs:884).
- Gemini: no thinkingConfig; generationConfig = {maxOutputTokens} only
  (gemini.rs:524).
- Anthropic wire body today: {model, max_tokens, messages, stream} +
  system shape + tools (wire/mod.rs:41-109; insert seam between :87
  and :105). NO thinking/effort/speed field.
- Stream-side reasoning DISPLAY works everywhere (ReasoningDelta) — knob
  looks real but isn't.

## 2. Session-config path (the ride for effort/fast)

- Extend `SessionMetadataV1` (protocol session.rs:34-59) with
  `effort: Option<String>` + `fast: bool` (serde-defaulted); new
  `SessionConfigEventPayload` variant beside ModelSelected (session.rs:67-97).
- RPC mirror of `session_select_model` (daemon session_hub/rpc.rs:2590-2662,
  validation authority model_select.rs:174-206, dispatch rpc.rs:1034).
- Actor: ActorCommand::SelectModel arm (actor.rs:151-160) →
  store.select_session_model (event_store.rs:223-255, txn :1595). New config
  fact kind must join the `session_config_only_delta` decoder (actor.rs:682-720)
  so F3's compaction tolerance still holds.
- R6 pickup free: `fresh_turn_metadata` (worker.rs:1572-1589) →
  `ProviderFactory::resolve_for_turn` (worker.rs:3125-3166; trait :112-131)
  → `build_account_provider` (accounts.rs:4993-5123) — thread effort/fast
  into provider constructors there.

## 3. TUI

- IdentityLine ALREADY renders effort+fast: `reasoning: Option<String>`,
  `fast: bool` (app.rs:2566-2590), `composer_identity` formats
  `model · auth · reasoning [· fast]` (app.rs:5992-6022), band top border
  render (render.rs:4989-5008). NO WRITER exists — dead F2c scaffolding.
- Command registry: CommandSpec/COMMANDS (commands.rs:7-129; completion
  :201-321). Add `/effort [level]` + `/fast`. Dispatch match in app.rs ~:7771.
- Picker machinery = ModelPicker (app.rs:2523-2561, open :9543, accept
  :9750-9754 → AppRequest::SelectModel → runtime.rs:1417, commit via
  apply_model_selected :9759-9765). Effort ladder per row requires
  ModelDetailWire to carry supported_efforts/default_effort.

## 4. Subagents

- Child metadata = parent.clone() with pair override
  (delegation.rs::resolve_child_metadata :112-151, clone :146-149;
  selector authority model_select.rs:219-267). Effort/fast fields on
  SessionMetadataV1 inherit automatically. Optional: spawn_subagent schema
  override (worker.rs:4163-4176).

## 5. Codex responses-lite constraints (MUST not break)

- Contract (openai.rs:1910-1916 comment; golden openai_tests.rs:784-840):
  lite REJECTS max_output_tokens; parallel_tool_calls must be false;
  reasoning.context must stay "all_turns"; store:false. Effort must MERGE
  into the reasoning object built at :1946-1962 (never replace/drop
  context), values validated against catalog supported_efforts.
  Construction guard: accounts.rs:5054-5076.

## 6. CRITICAL side-finding — anthropic thinking replay

- wire/mod.rs:155-156 REJECTS Block::Reasoning replay. Claude 5 has
  thinking ON by default (display omitted → empty text + signature) and
  REQUIRES verbatim replay of thinking blocks within tool-use turns.
  Suspicion: tool loops on anthropic-oauth 400 today (F4 probe was
  text-only). G3 MUST: live-probe a tool call on anthropic-oauth first,
  then (likely) capture thinking/redacted_thinking blocks from the stream
  and replay them verbatim in the follow-up request; law-pin both.

## Smallest correct implementation (reconciled)

1. Effort stays a validated String against per-pair supported_efforts
   (repo "never synthesized" law, catalog.rs:18-21). For anthropic pairs
   the ladder comes from OUR static capability table (API research doc):
   low/medium/high/xhigh/max on 5-family + 4.8/4.7; max-not-xhigh on 4.6s.
   fast: bool only valid on claude-opus-5 / claude-opus-4-8.
2. Persist: SessionMetadataV1 fields + new config fact + receipted RPC +
   actor arm + store txn + config-only-delta membership.
3. Inject per family in provider constructors:
   - anthropic: `output_config: {effort}` + optional
     `thinking: {type: adaptive|disabled}` rules (sonnet-5 may disable;
     opus-5 disable only ≤ high; fable-5 never) + fast → `speed: "fast"` +
     beta header fast-mode-2026-02-01.
   - openai: merge `reasoning.effort` (lite-safe).
   - kimi: existing with_kimi_thinking gated on supports_thinking_type.
   - gemini: generationConfig.thinkingConfig (values per pending
     research report).
4. Surface: ModelDetailWire ladder + /effort + /fast + IdentityLine writer.
5. Subagents: free via metadata clone.
