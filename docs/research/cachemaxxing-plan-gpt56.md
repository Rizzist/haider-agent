# CACHEMAXXING plan for Haider

Plan only; no repository files were changed. Findings are based on the current checkout and provider documentation available on 2026-08-10.

The core design decision is: keep Haider’s durable journal and compaction semantics unchanged. Cache optimization belongs in the compiled provider projection, wire adapters, telemetry, and TUI—not in journal durability.

## 1. WHAT HURTS HITS IN HAIDER TODAY

### Current prompt shape

Haider already has a good foundation:

- It compiles the selected ancestry from the journal rather than replaying every event: [prompt_history.rs:91](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/prompt_history.rs:91).
- Prompt-omitted facts, incomplete prior runs, cancelled runs, and error runs do not enter provider history: [prompt_history.rs:739](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/prompt_history.rs:739), [prompt_history.rs:750](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/prompt_history.rs:750).
- Completed user, assistant, tool-call, tool-result, and provider-opaque facts replay in journal/ancestry order: [prompt_history.rs:756](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/prompt_history.rs:756).
- The accepted current user message is last, and live tool-loop messages are appended after the stable prefix: [actor.rs:921](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:921), [actor.rs:1090](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:1090).
- Within one logical turn, the actor reuses the same system prompt, tool definitions, model, and growing message vector: [actor.rs:1131](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:1131).

Thus the main problem is not journal replay. It is turn-to-turn mutation of content or settings rendered before the volatile tail.

### Confirmed cache perturbations

