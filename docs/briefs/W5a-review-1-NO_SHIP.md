# W5a review of record — NO_SHIP (R2 split ACCEPTED; 3 P1 + 4 P2 in the OpenAI adapters)

Reviewer: gpt-5.6 (codex), frozen 2724d7f, scope 40d0a62..2724d7f.

R2 ADJUDICATION: the two-family split (native OpenAI on /v1/responses, openai-compatible on /v1/chat/completions) is ACCEPTED as sound — Chat is the lingua franca for vLLM/Ollama/LM Studio/LiteLLM/TGI; native stays on Responses for richer semantics. The report's Responses-only-compatibility rule (report:204) must be AMENDED to document the two explicit families. Mapping otherwise correct (text, visible reasoning summaries, split tool-arg fragments, dedup tool-ends, usage-before-finish, adapter-owned reqwest::retry::never, vault/redacted secrets, per-turn pin, additive base_url golden-clean). Mutation audit 2/2 killed+restored. Gate green (101 suites, 902, daemond live 30/30).

Required fixes (W5a.1):
1. **P1 — native reasoning continuation silently dropped** (openai.rs:1358 store:false, :756 ignores non-function output_item.done, provider.rs:41 has no provider-opaque StreamEvent). GPT-5/o-series encrypted reasoning state is shown as summary but cannot be committed/replayed on the next request → corrupts multi-turn. Fix: carry the reasoning-continuation item through — either a provider-opaque StreamEvent the turn engine persists and replays in the next TurnRequest input, or store:true with server-side reference-by-id (decide + justify; opaque-passthrough is the privacy-preserving choice). This is the capability R2 was protecting; it must not be lost.
2. **P1 — compatible base-URL credential-bearing SSRF** (openai.rs:1652 accepts arbitrary HTTP/HTTPS; :108 attaches the bearer key to probes + turns). A typo/malicious descriptor targets link-local metadata (169.254.169.254), an internal daemon service, or a remote plain-HTTP endpoint WITH the key. Fix: origin validation — remote MUST be HTTPS; plain HTTP only for numeric loopback (127.0.0.0/8, ::1) for Ollama/LM Studio; BLOCK private (10/8, 172.16/12, 192.168/16), link-local (169.254/16, fe80::/10), and metadata endpoints. Redirects/secret-formatting already secured; origin is not.
3. **P1 — incomplete tool calls reach the actor unbalanced** (openai.rs:804 Responses response.incomplete + :1092 Chat permit an open partial call before Finish(MaxTokens); actor.rs:1062/:1888 closes it Pending and parses the incomplete JSON → a legit max-token outcome becomes an error). Fix: on incomplete/max-token, do not hand the actor a partial tool call it will try to parse — emit the max-token finish cleanly (drop/abort the unterminated call), or make the actor treat an incomplete call as aborted-not-error. Pin the max-token-mid-tool-args case.

P2 (fold in — cheap + safety-relevant):
4. Unbounded /v1/models + HTTP error-body reads (openai.rs:286/:363) → a hostile endpoint exhausts daemon memory. Cap both.
5. Streamed overloaded_error → non-retryable InvalidRequest (openai.rs:1213); classify it Overloaded (HTTP 401/403/429/503 are already correct).
6. Refusal deltas emitted as ordinary TextDelta (openai.rs:656/:971) — the report forbids relabeling refusal content as assistant text; route refusal to its own channel (Finish(Refusal) already survives).
7. Fixture gaps: add concurrent tool calls, opaque reasoning continuation, refusal, hostile-probe bounds, timeout/cancellation, and an ignored OpenAI live-smoke/provenance manifest.

Also: amend docs/research/w5-provider-research-report.md:204 to the two-family rule.

W5b PREREQ (not W5a's fix, but W5b's first task): OAuth cannot slot into the current builder — turn resolution blindly resolves the vault blob as the bearer (accounts.rs:1051) and never branches on auth_method (accounts.rs:919). W5b needs an auth-aware credential broker (parse token bundle, single-flight refresh, supply only the access token).

