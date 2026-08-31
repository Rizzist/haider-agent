# Haider run JSONL contract (v1)

`haider run --output jsonl` writes LF-delimited JSON objects to stdout. This
contract applies after a run has been accepted; failures before acceptance use
the separate CLI error record documented by `haider run --help`.

## Stream and cursor

The first object is the acceptance proof:

```json
{"event":"accepted","session_id":"…","head_seq":42}
```

Every following object is a durable `RawEnvelope` for that session. Its `seq`
is the resume cursor. The first envelope has `seq == head_seq`; each later
envelope has exactly the preceding `seq + 1`. Haider suppresses duplicates and
repairs gaps before emitting them. A consumer may therefore detect loss with
the same rule and resume strictly after its last fully processed `seq`.

Unknown additive envelope or payload fields must be ignored. Existing payload
types and fields retain their meanings.

## Additive context-economy accounting

Model-boundary reductions do not rewrite earlier journal records. Conversation
compaction appends an ordinary completed parent extension item with
`kind == "context_savings_v1"`. Tool-output elision appends its additive child
kind, `context_savings_output_v1`. Both are ordered in one session economy
ledger and use the sole honesty marker
`measurement == "provider_request_bytes_div_four_v1"`: their token fields are
deterministic estimates derived from serialized provider-bound projection
bytes (including JSON-string escaping for output text), not exact provider or
billed token counts. The distinct child kind keeps
the required conversation `tier` backward-compatible; ctx-era consumers safely
ignore the new kind.

The event's `layer` establishes ownership:

- `tool_output` measures an original tool-output projection to the bounded,
  model-visible projection. Its `output` child carries byte-level omission and
  retained-head/tail facts.
- `conversation` measures the already-bounded transcript to its structurally
  trimmed or summarized projection and carries a `tier`.

Thus the parent and child layers form consecutive boundaries: raw output →
bounded transcript → compacted transcript. Merge completed records from both
kinds by their shared monotonic `session_operation_count`, then sum each
operation's `estimated_tokens_saved` once; conversation events never re-count
source bytes that output elision already removed. The monotonic session
cumulative value is the same sum and survives restart.

Model-visible text elisions contain a standalone JSON line keyed by
`haider_elision_v1`. Those markers disclose that content is incomplete, what
scope was affected, and whether omitted byte counts are exact. They deliberately
contain no token counter and are not an additive accounting stream. The
extension items and marker fields are additive: they do not change tool-call
ids, cursor sequencing, or the terminal rule below, and older consumers may
ignore them.

## Tool-call identity

The provider's tool-call id is the stable call/result join key; Haider does not
allocate a second public call id.

- `payload.type == "item"`, `event == "started"`, and
  `item.item == "tool_call"` publishes `item.call_id`.
- Argument fragments are `item` / `delta` / `tool_args` records. Their
  `item_id` equals the started item's `item_id`; concatenate their `fragment`
  values in cursor order.
- The completed tool-call item repeats both that `item_id` and the same
  `item.call_id`, with the fully parsed `item.args`.
- `payload.type == "tool_result"` repeats the same `payload.call_id`.

This identity remains unchanged when arguments arrive in multiple provider
chunks. The daemon's existing by-call-id deduplication is the authority.

## Exactly one typed terminal

An attached run ends with exactly one terminal envelope. It is still the
ordinary durable `payload.type == "run_state"` envelope, augmented on the
JSONL surface with `payload.terminal_kind`:

| `terminal_kind` | Meaning |
| --- | --- |
| `success` | Durable run state `done`. |
| `failure` | A non-provider run failure, including a daemon budget failure. |
| `cancellation` | Durable cancellation not caused by the caller's timeout. |
| `timeout` | The caller's wall-clock deadline fired and cancellation was durably confirmed. |
| `provider_error` | The adjacent `run_failed` code is `provider_error` or `provider_timeout`. |

Failed terminals also carry `payload.error_code`. Provider timeout reasons
remain in the durable provider failure presentation and use the provider's
typed reason vocabulary (including `response_open` when supported). JSONL does
not create a parallel timeout-reason taxonomy: `provider_timeout` is a
`provider_error` terminal, while `timeout` means the caller's run deadline.

The terminal envelope consumes its normal cursor exactly once. It is not also
emitted as an untyped envelope, so the stream never repeats the terminal `seq`.
Detached submission ends at the accepted/started boundary and is outside this
attached-run terminal guarantee; its terminal is consumed later through the
detached status/events APIs.

## Additive persistence commands

This contiguous JSONL contract is unchanged by the finite persistence
commands. `haider run --replay <run-id>` deliberately emits one
`haider.run.replay.v1` JSON document, not JSONL: it filters a shared session
journal to one run, so its strictly increasing durable `seq` values may have
gaps where other runs own intervening rows. The replay document preserves the
same stable provider tool-call ids and verifies exactly one typed terminal.

`haider resume <session-id> --json`, `haider session <id> recover --json ...`,
and `haider sessions wait-ready ... --json` also emit one versioned document
and one process exit. They do not add, reorder, or reinterpret records in an
accepted `haider run --output jsonl` stream.
