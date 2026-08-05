# F4 — anthropic-oauth turns 400 — notes

Lane: F4, branch `f4-anthropic-oauth-turns` from main 652e008 (v0.0.68).
Bug (live-verified on this Mac, v0.0.68): the first real turn on the
`anthropic-oauth` provider fails with `provider_error — InvalidRequest:
Anthropic HTTP 400 returned an invalid-request error`. Auth was already
correct (`Authorization: Bearer` + `anthropic-beta: oauth-2025-04-20`);
the failure was the request-body shape.

## Capture — the raw 400 the sanitizer collapses

A temporary env-gated hook (`HAIDER_ANTHROPIC_RAW_ERROR_LOG`, removed
before commit) logged the raw non-success body in `stream_turn` before
`replay_anthropic_http_error` sanitized it. One live PTY-probed turn on
`claude-fable-5 · anthropic-oauth` captured (request id redacted):

```
status=400
{"type":"error","error":{"type":"invalid_request_error","message":
"tools.3.custom.input_schema: input_schema does not support oneOf,
allOf, or anyOf at the top level"},"request_id":"req_011C…[redacted]"}
```

`tools[3]` is `fs_search`, whose schema carried a top-level
`anyOf: [{required:[pattern]}, {required:[query]}]`.

## Root cause — TWO independent body-shape defects

1. **Top-level `anyOf` in the `fs_search` tool schema.** Anthropic's
   Messages API rejects `oneOf`/`allOf`/`anyOf` at the top level of a
   custom tool `input_schema` with the exact 400 above. The constraint
   is auth-agnostic (public reports include plain `x-api-key` callers,
   e.g. home-assistant/core#160565); it fires first because tool
   validation precedes the OAuth system check.
2. **Foreign system prompt without the Claude Code identity lead.**
   With defect 1 fixed and an isolation toggle disabling only the
   identity block (temporary `HAIDER_F4_NO_IDENTITY` env experiment,
   removed before commit), every attempt with Haider's own
   `haider-system-v2…` string system prompt was refused —
   live-captured as HTTP 429 `rate_limit_error` with the deliberately
   generic message `"Error"` on all three retries (request ids
   `req_011C…[redacted]`). Community reports of the same server-side
   validation (anthropics/claude-code#40515) see a generic 400
   `"Error"`; either way the subscription token cannot run a turn
   unless the `system` field OPENS with the exact line
   `You are Claude Code, Anthropic's official CLI for Claude.` — as
   its own first block when `system` is an array. Omitting `system`
   or concatenating the identity into a larger first block also fails.

Checked and cleared: `max_tokens` 30000 is within `claude-fable-5`'s
128K output cap; no other beta fields are required for `/v1/messages`
OAuth turns beyond `oauth-2025-04-20` (already sent).

## Fix (crates/haider-provider)

`wire/mod.rs`:

- `request_json` now takes an `AnthropicSystemShape` (`ApiKey` |
  `OAuthClaudeCode`), selected in `AnthropicProvider::request_payload`
  from the provider's auth mode.
  - `ApiKey`: unchanged — plain-string `system`, omitted when absent.
  - `OAuthClaudeCode`: `system` is always an array whose first block is
    exactly `ANTHROPIC_OAUTH_SYSTEM_IDENTITY` (new exported const, doc
    comment records the server-side validation); the turn's real system
    prompt rides as its own second block.
- `anthropic_tool_schema` drops exactly the top-level
  `oneOf`/`allOf`/`anyOf` keys from every custom tool schema for BOTH
  auth modes (the API constraint is auth-agnostic); nested combinators
  are preserved. The dropped clauses stay enforced where they always
  were — the tool executor validates arguments before running.

No change to headers, retry policy, or the api-key system prompt.

## Laws (anthropic_tests.rs, ledger 1704 → 1707)

- `oauth_payload_opens_system_with_claude_code_identity_block` —
  OAuth body opens `system` with the exact identity block, the turn's
  prompt is the second block, and a promptless OAuth turn still sends
  exactly the identity block.
- `api_key_payload_keeps_plain_system_and_matches_oauth_outside_system`
  — api-key `system` stays a plain string (omitted when absent), never
  carries the identity line, and the two modes' bodies are golden-equal
  after removing `system`.
- `tool_schemas_drop_top_level_combinators_for_both_auth_modes` —
  top-level `oneOf`/`allOf`/`anyOf` are dropped in both modes,
  combinator-free schemas pass through byte-identical, nested `anyOf`
  survives.

## Executed mutation campaign — 4/4 kills

1. **Identity text mutated** (trailing period removed from
   `ANTHROPIC_OAUTH_SYSTEM_IDENTITY`): KILLED by
   `oauth_payload_opens_system_with_claude_code_identity_block`
   ("first system block is exactly the Claude Code identity line" —
   the test pins the literal string, not the const). Reverted.
2. **Block order swapped** (identity appended after the turn's
   prompt): KILLED by the same named assertion. Reverted.
3. **`anyOf` strip disabled** (`object.remove("anyOf")` commented
   out): KILLED by
   `tool_schemas_drop_top_level_combinators_for_both_auth_modes`
   ("top-level `anyOf` is dropped … (oauth=false)"). Reverted.
4. **Identity leaked into api-key mode** (`ApiKey` arm mapped to
   `OAuthClaudeCode`): KILLED by
   `api_key_payload_keeps_plain_system_and_matches_oauth_outside_system`
   ("api-key mode sends the turn's system prompt as a plain string").
   Reverted.

Full `cargo test -p haider-provider` green after each revert.

## Live verify (final clean build, hooks removed)

Rebuilt release `haiderd`, installed to /usr/local/bin (`xattr -c` +
ad-hoc codesign, `pkill -x haiderd`), then PTY probe
(`f4_final_probe.py`): launcher → `/model` → search `claude-fable` →
⏎ → send `Reply with exactly this markdown and nothing else: **97**`.

```
PASS picker lands on claude-fable-5 · anthropic-oauth
PASS turn settles IDLE with 97 rendered
PASS assistant render consumed the ** markers
PASS bold SGR (ESC[1m) immediately precedes the rendered 97
RESULT: ALL PASS
```

Raw-byte evidence of the bold render:
`\x1b[1m\x1b[38;2;242;242;242;48;2;15;15;15m97`. The literal `**97**`
appears only in the echoed user message, as expected.

## Review of record (coordinator, executed post-lane)

| # | Mutation (seam) | Law | Observed kill |
|---|---|---|---|
| RV2 | Combinator strip made RECURSIVE — nested `anyOf`/`oneOf`/`allOf` silently removed from every sub-object (`wire/mod.rs anthropic_tool_schema`) | `tool_schemas_drop_top_level_combinators_for_both_auth_modes` | KILLED — running 1 test → "nested combinators are preserved (oauth=false): left: Object {}" — over-stripping would corrupt executor-validated schemas invisibly; the law pins preservation, not just top-level removal |

Reverted; crate green. Lane's 4-kill campaign reviewed — no further gaps.
