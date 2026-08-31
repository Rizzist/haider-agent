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

Every successful conversation-level reduction records:

- tier;
- estimated input before and after;
- estimated saved input for that operation;
- session-cumulative estimated saved input;
- monotonic operation count; and
- the measurement identifier `provider_request_bytes_div_four_v1`.

The estimator is `ceil(serialized provider-request bytes / 4)` plus Haider's
fixed per-image vision estimate. The same system prompt and tool schemas are
included on both sides, so unchanged request components cancel in the saved
difference. This is repeatable and provider-neutral, but model tokenizers vary:
the API and UI must label these values estimated and must never present them as
exact billed tokens. Output-level reductions should reuse this measurement
identifier if they contribute to the same session total; they must not create
a second, incomparable `tokens_saved` counter.

Each `context_savings_v1` completed extension item is the append-only recovery
authority. `SessionMetadataV1.context_economy` is a monotonic restart-fast
projection. Turn startup reduces the journal and heals metadata if a crash
occurred between the event append and projection update. A malformed event of
this known authoritative kind fails closed as store corruption. Forks start a
new economy ledger, while the parent's transcript and counters remain
unchanged.

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