## R2 adjudication

The implementation is an explicit two-family split:

- Native OpenAI uses `POST /v1/responses` and the Responses decoder ([openai.rs:158](/Users/rizzist/haider-run/haider-w5a/crates/haider-provider/src/openai.rs:158), [openai.rs:207](/Users/rizzist/haider-run/haider-w5a/crates/haider-provider/src/openai.rs:207)).
- `openai-compatible` uses `/v1/chat/completions` ([openai.rs:228](/Users/rizzist/haider-run/haider-w5a/crates/haider-provider/src/openai.rs:228), [openai.rs:1652](/Users/rizzist/haider-run/haider-w5a/crates/haider-provider/src/openai.rs:1652)).

Ruling: accept this split. Chat Completions is defensible for vLLM/Ollama/LM Studio/LiteLLM/TGI, while keeping native OpenAI on Responses preserves its richer semantics. Do not unify native OpenAI onto Chat Completions.

However, the native implementation does not yet preserve Responses reasoning-continuation items, so the material capability R2 was protecting is still lost. The report must be amended to document two explicit families—native Responses and compatible Chat—rather than its current Responses-only compatibility rule ([report:204](/Users/rizzist/haider-run/haider-agent/docs/research/w5-provider-research-report.md:204)).

## Findings

### P1

1. Native reasoning continuation is silently dropped. With `store:false` ([openai.rs:1358](/Users/rizzist/haider-run/haider-w5a/crates/haider-provider/src/openai.rs:1358)), non-function `response.output_item.done` items are ignored ([openai.rs:756](/Users/rizzist/haider-run/haider-w5a/crates/haider-provider/src/openai.rs:756)), and `StreamEvent` has no provider-opaque event ([provider.rs:41](/Users/rizzist/haider-run/haider-w5a/crates/haider-protocol/src/provider.rs:41)). Scenario: GPT-5/o-series returns encrypted reasoning state; Haider displays its summary but cannot commit or replay the continuation on the next request, corrupting multi-turn behavior.

2. Compatible base URLs allow credential-bearing SSRF and cleartext remote keys. Validation accepts arbitrary HTTP/HTTPS hosts without HTTPS-remote/numeric-loopback-HTTP constraints or private/link-local protection ([openai.rs:1652](/Users/rizzist/haider-run/haider-w5a/crates/haider-provider/src/openai.rs:1652)); probes and turns attach the bearer key ([openai.rs:108](/Users/rizzist/haider-run/haider-w5a/crates/haider-provider/src/openai.rs:108)). Scenario: a typo or malicious descriptor targets link-local metadata, an internal daemon service, or a remote plain-HTTP endpoint. Redirects and secret formatting are correctly secured, but origin validation is not.

3. Incomplete tool calls reach the actor unbalanced. `response.incomplete` permits an open partial call before `Finish(MaxTokens)` ([openai.rs:804](/Users/rizzist/haider-run/haider-w5a/crates/haider-provider/src/openai.rs:804)); Chat behaves similarly ([openai.rs:1092](/Users/rizzist/haider-run/haider-w5a/crates/haider-provider/src/openai.rs:1092)). The actor closes it as `Pending` and parses the incomplete JSON ([actor.rs:1062](/Users/rizzist/haider-run/haider-w5a/crates/haider-core/src/actor.rs:1062), [actor.rs:1888](/Users/rizzist/haider-run/haider-w5a/crates/haider-core/src/actor.rs:1888)), converting a legitimate max-token outcome into an error. It does not remain durably dangling, but the terminal result is wrong.

### P2

1. Hostile `/v1/models` and HTTP error bodies are read without size bounds ([openai.rs:286](/Users/rizzist/haider-run/haider-w5a/crates/haider-provider/src/openai.rs:286), [openai.rs:363](/Users/rizzist/haider-run/haider-w5a/crates/haider-provider/src/openai.rs:363)). A configured endpoint can exhaust daemon memory. Capability inference is otherwise conservative and cannot claim unsupported features.

