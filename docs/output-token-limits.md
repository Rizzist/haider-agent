# Output-token limits

`session.create.max_tokens` is the per-provider-request output budget. It is
not a context-window size and it is not the headless run's cumulative token
budget.

The daemon owns model-limit truth. A provider catalog's positive
`max_output_tokens`, `max_tokens`, or `outputTokenLimit` declaration wins.
When discovery is unavailable or the row omits limits, the daemon uses the
pinned provider/model-family table in `haider-provider::model_limits`; exact
known generations take precedence over a conservative provider-family
default. `haider-provider::output_budget::model_output_limit` applies the
bounds: the resulting output maximum is never larger than a known context
window or the largest budget supported by a registered adapter (384,000).
`haider models` exposes both values. Session creation rejects an explicit
request above the resolved maximum instead of silently clamping it.

Every client without an explicit override sends `session.create.max_tokens = 0`.
The daemon resolves the smaller of the shared 30,000 default and the selected
model maximum, then persists/emits that effective nonzero value. TUI model
details may be unavailable when a session is created; zero remains safe then.
`haider run --max-tokens N` remains an exact per-response
override. The separate `--max-total-tokens N` flag bounds cumulative run
usage.

## Model switches and explicit budgets

Session metadata records whether the budget is `derived` or `user_set`
(`max_tokens_source`). Metadata written before 973 has no source; a stored
4,096, 8,192 or 30,000 (the pre-973 client defaults) is derived, any other
value was an explicit override. `session.select_model` re-applies the source
to the new model, so a switch never fails on the output budget:

- derived: `min(30,000, new model maximum)`, silently;
- user-set: `min(requested, new model maximum)`; a clamp returns a typed
  `output_budget.clamped` notice (TUI flash, CLI stderr). The requested value
  is kept, so switching back to a larger model restores it.

`session.select_model.max_tokens` (and `haider session <id> config
--max-tokens <n|auto>`) sets a user budget or returns to the derived one; an
explicit value above the selected model's maximum is refused, like
`session.create`. The automatic provider pair-switch path has no validated
model row and keeps the stored budget; the provider retry below covers it.

## Provider-stated maxima

If a provider rejects a request because `max_tokens` exceeds the model's
maximum and states that maximum (Anthropic `max_tokens: A > B, which is the
maximum allowed number of output tokens`, OpenAI-compatible `supports at most
B completion tokens`, DeepSeek `valid range of max_tokens is [1, B]`), the
actor retries that request once at `B` and keeps `B` for the rest of the
turn. A second rejection surfaces as the ordinary provider error; the retry
never loops.

## Sourced static rows

`model_limits.rs` cites the official page for every non-fallback row.
DeepSeek `deepseek-flash` (V4.1-Flash) and `deepseek-v4-pro`: 1M context,
384,000 output. Kimi Code (`kimi-oauth`) and Haider Code publish context
windows but no output ceiling, so documented rows use the shared 30,000
default; xAI documents a 128,000 default output budget and per-model windows.
Only IDs without any published limit stay on the unverified 8,192 fallback.

## Partial tool calls at the limit

An Anthropic `max_tokens`, OpenAI `length`, or Responses `incomplete` terminal
that arrives while tool arguments are open is an output-limit truncation, not
malformed model JSON. Adapters surface the start and argument deltas without a
`ToolCallEnd`. The actor then:

1. records the raw partial arguments and a typed `output_limit_truncation`
   result;
2. never dispatches the truncated call;
3. auto-continues with instructions to split large content across smaller
   tool calls; and
4. after three consecutive truncations, gives the model a typed
   `output_limit_truncation_repeated` tool error with guidance. The run remains
   active and the model decides its next action.

A provider response without an output-limited partial tool call resets the
truncation streak. A completed call in the same truncated response does not.
Malformed calls retain their existing typed `invalid_tool_call` strike path.
The AX-2 malformed-call strike predicate in `haider-core` matches only
`ToolResultData::InvalidToolCall`; keep `OutputLimitTruncation` outside that
predicate when integrating the lanes. The live path and
`recover_tool_repair_state` both use `invalid_tool_call_result` for malformed
strikes; the truncation streak is stored separately. This makes the
973-ax2-tool-friction predicate change a no-op at integration.
