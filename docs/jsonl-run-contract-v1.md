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
