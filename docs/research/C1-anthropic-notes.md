# C1 Anthropic Messages adapter notes

Date researched: 2026-07-26.

This document is the wire-to-provider oracle for C1. The implementation and
provisional fixtures must agree with it. The primary references are Anthropic's
[Streaming Messages](https://platform.claude.com/docs/en/build-with-claude/streaming),
[API errors](https://platform.claude.com/docs/en/api/errors),
[models overview](https://platform.claude.com/docs/en/about-claude/models/overview),
[vision guide](https://platform.claude.com/docs/en/build-with-claude/vision), and
[tool-use guide](https://platform.claude.com/docs/en/agents-and-tools/tool-use/overview).

## Request mapping

The adapter sends `POST https://api.anthropic.com/v1/messages` with
`content-type: application/json`, `accept: text/event-stream`,
`anthropic-version: 2023-06-01`, a sensitive `x-api-key` header, and
`stream: true`. The key comes only from a `haider_accounts::SecretHandle`.
The adapter never reads an environment variable.

`TurnRequest` maps as follows:

| Haider request value | Messages API value |
| --- | --- |
| `model` | top-level `model` |
| `max_tokens` | top-level `max_tokens` |
| `system_prompt: Some` | top-level `system` |
| tool `name`, `description`, `input_schema` | an entry in top-level `tools` |
| user/assistant message | `messages[]` with the same role |
| tool-role message | a user message (Anthropic represents tool results inside user content, not as a distinct role) |
| `Block::Text` | `{type:"text", text}` |
| assistant `Block::ToolCall` | `{type:"tool_use", id, name, input:args}` |
| user/tool `Block::ToolResult` | `{type:"tool_result", tool_use_id:call_id, content:preview}` |
| A2 `AttachmentBlock::Image` plus matching resolved data | `{type:"image", source:{type:"base64", media_type:mime, data}}` |
| Anthropic `ProviderOpaque` | its preserved native content-block JSON |

An image attachment without matching resolved bytes is a non-retryable typed
invalid-request error. Pasted-text and skill attachments are also rejected at
this boundary until the prompt compiler supplies their resolved representation;
there is no silent omission.

## Successful SSE sequence

Anthropic names every SSE event with `event:` and repeats the same name in the
JSON `data.type`. A normal stream is:

1. `message_start`
2. zero or more content blocks, each strictly
   `content_block_start`, `content_block_delta*`, `content_block_stop`
3. one or more `message_delta` events
4. `message_stop`

`ping` may appear anywhere and produces no provider item. Unknown future event
types also produce no item, per Anthropic's versioning guidance. A known event
whose SSE name disagrees with `data.type`, invalid JSON/UTF-8, an invalid block
transition, or a premature EOF produces one non-retryable
`ProviderErrorKind::MalformedFrame`, after which the stream terminates.

### Exact event mapping

| Anthropic event/data shape | State retained by adapter | Emitted `ProviderStreamItem` |
| --- | --- | --- |
| `message_start`, empty `message.content`, initial `message.usage` | Record input/cache counters; mark message open | none |
| `content_block_start` with `{type:"text",text:""}` | Open text block at `index` | none |
| `content_block_start` with `{type:"text",text:"nonempty"}` | Open text block | `Ok(TextDelta{text:"nonempty"})` |
| `content_block_delta` with `{type:"text_delta",text}` | Require an open text block at `index` | `Ok(TextDelta{text})` |
| `content_block_start` with `{type:"tool_use",id,name,input:{}}` | Open tool block; retain `index -> id` | `Ok(ToolCallStart{call_id:id,name})` |
| `content_block_delta` with `{type:"input_json_delta",partial_json}` | Require an open tool block | `Ok(ToolCallArgsDelta{call_id,args_fragment:partial_json})` |
| `content_block_stop` for a tool block | Close the block and forget its index | `Ok(ToolCallEnd{call_id})` |
| `content_block_start` with `{type:"thinking",...}` | Open thinking block | none |
| `content_block_delta` with `{type:"thinking_delta",thinking}` | Require an open thinking block | `Ok(ReasoningDelta{text:thinking})` |
| `content_block_delta` with `{type:"signature_delta",...}` | Integrity material stays provider-native; require thinking block | none |
| `content_block_start/stop` for `redacted_thinking` or `fallback` | Track and close opaque block | none |
| `content_block_stop` for text/thinking/opaque | Close the indexed block | none |
| `message_delta.delta.stop_reason` | Retain normalized finish reason | none |
| `message_delta.usage` | Merge cumulative output count with initial input/cache counts | `Ok(UsageUpdate(...))` |
| `message_stop` | Require no open blocks and a retained stop reason; close message | `Ok(Finish{reason})` |

Stop reasons normalize as:

| Anthropic `stop_reason` | Haider `FinishReason` |
| --- | --- |
| `end_turn`, `stop_sequence` | `EndTurn` |
| `tool_use` | `ToolUse` |
| `max_tokens`, `model_context_window_exceeded` | `MaxTokens` |
| `refusal` | `Refusal` |
| `pause_turn` | `EndTurn` (the current actor has no server-tool continuation contract yet) |

An unknown non-null stop reason is a malformed frame rather than a silent
semantic downgrade.

### Usage normalization

The initial `message_start.message.usage` supplies input-side counters.
`message_delta.usage.output_tokens` is cumulative. Every message-delta usage
frame therefore emits a complete per-turn snapshot:

```text
input     = input_tokens + cache_creation_input_tokens
cached    = cache_read_input_tokens
output    = output_tokens
reasoning = 0
source    = ProviderReported
account   = resolved credential alias, when supplied
```

Anthropic does not report thinking tokens as a separate counter; they are part
of `output_tokens`, so inventing a reasoning count would double-count usage.
All additions and conversions are checked. Overflow is a malformed frame.

## Error mapping and retryability

There are no retries, sleeps, or backoff loops in this adapter. Classification
is returned to the actor.

| Wire failure | Provider kind | Retryable | Retry-after |
| --- | --- | --- | --- |
| HTTP 401 `authentication_error` | `Authentication` | no | none |
| HTTP 403 `permission_error` | `PermissionDenied` | no | none |
| HTTP 429 `rate_limit_error` | `RateLimited` | yes | parse delta-seconds or HTTP-date header to milliseconds |
| HTTP 529 `overloaded_error` | `Overloaded` | yes | parse header when present |
| SSE `error.error.type == overloaded_error` | `Overloaded` | yes | unavailable mid-stream |
| SSE `error.error.type == rate_limit_error` | `RateLimited` | yes | unavailable mid-stream |
| SSE authentication/permission error | corresponding auth kind | no | unavailable mid-stream |
| Other HTTP 4xx/API error | `InvalidRequest` | no | none |
| Other HTTP 5xx, timeout, or I/O failure | `Transport` | yes | header when present |
| Bad SSE framing/JSON/state/UTF-8 | `MalformedFrame` | no | none |

Anthropic documents that stream errors can arrive after HTTP 200, and that 529
is represented by `overloaded_error`. It also documents `retry-after` handling
for transient HTTP failures. Adapter error messages retain the normalized type
and status, not provider-controlled message text; they never include request
headers, response bodies, or secret bytes.

## Capabilities and model table

All current listed models support vision. Anthropic documents native parallel
client-tool calls and streamed partial input JSON. Capability lookup uses the
provider instance's selected model:

| Model family/ID | Context | Visible thinking |
| --- | ---: | --- |
| `claude-fable-5` | 1,000,000 | native adaptive summaries |
| current `claude-opus-5*` | 1,000,000 | native adaptive summaries |
| current `claude-sonnet-5*` | 1,000,000 | native adaptive summaries |
| current `claude-haiku-4-5*` | 200,000 | native extended thinking |
| unknown | 100,000 (conservative) | unsupported |

For every row, `parallel_tools`, `streaming_tool_args`, and `vision` are
`Native`.

## HTTP/SSE dependency choice

Use `reqwest` 0.13 with default features disabled and only `json`, `rustls`, and
`stream`. This supplies HTTP, TLS certificate verification, JSON request bodies,
and incremental response chunks without OpenSSL or an SDK. SSE is parsed in
this crate because the format needed here is small and this avoids a second
event-source dependency. `httpdate` is the only other new dependency, used to
honor both legal forms of `Retry-After`. The existing `Utf8Assembler` remains
the sole incremental UTF-8 decoder.

## Fixture provenance and promotion

The C1 fixture manifest is `provisional: true`. Its SSE/HTTP bytes are
synthesized strictly from the documented shapes above; they are not described
as real captures. The ignored capture harness requires both
`HAIDER_LIVE_PROVIDER_TESTS=1` and an explicit promotion flag, imports
`HAIDER_ANTHROPIC_API_KEY` through the accounts environment bridge, sanitizes
message/request/tool IDs, and replaces the provisional files and manifest only
when the owner deliberately runs it. CI only replays local fixture bytes.