2. Streamed `overloaded_error` is not classified as `Overloaded`; it falls to non-retryable `InvalidRequest` ([openai.rs:1213](/Users/rizzist/haider-run/haider-w5a/crates/haider-provider/src/openai.rs:1213)). HTTP 401/403/429/503 classification is correct.

3. Refusal deltas are emitted as ordinary `TextDelta` in both decoders ([openai.rs:656](/Users/rizzist/haider-run/haider-w5a/crates/haider-provider/src/openai.rs:656), [openai.rs:971](/Users/rizzist/haider-run/haider-w5a/crates/haider-provider/src/openai.rs:971)). `Finish(Refusal)` survives, but the report explicitly forbids relabeling refusal content as ordinary assistant text.

4. Fixture coverage uses the real decoder but lacks concurrent tool calls, opaque reasoning continuation, refusal, hostile-probe bounds, timeout/cancellation, and an ignored OpenAI live smoke/provenance manifest.

### P3

None found.

## Mapping-correctness report

- Text, visible reasoning summaries, split tool-argument fragments, deduplicated tool ends, terminal usage, and normal finishes map correctly.
- Native calls are indexed by `output_index`/`item_id`; compatible calls are indexed independently and reject ID/name changes. Multiple calls are structurally supported, though not fixture-tested.
- Usage includes input, output, cached, and reasoning tokens and is emitted before `Finish`.
- Incomplete/max-token handling is unsafe as described in P1-3.
- HTTP authentication, permission, rate-limit, and 503 overload mapping is sound; streamed overload mapping is incomplete.
- Both adapters use `reqwest::retry::never()`, leaving retry/backoff with the actor ([openai.rs:73](/Users/rizzist/haider-run/haider-w5a/crates/haider-provider/src/openai.rs:73)).
- Secrets remain vault-resolved and debug-redacted; response bodies are excluded from formatted provider errors.
- Factory dispatch preserves Anthropic, native OpenAI, and compatible OpenAI; resolution occurs once per logical turn, preserving the account pin across retries.
- `base_url` is additive, absent-on-`None`, and unknown-field tolerant; protocol goldens passed byte-identically.

## Mutation audit

- Factory mutation: removed the OpenAI and compatible dispatch arms. The dispatch test failed with `no account-backed adapter for provider openai`.
- Mapping mutation: dropped native function-argument deltas. The real Responses SSE fixture failed, showing both split JSON fragments missing.
- Both mutations were restored. Original SHA-256 values were recovered and `git status --short` is empty.

## Gate result

- Frozen HEAD: `2724d7fc7bffdec734e0cd93ebe7d38dfec7d7f4`
- `cargo test --workspace`: passed; no failures or compile errors.
- Real daemond UDS live-turn suite: 30/30 passed.
- OpenAI provider suite: 11/11 passed.
- `cargo clippy --workspace --all-targets -- -D warnings`: passed.
- `cargo fmt --all -- --check`: passed.
- `xtask test-count`: 902/902; baseline increased monotonically from 888, with no test deletion.
- Final worktree: clean and byte-identical.

## W5b readiness

`AuthMethod::OAuth` exists, but OAuth cannot slot in unchanged. Turn resolution blindly resolves the vault blob and passes it as the bearer credential ([accounts.rs:1051](/Users/rizzist/haider-run/haider-w5a/crates/haider-daemon/src/accounts.rs:1051)); the builder never branches on `auth_method` ([accounts.rs:919](/Users/rizzist/haider-run/haider-w5a/crates/haider-daemon/src/accounts.rs:919)). W5b needs an auth-aware credential broker that parses token bundles, refreshes single-flight, and supplies only the access token—not the stored OAuth bundle—to the adapter.

VERDICT: NO_SHIP
