# Lane brief W2b/C1 — real Anthropic adapter (first real provider)

DRAFT — launches after the W2a merge tags v0.0.6. Prereqs in main by then: haider-accounts
resolver (D3a, API-key credentials; OAuth is D3b/W3a) and the effect/permission boundary
(C4a). Own crates/haider-provider (new module `anthropic.rs` + `wire/` as needed).

Scope
1. AnthropicProvider implementing the existing `Provider` trait (capabilities +
   stream_turn) against the Messages API with SSE streaming:
   - Map TurnRequest → messages payload. Extend TurnRequest as needed (system prompt,
     tool definitions, image blocks per A2 attachment schema) — TurnRequest is
     provider-crate-local, NOT frozen protocol; keep the FakeProvider in lockstep.
   - Stream mapping: message_start/content_block_start/delta/stop → ProviderStreamItem
     (text deltas, tool_use blocks with streamed input JSON fragments, thinking blocks
     where exposed). The Utf8Assembler already guards split scalars — reuse it.
   - Usage: message_delta usage frames → protocol Usage {input, output, cached,
     reasoning, source: ProviderReported} per turn; cache-read tokens mapped to `cached`.
   - Errors: typed — auth (401/403), rate limit (429 + retry-after), overloaded (529),
     malformed frame; each with retryability classification. NO retry loops in the
     adapter — the actor owns backoff policy (RunState::Waiting{provider_backoff}).
2. Credential wiring: the adapter takes a resolved credential handle from
   haider-accounts (never a raw env read; SecretHandle redaction rules apply — the key
   must be unloggable). `HAIDER_ANTHROPIC_API_KEY` import path goes through the
   accounts env bridge, not the adapter.
3. CapabilityDoc: parallel_tools Native, streaming_tool_args Native, vision Native,
   thinking_visible per model, context_limit from a small model table (claude-fable-5,
   opus, sonnet, haiku current ids) with a conservative default for unknown models.
4. Fixtures: sanitized REAL captures (fixture provenance rule: real captures live in C1).
   Record one real streamed turn per shape (text-only, tool-call, image-in, usage-heavy,
   429, malformed) via a capture harness; strip ids/keys; goldens drive an offline
   replay test that feeds the captured SSE bytes through the adapter and asserts the
   ProviderStreamItem sequence. NETWORK TESTS STAY OUT OF CI: live smoke behind
   `HAIDER_LIVE_PROVIDER_TESTS=1`, ignored by default.
5. `haider run --jsonl` gains `--provider anthropic --model <id>` (FakeProvider stays
   the default without flags/env). Exit-code map unchanged; provider errors stay 65.

Laws
- No unwrap/expect in src; typed errors end-to-end; secrets unloggable at type level.
- Deltas advisory, Completed authoritative (item lifecycle law) — the adapter emits
  exactly the same stream-item contract the FakeProvider does; the actor must not learn
  Anthropic-specific behavior.
- Tests in tests/ files only; baseline via xtask test-count --update.

Gate: cargo test --workspace, clippy -D warnings (all targets), fmt --check,
xtask test-count --update, git diff --check. Leave uncommitted.