| Cause | Current Haider behavior | Cache consequence |
|---|---|---|
| Provider or model change | Provider/model is resolved once per logical turn, but metadata is re-read for the next turn: [worker.rs:3261](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/worker.rs:3261), [worker.rs:3312](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/worker.rs:3312). | The first request to a different provider or model should be treated as zero reusable cache. Caches are at least provider/model scoped. Switching back may recover the old entry if its TTL has not expired and the prefix is identical. |
| Cross-provider projection changes | Foreign provider-opaque continuation blocks are removed on a provider-family switch: [worker.rs:3243](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/worker.rs:3243), [worker.rs:3359](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/worker.rs:3359). | The replayed message sequence itself changes in addition to moving to a new cache domain. |
| Effort, thinking, and fast mode | Anthropic renders `output_config.effort` and `speed`; OpenAI renders `reasoning.effort`; Gemini renders `generationConfig.thinkingConfig.thinkingLevel`; Kimi renders `thinking` or `reasoning_effort`: [wire/mod.rs:127](/Users/rizzist/haider-run/b2b-tui/crates/haider-provider/src/wire/mod.rs:127), [openai.rs:2336](/Users/rizzist/haider-run/b2b-tui/crates/haider-provider/src/openai.rs:2336), [gemini.rs:557](/Users/rizzist/haider-run/b2b-tui/crates/haider-provider/src/gemini.rs:557), [openai.rs:2544](/Users/rizzist/haider-run/b2b-tui/crates/haider-provider/src/openai.rs:2544). | Changing any rendered setting starts a new prefix/cache epoch. Haider’s own research confirms thinking and resolved effort invalidate caches and that OpenAI codex-lite effort changes invalidate its `previous_response_id` anchor: [g-wave research:38](/Users/rizzist/haider-run/b2b-tui/docs/research/g-wave-external-api-research.md:38), [g-wave research 2:18](/Users/rizzist/haider-run/b2b-tui/docs/research/g-wave-external-api-research-2.md:18). |
| Live project-instruction reload | `HAIDER.md`/`AGENTS.md` content is loaded every logical turn and placed in the system prompt with cwd and optional handoff path: [worker.rs:595](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/worker.rs:595), [worker.rs:3338](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/worker.rs:3338). | An instruction edit changes the system prefix for the next turn and invalidates all subsequent cached history. This should be an explicit cache-epoch transition, not an unexplained miss. |
| System-policy binary upgrades | The builder emits the current binary’s `haider-system-v2` policy rather than reconstructing an old session’s exact policy: [worker.rs:595](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/worker.rs:595). | Updating system text or the version constant makes existing sessions cold after restart/upgrade. |
| Tool-list churn | Normal tool order is a fixed vector and is deterministic: [worker.rs:4164](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/worker.rs:4164). But the advertised pack changes by provider, child/root mode, Anthropic web-tool degradation, and OpenAI hosted-search degradation: [worker.rs:4341](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/worker.rs:4341). | A definition, description, schema, order, or hosted-tool change moves the common-prefix boundary earlier. Anthropic specifically invalidates system and messages when tools change. |
| Provider-native tool churn | Anthropic, OpenAI, and Gemini append different server-side tools: [wire/mod.rs:75](/Users/rizzist/haider-run/b2b-tui/crates/haider-provider/src/wire/mod.rs:75), [openai.rs:2293](/Users/rizzist/haider-run/b2b-tui/crates/haider-provider/src/openai.rs:2293), [gemini.rs:585](/Users/rizzist/haider-run/b2b-tui/crates/haider-provider/src/gemini.rs:585). | Enabling, disabling, or degrading web/search capabilities changes the provider-visible prefix. |
| Compaction | Compaction inserts an immutable summary at the start of the covered range and omits covered ancestry: [prompt_history.rs:585](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/prompt_history.rs:585), [prompt_history.rs:619](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/prompt_history.rs:619). | The first request after compaction can normally reuse system/tools but not the old conversation-history cache. The summary becomes a strong new cache epoch after one re-warm. |
| Compactor uses a different prompt | The compaction request omits the main system prompt and tools and adds a summarization instruction; its usage updates are currently ignored: [worker.rs:229](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/worker.rs:229), [worker.rs:263](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/worker.rs:263). | It has little prefix overlap with normal turns, and its input/cost/cache telemetry is missing from session totals. |
| Anthropic auth-mode change | API-key mode sends a plain system string; OAuth sends an array beginning with the required Claude Code identity: [wire/mod.rs:28](/Users/rizzist/haider-run/b2b-tui/crates/haider-provider/src/wire/mod.rs:28), [wire/mod.rs:107](/Users/rizzist/haider-run/b2b-tui/crates/haider-provider/src/wire/mod.rs:107). | Switching auth surfaces changes both cache/account scope and system shape. Public Anthropic documentation does not confirm cache parity for consumer/Claude Code OAuth; this is unverified. |
| Account/credential rotation | A pre-first-event retry may rotate accounts: [actor.rs:2006](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:2006). | Identical bytes may still miss because provider caches can be workspace/account scoped. Classify this as a cache-scope reset. |
| Attachments and images | Current-user attachments appear after user text and are resolved in place; old CAS-backed attachments are immutable: [prompt_history.rs:757](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/prompt_history.rs:757), [worker.rs:3613](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/worker.rs:3613). | New attachments are correctly placed in the volatile tail. Risks arise from changing block order/shape, re-encoding identical content differently, or provider switching. Anthropic also treats image presence as message-cache-significant. |
| Background task notices | Bounded task notices may be rendered as user-role messages before the terminal-state gate: [prompt_history.rs:727](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/prompt_history.rs:727). | New notices belong at the tail. A future reinjection into system/history would be damaging; retain append-only placement. |
| Failed/cancelled turns | Prior non-done runs are omitted: [prompt_history.rs:743](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/prompt_history.rs:743). | They do not destroy the older prefix, but any tail warmed during the failed run will not be reused. |

### Missing cache controls today

- Anthropic parses `cache_creation_input_tokens` and `cache_read_input_tokens`, but sends no `cache_control`: [wire/mod.rs:867](/Users/rizzist/haider-run/b2b-tui/crates/haider-provider/src/wire/mod.rs:867).
- OpenAI replays full history with `store:false`; it sends no `previous_response_id`, `prompt_cache_key`, `prompt_cache_options`, retention setting, or explicit breakpoints: [openai.rs:2303](/Users/rizzist/haider-run/b2b-tui/crates/haider-provider/src/openai.rs:2303).
- Gemini replays full `contents` and sends no `cachedContent`: [gemini.rs:389](/Users/rizzist/haider-run/b2b-tui/crates/haider-provider/src/gemini.rs:389).
- Kimi and DeepSeek cache-specific usage fields are not decoded by the generic Chat-compatible parser.

