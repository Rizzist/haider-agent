# Conversation context economy v1

Haider keeps the event journal append-only and byte-preserving. Context economy
changes only the provider-bound projection. Raw replay, named forks, and
fork-from-prompt continue to read the original envelopes.

## Policy

Two durable modes use one summary boundary:

| Mode | Window use | Action |
|---|---:|---|
| `default` | 85% | summarize the oldest safe clean-turn prefix |
| `fast` | 60% | retain the newest 24 complete tool-call/result pairs |
| `fast` | 75% | retain the newest 12 complete tool-call/result pairs |
| both | 85% | summarize the oldest safe clean-turn prefix |

Thresholds are percentages of the model-declared full context window, capped
by the hard input fit after reserving output tokens. An unknown model window
disables proactive tiers; a provider-reported overflow can still request
recovery.

The 60% first tier deliberately starts later than Command Code's documented
50% reference point. Invalidating a reusable provider prefix while 40% of the
window remains is a real cost. The 75% second tier then gets ten percentage
points to work before Haider's existing 85% summary boundary. Summary remains
at 85%, rather than moving to the reference's 90%, because Haider reserves
provider output and enforces a 15% minimum freed-footprint yield. That stricter
guard rejects summaries which merely churn context.

Structural tiers delete only complete, uniquely matched `tool_call` /
`tool_result` pairs, oldest first. They never shorten a block. Text or
reasoning beside a tool call remains byte-identical, orphan or ambiguous calls
remain, image-bearing results remain, and current-turn pairs remain. The
durable savings event names the removed call IDs; prompt reconstruction
reapplies that selection after restart without altering the original events.
Replay consumes those selections in journal order and removes only the oldest
complete occurrence once, so a provider-scoped call ID reused by a later turn
survives.

## Summary preservation contract

Automatic model-authored summaries retain a suffix estimated at 24,000 tokens
and at least two prior user turns. The split is always immediately before a
committed user-turn node, so it cannot bisect a turn. The estimate is
deterministic serialized provider-request bytes divided by four, not an exact
tokenizer. An explicit manual compaction keeps its established whole-history
behavior, but still moves its boundary before protected skill/image turns and
still splits only at a clean node boundary.

The boundary moves back to the earliest retained turn containing:

- a pinned `Skill` attachment;
- an image attachment; or
- an image-bearing tool result anywhere in that run.

Those turns and every later turn stay verbatim. If these rules leave no older
prefix, compaction declines instead of consuming load-bearing context.

A replacement summary is generated from the original journal fragments named
by the new intent. It does not read the active prior summary artifact. The old
brief is dropped from the provider view and replaced; summaries are never fed
through a summary-of-summary chain.

## Accounting and units

Every successful reduction records estimated input before and after, the
nonnegative estimated saving for that operation, the session-cumulative
estimated saving, a monotonic operation count, and the sole measurement
identifier `provider_request_bytes_div_four_v1`. Conversation events also name
their tier; output events carry an `output` child with the exact or lower-bound
omission facts.

The savings estimator is `ceil(serialized provider-bound projection bytes / 4)`
before minus after. Conversation operations serialize the whole neutral
request projection; output operations serialize the changed provider-bound
text projection, including JSON-string escaping. It deliberately adds no image-token heuristic. The separate
request-occupancy estimate may still use Haider's fixed per-image heuristic to
make threshold decisions, but that value never enters the savings ledger. The
savings unit is repeatable and provider-neutral, while model tokenizers vary:
the API and UI must label these values estimated and must never present them as
exact provider or billed tokens.

Output and conversation records are parent-child layers in one stream, not
competing counters. Conversation operations retain the backward-compatible
parent kind `context_savings_v1`; output operations use the additive child kind
`context_savings_output_v1`. Both share the same monotonic session operation
coordinate and cumulative ledger. Output elision measures the original output
projection (P0) to the bounded model-visible projection (P1). Later
conversation compaction measures that already-bounded transcript (P1) to the
compacted projection (P2). A consumer merges completed records from both kinds
by `session_operation_count` and sums each operation once; inline
`haider_elision_v1` markers are disclosure-only and carry no independently
additive token value. This P0→P1 plus P1→P2 composition prevents a source byte
removed at the output boundary from being counted again by compaction.

Each completed parent `context_savings_v1` or child
`context_savings_output_v1` extension item is an append-only recovery record.
`SessionMetadataV1.context_economy` is a monotonic restart-fast projection.
Turn startup reduces both kinds and heals metadata if a crash occurred between
the event append and projection update. A malformed event of either known
authoritative kind fails closed as store corruption. Forks start a new economy
ledger, while the parent's transcript and counters remain unchanged.

For ctx-v1 typed-reader compatibility, metadata's original `last_event` slot
continues to hold only its tier-bearing conversation event. The newest output
child is stored in additive `last_output_event`; older readers ignore that
field and therefore never deserialize a tier-less child as the old required
conversation shape.

The deterministic stale-output-heavy reference fixture measured 991,104
estimated input tokens before trimming, 198,240 after the 24-pair tier, and
99,132 after the 12-pair tier. Its cumulative estimated saving is 891,972, or
899,978 per one million estimated input tokens. This is a reproducible shape
benchmark, not a workload-average claim and not an exact model-token count.

## Programmatic surface and compatibility

`ContextFootprint.accounting` adds these request-boundary coordinates:

- tokens used;
- model limit;
- remaining tokens;
- usage percentage in basis points;
- next mode-specific tier;
- its token threshold and distance; and
- the complete cumulative economy.

The same economy is available from typed session metadata. Both fields use
Serde defaults and omission for empty/absent values, so old readers keep their
existing JSONL run contract and old stored metadata decodes unchanged. No
existing JSONL key changes meaning; the new extension items and nested fields
are additive.

The policy was informed by Command Code's public [context management
documentation](https://commandcode.ai/docs/context), but the thresholds,
minimum-yield guard, durable structural-selection replay, and typed accounting
surface are Haider's own design.
