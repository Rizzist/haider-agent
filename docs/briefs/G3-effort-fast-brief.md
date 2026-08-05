# G3 — real /effort + /fast per provider-model pair (+ anthropic thinking replay)

Owner contract, verbatim: "make sure reasoning effect is real and can be
changed ./effort etc.. as well as toggle fast mode (if supported by
provider - model pair)". Authority: docs/research/g3-effort-fast-seam-map.md
+ docs/research/g-wave-external-api-research.md (Anthropic) +
g-wave-external-api-research-2.md (other providers). Branch:
`g3-effort-fast`. Read all three BEFORE code.

## Locked design decisions

1. Effort is a per-pair validated STRING (no global enum). Validation
   source of truth: `DiscoveredModel.supported_efforts` from the catalog
   for codex/kimi pairs; for ANTHROPIC pairs a new static capability
   table in haider-provider (API research §effort): 5-family + opus-4-8 +
   opus-4-7 → [low, medium, high, xhigh, max]; opus-4-6 / sonnet-4-6 →
   [low, medium, high, max]. Gemini 3.x name-gated → [minimal, low,
   medium, high] (3.1-pro: no minimal). Unknown pair → empty ladder →
   /effort refuses honestly.
2. Persistence: `SessionMetadataV1.effort: Option<String>` +
   `SessionMetadataV1.fast: bool` (serde defaults). Two new
   `SessionConfigEventPayload` variants: `EffortSelected { effort:
   Option<String> }` (None = revert to provider default) and
   `FastModeSelected { enabled: bool }`. Both JOIN the
   session_config_only_delta classifier (F3 guard) — extend the
   worker_head_cas tolerance law.
3. Wire: two RPC pairs mirroring session.select_model exactly (receipt
   replay, generation fence, validation authority):
   `SessionSelectEffort { command_id, session_id, worker_generation,
   effort }` + `SessionSelectFast { …, enabled }` + FEATURE consts.
   Validation: effort must be in the current pair's ladder; fast only
   valid when pair supports it (see 6). ModelDetailWire grows
   `supported_efforts: Vec<String>` + `default_effort: Option<String>`
   (additive) — the daemon enriches anthropic models from the static
   table when projecting (daemon is the single source of truth; TUI holds
   no tables).
4. Injection per family in build_account_provider →
   provider constructors, threading metadata.effort/fast through
   ProviderFactory::resolve_for_turn (seam map §2-3):
   - anthropic: `output_config: {"effort": E}` when set. NEVER
     thinking.budget_tokens. Do not emit a thinking field otherwise.
   - openai Responses: MERGE `effort` into the existing reasoning object
     (openai.rs:1946-1962) — never drop summary/context; lite contract
     preserved (golden updated, not weakened).
   - kimi: existing with_kimi_thinking gated on catalog
     supports_thinking_type/think_efforts; k3-style pairs whose catalog
     declares reasoning_effort ladder get top-level reasoning_effort.
     Only inject what the catalog declared for that model.
   - gemini: generationConfig.thinkingConfig.thinkingLevel for 3.x-named
     models when set; never thinkingBudget (defer 2.5 numeric budgets);
     never both fields.
5. Effort change mid-session is allowed (codex accepts it; anthropic
   prompt-cache re-warms — TUI flash notes "cache re-warm" on anthropic
   pairs).
6. FAST MODE: anthropic-only, static gate models {claude-opus-5,
   claude-opus-4-8}. ON → body `"speed": "fast"` + append
   `fast-mode-2026-02-01` to the anthropic-beta header (comma-join with
   the oauth beta when both). /fast on unsupported pair → client refuses
   AND daemon validation refuses (honest error, no silent no-op). Wire
   golden asserts both the body field and the beta header. On 400/403
   from the API (third-party OAuth may not be entitled — undocumented),
   surface the provider error verbatim; do not retry-strip silently.
7. TUI: `/effort [level]` — with arg sets; no-arg opens a small picker
   listing the CURRENT pair's ladder (ModelPicker machinery pattern),
   showing default marker. `/fast` toggles with flash. IdentityLine
   writer finally lands: `reasoning` = session's explicit effort (only
   when set), `fast` = metadata.fast — populated from daemon truth on
   select replies + session attach (the F2c scaffolding at
   app.rs:2566-2590/5992-6022 renders it with zero render changes).
   Subagents inherit via metadata clone (delegation.rs:146) — free; add
   an inheritance law.

## Part 2 — anthropic thinking-block replay (latent 400 fix)

8. Claude 5 = thinking ON by default (display omitted → empty thinking
   text + signature). The Messages API REQUIRES verbatim replay of
   thinking + redacted_thinking blocks within a tool-use turn; we
   currently reject Block::Reasoning replay (wire/mod.rs:155-156) →
   tool loops on anthropic are presumed broken (untestable live right
   now: owner's oauth creds expired).
9. Fix rides the EXISTING provider-opaque mechanism (openai
   reasoning.encrypted_content precedent in prompt_history): capture
   anthropic thinking/redacted_thinking blocks from the stream
   (signature_delta accumulation) into the provider-opaque fact for the
   run; on within-run follow-up request assembly, wire/mod.rs emits them
   VERBATIM in the assistant message (before tool_use blocks, original
   order). Cross-provider/model switch: opaque facts from a different
   provider family are SKIPPED (existing behavior — pin it).
10. Do not send temperature/top_p/top_k to 5-family/4.7+/sonnet-5
    (verify we never do; pin with a law if trivially possible).

## Mandatory laws (runtime)

- LE1 effort persists: RPC → meta_json + fact + receipt idempotent +
  stale generation refused (clone the select_model law set).
- LE2 ladder validation: out-of-ladder effort refused per source
  (anthropic static, codex catalog, empty-ladder pair).
- LE3 wire injection per family: request-JSON goldens for anthropic
  output_config.effort; openai reasoning merge preserving
  summary+context (lite golden extended); kimi thinking/effort; gemini
  thinkingLevel. One golden each, non-vacuous.
- LE4 fast: body+header golden; unsupported-pair refusal (client+daemon);
  oauth+fast beta comma-join golden.
- LE5 config-only-delta: EffortSelected + FastModeSelected tolerated by
  compaction head CAS (extend F3 law).
- LE6 subagent inheritance: child metadata carries parent effort/fast.
- LE7 IdentityLine: TUI test — after effort select reply, composer rule
  shows `· <effort>`; after /fast on supported pair shows `· fast`.
- LT1 thinking capture: scripted anthropic stream with thinking block +
  signature → opaque fact journaled with exact payload.
- LT2 thinking replay: follow-up request within the run contains the
  thinking block verbatim, in-order, before tool_use; redacted_thinking
  preserved too.
- LT3 cross-provider strip: after model switch to openai family, no
  anthropic thinking blocks in the request.
- Goldens: new protocol fixtures for the two config facts; rpc transcript
  grows two request/response pairs — regenerate + re-anchor honestly.

## Discipline

Standard lane rules: CARGO_INCREMENTAL=0; per-crate tests; fmt check at
every commit; ledger update + truthful old→new; notes +
mutation-notes docs (≥6 executed kills incl. the reasoning-merge seam,
fast gate, ladder validation, config-delta membership, thinking capture,
thinking replay). No version bumps, no MCP, no renames of existing
variants, never delete ~/.codex/sessions.