### Things not found

These should not be presented as current bugs:

- No memory or roster/callsign reinjection into the provider prompt was found. The roster is TUI state.
- No per-response system override occurs inside a logical turn; the same system prompt is reused through the tool loop.
- No nondeterministic prompt ordering was found. Ancestry, project instructions, and production tool order are deterministic. Canonicalization tests are still valuable, but there is no evidence of hash-map iteration churn today.
- Session titles and permission overrides do not enter the provider prompt.

## 2. STRATEGY

### Priority 0: establish a provider-neutral cache model

Every provider request should be described by these logical zones:

1. Stable policy/system.
2. Stable tool definitions.
3. Immutable completed history, including the active compaction summary.
4. Volatile tail: current user input, new attachments, live assistant/tool blocks, retries, and notices.

This is a conceptual order. Adapters must honor the provider’s actual cache hierarchy—Anthropic’s is `tools → system → messages`—rather than moving tool definitions into messages.

The compiler should expose:

- Stable-history end.
- Current-user start.
- Latest active compaction-summary boundary.
- System, tool-pack, and stable-history digests.
- A cache-epoch identifier.

Cache metadata remains ephemeral request metadata. It must not enter the durable journal or change replay semantics.

### Priority 1: preserve prefix stability

1. **Pin reasoning configuration per cache epoch.** At session creation, resolve and pin provider-specific effort, thinking mode/level, and fast/speed mode. `/effort`, `/fast`, and future thinking commands should either:

   - Apply only to a newly created cache epoch after explicit confirmation, or
   - Be rejected for the current session with a clear “start a new epoch/session” instruction.

2. **Make system-policy changes explicit.**

   - Keep project-instruction loading deterministic.
   - Do not silently freeze stale project policy forever.
   - When an instruction digest changes, record an operational cache-epoch transition and show “instructions changed; next turn cold.”
   - Pin the exact system-policy version for existing sessions across binary upgrades where correctness permits.

3. **Keep tools byte-stable.**

   - Preserve the fixed registry order.
   - Version tool packs.
   - Canonicalize only Haider-owned schema objects and metadata.
   - Never reorder or normalize signed provider-opaque blocks or provider-produced tool arguments.
   - Treat web-tool degradation as an explicit tool-pack epoch change.

4. **Place all new volatility at the end.** Preserve the current append-only tool-loop behavior. New roster, memory, task, environment, or status information must be appended as a bounded tail message unless it is genuinely stable session policy.

5. **Digest the final provider-visible components.** Record non-secret hashes of rendered system, tool definitions, immutable history, model, auth mode, and reasoning settings. An unexpected digest change becomes diagnosable rather than appearing as a mysterious cache miss.

### Anthropic

