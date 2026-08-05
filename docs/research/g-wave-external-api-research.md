# G-wave external API research (verified against live docs, 2026-08-05)

Sources: platform.claude.com effort/thinking/fast-mode docs, code.claude.com
model-config/todo-tracking/tools-reference docs. Researcher: Fable 5 subagent
(claude-code-guide), 17 tool uses. Non-Anthropic providers arrive in a second
report; OSS/enterprise endpoints in a third.

## Anthropic fast mode (G3)

- Request: top-level `"speed": "fast"` + header `anthropic-beta: fast-mode-2026-02-01`.
- Models: `claude-opus-5`, `claude-opus-4-8` ONLY. Opus 4.7 + fast → hard
  error (removed 2026-07-24). Opus 4.6 + fast → silently standard
  (`usage.speed: "standard"`). Response reports `usage.speed`.
- Rate limits: 429/529 + `retry-after`; headers
  `anthropic-fast-{input,output}-tokens-{limit,remaining,reset}`.
- Pricing flat $10/$50 per MTok; fast↔standard toggle is a PROMPT-CACHE MISS —
  recommend enabling at session start; surface that in the TUI hint.
- Availability: Claude API only (not Bedrock/Vertex), research preview;
  api-key orgs need enablement; subscription OAuth bills usage credits from
  the first token. Whether third-party oauth-2025-04-20 clients may send it is
  UNDOCUMENTED — treat as capability=maybe, handle the 400/403 path honestly.

## Anthropic effort + thinking (G3)

- Effort: `"output_config": {"effort": "low"|"medium"|"high"|"xhigh"|"max"}`.
  GA, no beta header. Default `high` (== omitting). `"adaptive"` is NOT an
  effort value.
- Support: all five levels on fable-5 / opus-5 / sonnet-5 / opus-4-8 /
  opus-4-7. `max` but NOT `xhigh` on opus-4-6 / sonnet-4-6. Fallback rule
  Claude Code uses: unsupported level → highest supported level at or below.
- Thinking (Claude 5): `"thinking": {"type": "adaptive"|"disabled",
  "display": "summarized"|"omitted"}`; ON by default, display default
  `omitted`. `budget_tokens` (`type: "enabled"`) → 400 on 4.7+ and all 5s.
- Disable rules: sonnet-5 accepts disabled; opus-5 only at effort ≤ high;
  fable-5 rejects disabled entirely.
- 5-family + opus-4.7/4.8 + sonnet-5: non-default temperature/top_p/top_k →
  400. Do not send sampling params there.
- Cache: `thinking` config and resolved `effort` are rendered into the
  prompt — changing either mid-session invalidates prompt cache. Pin per
  session; changing via /effort should warn "cache re-warm".
- Usage: thinking spend in `usage.output_tokens_details.thinking_tokens`
  (final message_delta when streaming).

## Thinking replay in tool loops (LATENT BUG RISK — verify in G3)

- Within a tool-use turn, the assistant content must be echoed back VERBATIM
  with `thinking` blocks (empty text + populated `signature` under
  display=omitted) and any `redacted_thinking` blocks. Dropping/reordering →
  400. Classic bug: filtering `type == "thinking"` misses `redacted_thinking`.
- No beta header needed for thinking+tools on Claude 5;
  `interleaved-thinking-2025-05-14` is legacy — drop from 5-family paths.
- Across turns replay optional (API auto-filters); when SWITCHING models
  mid-session strip prior thinking blocks (other models ignore but bill).
- ACTION: audit our anthropic wire — F4's live probe was a plain text turn;
  if we strip thinking blocks from the replayed assistant message, TOOL
  LOOPS on Claude 5 (thinking on by default) will 400. Must pin with a law.
- Fable-5: refusals to extract reasoning surface as
  `stop_details.category: "reasoning_extraction"`.
- Context overflow on 4.5+ is `stop_reason: "model_context_window_exceeded"`
  (not a validation error). 5-family allows 128k output tokens; SDKs force
  streaming above max_tokens 21333.

## Claude Code todo ergonomics (G1)

- Legacy TodoWrite: `{"todos": [{content, status: pending|in_progress|completed,
  activeForm}]}` — whole-list replace per call. Now DISABLED BY DEFAULT
  (v2.1.142+), replaced by granular Task tools:
  - TaskCreate `{subject, description, activeForm?, metadata?}` → result
    carries assigned `{task: {id, subject}}`.
  - TaskUpdate `{taskId, status?, subject?, description?, activeForm?,
    addBlocks?, addBlockedBy?, owner?, metadata?}`; status enum + `deleted`.
  - TaskList / TaskGet for reads.
  - Claude Code key-repairs model output (`id`/`task_id` → `taskId`,
    `active_form` → `activeForm`) before execution — replicate.
- Owner asked for "update, and complete specific todos etc.. like claude
  code" — granular ops fit; design G1 around create/update-by-id with
  status enum {pending, in_progress, completed} + activeForm spinner label.

## Claude Code /effort surface (G3 UX reference)

- `/effort`, `--effort`, env `CLAUDE_CODE_EFFORT_LEVEL`, settings
  `effortLevel`; levels low/medium/high/xhigh (+ session-only max); default
  high everywhere except opus-4-7 (xhigh).
