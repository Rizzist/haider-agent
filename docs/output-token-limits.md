# Output-token limits

`session.create.max_tokens` is the per-provider-request output budget. It is
not a context-window size and it is not the headless run's cumulative token
budget.

The daemon owns model-limit truth. A provider catalog's positive
`max_output_tokens`, `max_tokens`, or `outputTokenLimit` declaration wins.
When discovery is unavailable or the row omits limits, the daemon uses the
pinned provider/model-family table in `haider-provider::model_limits`; exact
known generations take precedence over a conservative provider-family
default. The resulting output maximum is never larger than a known context
window or the largest budget supported by a registered adapter (384,000).
`haider models` exposes both values. Session creation rejects an explicit
request above the resolved maximum instead of silently clamping it.

Interactive sessions request the smaller of 30,000, the model output maximum,
and the context window. A headless client without an override sends the
feature-gated `session.create.max_tokens = 0` derivation sentinel; the daemon
resolves the same model-bounded default and persists/emits the effective
nonzero value. `haider run --max-tokens N` remains an exact per-response
override. The separate `--max-total-tokens N` flag bounds cumulative run
usage.

## Partial tool calls at the limit

An Anthropic `max_tokens`, OpenAI `length`, or Responses `incomplete` terminal
that arrives while tool arguments are open is an output-limit truncation, not
malformed model JSON. Adapters surface the start and argument deltas without a
`ToolCallEnd`. The actor then:

1. records the raw partial arguments and a typed `output_limit_truncation`
   result;
2. never dispatches the truncated call;
3. performs one automatic continuation with instructions to split large
   content across smaller tool calls; and
4. terminates with `output-limit-tool-arguments` if the next tool call is also
   truncated before its end marker.

A valid completed tool call resets this shared tool-call repair allowance.
Malformed calls retain their existing typed `invalid_tool_call` path.
The AX-2 malformed-call strike predicate in `haider-core` matches only
`ToolResultData::InvalidToolCall`; keep `OutputLimitTruncation` outside that
predicate when integrating the lanes. The shared repair allowance is tracked
separately by `repairable_tool_call_result` in `actor.rs`.