Anthropic supports automatic or explicit prompt caching, default 5-minute TTL, optional `ttl: "1h"`, and at most four explicit breakpoints. A 5-minute write costs 1.25× base input, a 1-hour write costs 2×, and cache reads cost 0.1×. Usage is partitioned into `input_tokens`, `cache_creation_input_tokens`, and `cache_read_input_tokens`. Exact-prefix changes before a breakpoint invalidate it. Tool changes invalidate tools/system/messages; thinking, effort, images, `tool_choice`, speed, and related request configuration invalidate later cache levels. See the [Anthropic prompt-caching guide](https://platform.claude.com/docs/en/build-with-claude/prompt-caching) and [tool-caching guide](https://platform.claude.com/docs/en/agents-and-tools/tool-use/tool-use-with-prompt-caching).

Recommended explicit allocation:

| Breakpoint | Placement | Purpose |
|---|---|---|
| 1 | Last stable tool definition | Retain the tool pack independently. |
| 2 | Last Haider system block | Retain tools plus system. In OAuth mode, preserve the exact identity block first and attach caching to the later Haider block. |
| 3 | Latest active compaction summary | Establish the current immutable history epoch and avoid falling outside Anthropic’s current 20-block lookback. |
| 4 | End of the last completed historical turn | Cache the longest immutable prefix; current user/live tool loop remains after it. |

There cannot be a simultaneously live breakpoint after every historical compaction because the provider limit is four. Haider should retain every boundary in compiler metadata but emit only the latest valuable epoch boundaries.

TTL policy:

- Default to 5 minutes for fast tool loops.
- Use 1 hour when expected gaps exceed five minutes and the prefix is expected to receive at least two later reads. One later read does not recover the 2× write premium; two generally do.
- Do not combine four explicit breakpoints with top-level automatic caching because the automatic mechanism also consumes capacity.

Public Anthropic documentation describes API-key and workload-identity OAuth on the same API. Haider’s consumer/Claude Code OAuth path is not publicly documented for caching, so cache controls there require a capability probe and safe fallback.

### OpenAI: Responses, Chat, and codex responses-lite

OpenAI automatic prefix caching begins at 1,024 tokens, historically reports hits in 128-token increments, and requires an identical prefix. Tools, images, messages, and structured schemas can be cacheable. See the [prompt-caching guide](https://developers.openai.com/api/docs/guides/prompt-caching) and [cookbook details](https://developers.openai.com/cookbook/examples/prompt_caching_201#11-basics).

Important current distinctions:

- Older model generations generally use automatic longest-prefix matching and have no write surcharge.
- GPT-5.6 adds 1.25× cache writes, explicit `prompt_cache_breakpoint`, request-level `prompt_cache_options`, and a reported `cache_write_tokens`.
- `prompt_cache_options.mode: "explicit"` avoids writing the volatile implicit suffix; current documented explicit TTL is `30m`.
- `prompt_cache_key` is stable for one session, not regenerated per turn. The shipped domain is provider + model + account scope + finalized provider-view header epoch + cohort, where cohort defaults to the session identity. A byte-identical C3 inherited fork uses the durable fork-root route only while its recorded inherited provider-view segment remains active; after provider-view divergence it falls back to its own session identity. Unrelated same-account sessions never share a key.
- Cached-read rates are model-specific, not universally 0.25–0.5×: current GPT-5.x models are commonly 0.1×; GPT-4.1/o3/o4-mini are commonly 0.25×; GPT-4o/o1/o3-mini commonly 0.5×. Use the model-price registry. See [OpenAI pricing](https://developers.openai.com/api/docs/pricing).

For GPT-5.6, mirror the Anthropic boundary allocation with explicit breakpoints and exclude the live tail from writes. For older models, preserve the exact automatic prefix and provide a stable `prompt_cache_key`/supported retention option.

`previous_response_id` is conversation chaining, not free context. OpenAI documents that prior input is still billed and prior `instructions` are not automatically carried over. Haider currently uses `store:false` and does not capture response IDs, so the main cache strategy should not depend on chaining. See [OpenAI conversation state](https://developers.openai.com/api/docs/guides/conversation-state).

The private codex responses-lite contract is externally unverified. Haider’s internal research says effort changes invalidate its response anchor. Any future lite anchor support therefore needs:

- A capability gate.
- Captured response IDs only when the endpoint supports them.
- Full-history fallback.
- Pinned effort for the anchor’s lifetime.

### Gemini

Gemini 2.5 and newer models provide implicit caching automatically. Current minimums are model-specific: the guide currently lists 2,048 tokens for Gemini 2.5 Flash/Pro and 4,096 for Gemini 3.1 Pro Preview/3.5 Flash. Stable large prefixes and closely spaced requests increase hit probability. See the [Gemini cache guide](https://ai.google.dev/gemini-api/docs/generate-content/caching).

For long-lived immutable prefixes, create an explicit `CachedContent` resource containing the system instruction, tool definitions, and stable content, then send its resource name through `cachedContent`. The default TTL is one hour; `ttl` or `expireTime` can be updated, while cached content remains immutable. See the [CachedContent API](https://ai.google.dev/api/caching) and [GenerateContent API](https://ai.google.dev/api/generate-content).

Use explicit Gemini caching only when:

- The stable prefix exceeds the selected model’s minimum.
- It will be reused enough to cover creation and token-hour storage.
- Its system/tools/history will remain unchanged for the TTL.

Create a new resource after compaction or a system/tool epoch transition, and delete superseded resources rather than leaking storage charges. Current standard read pricing is commonly about 0.1× input, plus model-specific storage; use the [current Gemini pricing table](https://ai.google.dev/gemini-api/docs/pricing).

### Kimi, DeepSeek, compatible and local providers

| Provider | Strategy and telemetry |
|---|---|
| Kimi | Caching is automatic. Repeated prefixes must be stable and the earlier request must exceed the documented threshold. Retain a stable `prompt_cache_key`, particularly for Kimi Code Plan sessions. Parse top-level `usage.cached_tokens`; `prompt_tokens` includes it, so uncached input is `prompt_tokens - cached_tokens`. Current multipliers are model-specific: K3 is about 0.1×; K2.6 about 0.168×; K2.7 Code about 0.2×. See [Kimi context caching](https://platform.kimi.ai/docs/guide/use-context-caching-feature-of-kimi-api) and [Kimi pricing](https://platform.kimi.ai/docs/pricing/chat). |
| DeepSeek | Disk prefix caching is automatic and best-effort. Parse `usage.prompt_cache_hit_tokens` and `usage.prompt_cache_miss_tokens` directly. Current V4 Flash hit/miss pricing is $0.0028/$0.14 per million—0.02×—while V4 Pro is approximately 0.0083×. The pricing page warns increases are planned. See [DeepSeek context caching](https://api-docs.deepseek.com/guides/kv_cache/) and [pricing](https://api-docs.deepseek.com/quick_start/pricing/). |
| Generic OpenAI-compatible | Parse only recognized usage shapes. Do not assume standard nested `cached_tokens` is present merely because the endpoint accepts OpenAI request syntax. Unknown means `n/a`. |
| vLLM | Automatic Prefix Caching is deployment-controlled. Newer servers can expose `prompt_tokens_details.cached_tokens` when prompt-token details are enabled; some versions also expose `created_cache_tokens`. Treat this as a version/capability probe. See [vLLM prefix caching](https://docs.vllm.ai/en/latest/features/automatic_prefix_caching/) and [serve options](https://docs.vllm.ai/en/latest/cli/serve/). |
| Ollama | Internal KV reuse may improve latency, but documented usage exposes `prompt_eval_count`, `prompt_eval_duration`, `eval_count`, and duration fields—not exact cached-token counts. Show cache statistics as `n/a`; never infer token hits from latency. See [Ollama usage](https://docs.ollama.com/api/usage). |

### Switching tradeoff

A provider/model switch is justified when expected quality, capability, or latency gains exceed the one-turn re-warm cost. For a stable prefix of `L` tokens, approximate cold-versus-warm extra input cost is:

- Anthropic 5-minute or OpenAI GPT-5.6: `1.15 × L` base-input equivalents.
- Anthropic 1-hour: `1.90 × L`.
- Older OpenAI: `(1 − cached multiplier) × L`, typically `0.5–0.9 × L`.
- Gemini: approximately `0.9 × L`, plus explicit storage where applicable.
- Kimi K3: approximately `0.9 × L`.
- DeepSeek V4 Flash: approximately `0.98 × L`.

Before applying a switch, show: estimated stable-prefix tokens, expected next-turn cold cost, current cache age/TTL if known, and “switching back may recover the old cache but is not guaranteed.”

## 3. THE CACHE-HIT-RATE DISPLAY

### Compact readout

`↑450k ↓227k ⚡108.8M 99.59% hit`

Definitions:

- `↑450k`: cumulative uncached input tokens—tokens requiring normal input processing or cache creation. Cache-write tokens count once here.
- `↓227k`: cumulative provider-billed output tokens. Reasoning detail must not be added twice where it is already a subset of output.
- `⚡108.8M`: cumulative cache-read input tokens only; never cache writes.
- `99.59% hit`: token-weighted hit rate:

  `cached_read / (cached_read + uncached_input)`

For the example, `108.8M / (108.8M + 450k) = 99.59%`.

This is a token hit rate, not a request hit rate, and session aggregation must use summed tokens rather than averaging per-request percentages.

### Provider-source mapping

| Adapter | Uncached input | Cache read | Cache write/detail |
|---|---|---|---|
| Anthropic | `usage.input_tokens + usage.cache_creation_input_tokens` | `usage.cache_read_input_tokens` | `usage.cache_creation_input_tokens`; optionally split through `usage.cache_creation.ephemeral_5m_input_tokens` and `ephemeral_1h_input_tokens`. |
| OpenAI Responses / codex-lite | `usage.input_tokens - usage.input_tokens_details.cached_tokens` | `usage.input_tokens_details.cached_tokens` | GPT-5.6 `usage.input_tokens_details.cache_write_tokens`; do not add this again to total input. |
| OpenAI Chat | `usage.prompt_tokens - usage.prompt_tokens_details.cached_tokens` | `usage.prompt_tokens_details.cached_tokens` | GPT-5.6 sibling `cache_write_tokens` when supplied. |
| Gemini GenerateContent | `usageMetadata.promptTokenCount - usageMetadata.cachedContentTokenCount` | `usageMetadata.cachedContentTokenCount` | No separate write counter in GenerateContent usage; explicit resource metadata/storage is tracked separately. |
| Kimi | `usage.prompt_tokens - usage.cached_tokens` | `usage.cached_tokens` | No documented write/storage counter. |
| DeepSeek | `usage.prompt_cache_miss_tokens` | `usage.prompt_cache_hit_tokens` | No write/storage charge. |
| vLLM | Normalized from nested details if the deployment reports them | `prompt_tokens_details.cached_tokens` | Version-dependent `created_cache_tokens`; otherwise unavailable. |
| Ollama/unknown compatible | Input may be available, but cache split is unavailable | `n/a` | `n/a`. |

The existing Haider protocol has only a numeric `cached` field, so an omitted provider field becomes indistinguishable from a reported zero: [provider.rs:127](/Users/rizzist/haider-run/b2b-tui/crates/haider-protocol/src/provider.rs:127). Phase 1 must add cache-stat availability. A missing field must render `⚡n/a · hit n/a`, not `⚡0 · 0.00%`.

### Accumulation

Use session-rolling totals across all billed provider requests:

- Main turns.
- Every tool-loop request.
- Compaction requests.
- Delegated-agent requests if the display is session-wide.
- Retries only when usage is actually reported.

Usage updates are cumulative only within one logical turn: [actor.rs:1560](/Users/rizzist/haider-run/b2b-tui/crates/haider-core/src/actor.rs:1560). Therefore:

1. Key the latest cumulative snapshot by `(run, agent, provider, model, cache epoch, request kind)`.
2. Let the latest snapshot replace earlier snapshots for that key.
3. Sum only the final snapshots.
4. Never sum every streaming usage update.

This follows the existing `/usage` aggregation pattern: [usage_report.rs:503](/Users/rizzist/haider-run/b2b-tui/crates/haider-daemon/src/usage_report.rs:503).

For mixed-provider sessions:

- `↑` and `↓` may still show complete totals.
- Show a session hit percentage only when all relevant input has cache-reporting coverage.
- If coverage is partial, compact status should say `hit n/a`; `/usage` can display provider-specific rates plus “cache telemetry coverage: 72%.”
- Never blend unsupported-provider input invisibly into the denominator.

### Rendering

Primary location: the one-line status/composer area immediately after the existing state/context meter. The status bar already performs responsive segment dropping: [render.rs:6106](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/render.rs:6106), [render.rs:6175](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/render.rs:6175).

Responsive forms:

- Wide: `↑450k ↓227k ⚡108.8M 99.59% hit`
- Medium: `⚡108.8M 99.6% hit`
- Narrow: omit the cache segment as one unit rather than truncating figures.

`/usage` should show the full session breakdown:

- Uncached input.
- Cache creation/write, split by TTL where possible.
- Cache reads.
- Hit rate.
- Telemetry coverage.
- Input dollars with caching.
- Estimated dollars without caching.
- Estimated savings.
- Provider/model/epoch breakdown.
- Compaction lane separately.

The existing cached row in `/usage` is the natural expansion point: [render.rs:2168](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/render.rs:2168). Plain mode must expose identical semantics: [plain.rs:110](/Users/rizzist/haider-run/b2b-tui/crates/haider-tui/src/plain.rs:110).

## 4. COST MODEL

Let:

- `R` = normal input price.
- `W` = cache-write multiplier.
- `C` = cache-read multiplier.
- `U` = fresh/uncached tokens excluding writes.
- `K` = cache-write tokens.
- `H` = cache-read tokens.

Input cost with caching is approximately:

`U·R + K·W·R + H·C·R + explicit-storage cost`

Without caching it is:

`(U + K + H)·R`

### Illustrative long coding session

Assume 100M logical input tokens:

- 90M cache reads.
- 2M cache writes.
- 8M other uncached input.
- Output excluded because caching does not reduce output cost.

| Provider/model example | Without caching | With cachemaxxing | Input saving |
|---|---:|---:|---:|
| Anthropic Sonnet-class at $3/M, 5-minute cache | $300.00 | $58.50 | $241.50 / 80.5% |
| Anthropic Sonnet-class, 1-hour writes | $300.00 | $63.00 | $237.00 / 79.0% |
| OpenAI GPT-5.6 Terra at $2/M, $2.50/M writes, $0.20/M reads | $200.00 | $39.00 | $161.00 / 80.5% |
| Gemini 2.5 Flash at $0.30/M, $0.03/M reads, explicit 2M-token cache held one hour | $30.00 | about $7.70 | about $22.30 / 74.3% |
| Kimi K3 at $3/M misses and $0.30/M hits | $300.00 | $57.00 | $243.00 / 81.0% |
| DeepSeek V4 Flash at $0.14/M misses and $0.0028/M hits | $14.00 | $1.652 | $12.348 / 88.2% |

These are input-only illustrations. Actual savings depend on model, context tier, cache-write frequency, TTL, storage, and how often compaction/system/tool changes force re-warming.

Current Haider pricing cannot yet produce these numbers reliably:

- The price estimator explicitly ignores write premiums: [pricing.rs:12](/Users/rizzist/haider-run/b2b-tui/crates/haider-provider/src/pricing.rs:12).
- It prices `input` at normal rate and then adds `cached` again: [pricing.rs:201](/Users/rizzist/haider-run/b2b-tui/crates/haider-provider/src/pricing.rs:201). That works with Anthropic’s separate-read semantics but double-counts OpenAI/Gemini cached tokens because their total input already includes cache reads.

Fixing semantic normalization is therefore a prerequisite for savings claims.

## 5. PHASED IMPLEMENTATION PLAN

### Phase 1 — Measure correctly

Objective: obtain trustworthy per-request and session-level cache telemetry before changing prompt behavior.

1. Introduce normalized usage concepts:

   - Total logical prompt input.
   - Uncached input.
   - Cache-read input.
   - Cache-write input.
   - Anthropic 5-minute/1-hour write split.
   - Billed output.
   - Reasoning detail and whether it is a subset of output.
   - Cache-stat availability.
   - Provider, model, account/auth scope, cache epoch, and request kind.

2. Decode every supported usage shape:

   - Existing Anthropic, OpenAI, and Gemini fields.
   - OpenAI GPT-5.6 `cache_write_tokens`.
   - Kimi top-level `cached_tokens`.
   - DeepSeek hit/miss fields.
   - Version-gated vLLM details.
   - Honest unsupported state for Ollama/arbitrary compatible endpoints.

3. Include compaction-lane usage instead of discarding it.

4. Correct the pricing fold so subset-style provider input is not double-counted and write premiums/storage are represented.

5. Add captured-response fixture tests covering:

   - Reported zero cache hits.
   - Missing cache telemetry.
   - Anthropic separate read/write semantics.
   - OpenAI/Gemini subset semantics.
   - Malformed `cached > total` telemetry, which must become unavailable rather than silently saturating.

6. Add prefix-digest instrumentation without changing requests. Establish a baseline: same session configuration should produce identical system/tool/history digests between turns except for append-only history growth.

Exit criterion: session usage reconciles with provider dashboards or captured response totals within rounding limits, including compaction.

### Phase 2 — Display cache performance

Objective: expose the data before optimizing behavior.

1. Add the responsive status-bar readout.
2. Extend `/usage` with session, provider, model, epoch, and compaction breakdowns.
3. Implement the “latest cumulative snapshot per `(run, agent)`” fold.
4. Add telemetry coverage and `n/a` behavior.
5. Add the same meaning to plain mode.
6. Show estimated input cost with and without caching only where the price and telemetry are known.

Exit criterion: the sample counters produce exactly `99.59% hit`, unsupported providers show `n/a`, and mixed-provider sessions cannot display a misleading complete percentage.

### Phase 3 — Prefix stability and provider caching levers

Objective: increase hits without changing conversational semantics.

1. Extend compiled prompt metadata with stable-history and compaction boundaries.
2. Introduce provider-neutral cache-boundary metadata on requests.
3. Preserve the current append-only volatile tail.
4. Add stable final-wire digests and canonicalization tests.
5. Anthropic:

   - Emit explicit breakpoints at tools, system, current compaction epoch, and stable-history end.
   - Select 5-minute versus 1-hour TTL by reuse/gap policy.
   - Capability-gate consumer OAuth.

6. OpenAI:

   - Add stable `prompt_cache_key`.
   - Use GPT-5.6 explicit mode/breakpoints to avoid volatile suffix writes.
   - Preserve automatic exact prefixes on older models.
   - Do not make standard `previous_response_id` a prerequisite.

7. Gemini:

   - Continue exploiting implicit caching.
   - Add explicit `CachedContent` lifecycle for sufficiently large, stable epochs.
   - Track storage and delete superseded resources.

8. Kimi:

   - Retain a stable session `prompt_cache_key`.
   - Preserve reasoning content and tool schemas exactly.

9. Add integration tests that issue repeated representative requests and verify the second request reports a cache hit near the expected stable-prefix length, allowing provider granularity/minimums.

Exit criterion: no-op successive turns do not change system/tool digests; the second eligible request hits the stable prefix; the first post-compaction request is cold only for history and subsequent requests warm the new epoch.

### Phase 4 — Enforce and warn

Objective: stop avoidable cache destruction.

1. Pin effort, thinking, and fast/speed settings to the cache epoch.
2. Make config-change commands show:

   - Which request fields will change.
   - Estimated stable tokens being invalidated.
   - Estimated next-turn re-warm cost.
   - A confirmation to create a new epoch.

3. Warn before provider/model/auth/account switches using the cold-versus-warm estimate.
4. Surface instruction-file, tool-pack, system-version, and web-tool-degradation transitions as named cache busts.
5. Mark compaction as a planned epoch transition, not a cache failure.
6. Offer policy modes:

   - `economy`: prefer current provider/model and stable config.
   - `balanced`: warn above a configurable cold-cost threshold.
   - `mobility`: permit switching but still surface cost.

7. Keep all enforcement reversible through a deliberate new cache epoch; never trap a user in an unsuitable model for the sake of cache percentage.

### Principal risks and mitigations

| Risk | Mitigation |
|---|---|
| Cache writes cost more on short sessions | Default to 5-minute Anthropic caching; use 1-hour or Gemini explicit resources only after reuse thresholds. |
| Four-breakpoint limits reduce flexibility | Allocate by value and current epoch; do not attempt to retain every historical compaction boundary. |
| Explicit Gemini storage leaks | Own resources by session/epoch, record expiry, and delete superseded caches. |
| Pinned settings become undesirable | Permit an explicit new epoch with a cost warning. |
| Project instructions become stale | Do not silently pin changed policy; create a visible epoch transition. |
| Canonicalization breaks signatures or semantics | Canonicalize only Haider-owned definitions; preserve provider-opaque blocks and provider-produced arguments exactly. |
| Private OAuth/responses-lite behavior differs | Capability-probe, feature-gate, retain stateless/full-history fallback, and label unsupported behavior unverified. |
| Provider docs/prices change | Keep TTLs, capabilities, field parsers, and multipliers model/provider-versioned rather than global constants. |
| Cache telemetry absence looks like a miss | Represent availability explicitly and render `n/a`. |
| High hit rate hides excessive context growth | Display hit rate alongside total logical input and context occupancy; cache efficiency does not replace compaction discipline. |

The expected result is that ordinary tool-loop turns retain nearly the entire system/tools/completed-history prefix, compaction causes one clearly explained re-warm, configuration changes become deliberate cache-epoch transitions, and the TUI reports actual—not inferred—cache savings.
