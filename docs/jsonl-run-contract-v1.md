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

A model-authored tool call whose arguments are valid JSON but fail the tool's
argument shape closes with the existing `payload.type == "tool_result"`. Its
`result.status` is `rejected`; the JSON text in `result.preview` has
`status == "rejected"`, `error.kind == "invalid_argument"`, and an
`error.message` naming the invalid or missing field. The result is returned to
the model and the turn continues to another provider request. This is not a
run failure and adds no JSONL field, payload kind, cursor rule, or terminal
kind.

If a session's stored workspace root is missing, is not a directory, or cannot
be opened, a plain chat run continues. Its stream contains exactly one durable,
prompt-omitted raw envelope with `payload.type == "workspace_unavailable"`,
plus `path`, typed `reason`, and bounded `detail`. Cwd-dependent tool calls
complete as rejected tool results with `error.kind == "workspace_unavailable"`;
they are not provider failures. A successful `session.workspace.set` mutation
journals `payload.type == "workspace_selected"` with the new canonical `path`
and optional `previous_path` (present on current producers, absent on legacy
facts). Both payload kinds are additive and must be preserved by raw-envelope
readers.

## Exactly one typed terminal

An attached run ends with exactly one terminal envelope. It is still the
ordinary durable `payload.type == "run_state"` envelope, augmented on the
JSONL surface with `payload.terminal_kind`:

| `terminal_kind` | Meaning |
| --- | --- |
| `success` | Durable run state `done`. |
| `failure` | A non-provider run failure other than budget exhaustion. |
| `budget` | The adjacent `run_failed` code is `budget_exhausted`. |
| `cancellation` | Durable cancellation not caused by the caller's timeout. |
| `timeout` | The caller's wall-clock deadline fired and cancellation was durably confirmed. |
| `provider_error` | The adjacent `run_failed` code is `provider_error` or `provider_timeout`. |

Failed terminals also carry `payload.error_code`. Provider timeout reasons
remain in the durable provider failure presentation and use the provider's
typed reason vocabulary (including `response_open` when supported). JSONL does
not create a parallel timeout-reason taxonomy: `provider_timeout` is a
`provider_error` terminal, `budget_exhausted` is a `budget` terminal, and
`timeout` means the caller's run deadline.

`workspace_unavailable` is never mapped to `provider_error`; a plain degraded
chat still ends with `success`, while a workspace-required direct operation
uses the ordinary non-provider `failure` terminal.

The durable `run_budget_exhausted` fact precedes a budget terminal. New writers
include additive `decision` detail: `spent`, `projected`, `cap`, and a typed
`reason`. `projected` is the candidate request's incremental usage; admission
is refused when `spent + projected > cap`. It is `null` when a projection is
unavailable or does not apply to the decision; unavailable pricing or usage
reasons name the provider and model. A capped run never represents an unknown
estimate as zero. Native PDF projection includes the resolved base64 request
bytes for every document-block occurrence; images retain their documented
fixed visual-token estimate. A sent request abandoned before final usage,
including across restart, is `usage_unavailable` and prevents any later
provider request. Older stored facts without `decision` remain valid and decode
as legacy budget outcomes.

The terminal envelope consumes its normal cursor exactly once. It is not also
emitted as an untyped envelope, so the stream never repeats the terminal `seq`.
Detached submission ends at the accepted/started boundary and is outside this
attached-run terminal guarantee; its terminal is consumed later through the
detached status/events APIs.

## SIGINT cancellation

For `haider run` and the reusable headless control attachment, the first
SIGINT after correlation requests exactly one durable `turn.cancel` for that
run. Transport retries reuse the same command identity and therefore cannot
append a second cancellation intent. The client retains its attachment and
continues consuming cursor-ordered envelopes until the one durable
`cancellation` terminal arrives. The wait never extends past the tighter of
the caller's `--timeout` deadline and time-budget deadline; without either
caller deadline, the ordinary terminal-grace bound applies.

After writing that terminal, `haider run` exits 130. The terminal keeps its
ordinary durable `run_state: cancelled` cursor and appears exactly once; SIGINT
does not create a CLI-only terminal or an extra JSONL record. A second SIGINT
stops the client immediately with exit 130. It is acted on only after the
first signal's durable cancellation receipt, so the fast exit cannot erase or
outrun the journaled cancel. The daemon continues draining the correlated run
if its cancellation terminal was not already delivered.

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
