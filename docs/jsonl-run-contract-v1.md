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

## Durable model narrative and compaction (v0.0.970)

Assistant text and emitted reasoning summaries are captured by the existing
`item` lifecycle (`agent_message`/`incomplete_agent_message`, `reasoning`, and
`text`/`reasoning` deltas). Their additive `payload.provider_request` coordinates
match `X-Haider-Turn`: session_id, run_id, turn_ordinal, request_ordinal,
request_kind. These coordinates and any provider_finish_reason are journaled
before publication; committed_at_ms and schema_version supply metadata.

JSON documents from both `--output json` and `run --replay` add `provider_rounds`,
a shared derived projection of request coordinates, emitted_text,
reasoning_summary, tool_calls, results and terminal_cause. It does not modify
raw envelopes. Unsupported future request metadata stays in the raw events;
legacy absence never causes invented request coordinates. Private compaction
narrative is marked provider_purpose=compaction and excluded from final response.

A successful compaction appends `payload.type=context_compaction` in the same
transaction as its history overlay. It announces the trigger turn, successful
summary request ordinal, inclusive covers_from/covers_to node range,
summary_artifact, dropped_item_count and retained_suffix_size. Both count units
are explicitly provider_message: active prefix replaced and original suffix
retained, excluding the new summary and request-only scaffolding. Replacing an
old summary counts it once; the journal's original history is not deleted.
Failed compaction emits no announcement. Full field and compatibility details
are in `docs/event-schema-changelog.md`.

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

## Autonomous permissions and explicit denials

`haider run` creates an autonomous session. All Haider permission-policy Ask
defaults resolve to ordinary journaled `Allow`, including workspace writes and
process execution; no allow flag is required. Explicit user deny rules and
provider-lockdown hard denies retain precedence, and workspace containment is
unchanged.

An explicit brokered-effect deny produces an `effect` / `authorized` envelope with
`verdict == "deny"` and its stable reason, followed by a typed rejected
`tool_result` that the model can read. The aggregate JSON result also includes
that reason in `permission_denials`; it is never synthesized from a menu label.
For a direct filesystem write under `--read-only`, the exact reason is `write
denied: run is --read-only`. The option also denies local/remote process, Git,
desktop-control, and peer-message effects that could write indirectly, using
route-specific reasons. The client first requires the additive
`session_read_only_v1` feature so an older daemon cannot silently ignore this
explicit deny. After the model observes such a typed refusal, the terminal is
`failure` with `error_code == "permission_denied"` and the same reason.
The plan-gated `loom_register` route has no effect class; read-only therefore
rejects it directly with a typed `tool_result` and the exact terminal reason
`registry mutation denied: run is --read-only`, before any registry CAS or
installer job exists.

## Tool-result byte provenance and file effects (v0.0.970)

These fields are additive; `schema_version` stays 1. On a durable
`payload.type == "tool_result"`, `/truncation` and `/effects` below are JSON
pointers relative to `payload`. The same typed fields are available on its
`result` object, and on standalone bounded tool results. Omitted fields stay
absent, rather than `null` or an empty array, on legacy/unaffected results.

When captured tool output is reduced, `result.preview` retains the existing
prefix/suffix or tool-specific projection and ends with exactly this standalone
line (decimal unsigned integers and lowercase SHA-256):

```text
[haider:truncated truncated=true original_bytes=<uint> payload_bytes=<uint> sha256=<64 lowercase hex of the ORIGINAL bytes>]
```

`/truncation` is its typed mirror:

```json
{"truncated":true,"original_bytes":1048576,"payload_bytes":1234,"sha256":"<hex64>"}
```

`original_bytes` counts the original captured bytes before the preview's
reduction; `sha256` hashes those bytes, not the retained prefix/suffix, a
lossy UTF-8 conversion, or the JSON wrapper. For a process, stdout and stderr
are hashed in capture order. Bytes observed while draining after a process
limit also count; bytes never read from a terminated producer cannot count.
Enumeration/execution limits retain their existing separate incompleteness
facts. For filesystem search/glob, the original is the materialized result
text, not unvisited files. `payload_bytes` counts UTF-8 bytes of the unchanged
preview before the new footer, excluding the footer and any LF added to put
it on its own line. Existing payload bytes, including an existing trailing LF,
are preserved. JSON-in-text consumers can slice the first `payload_bytes`
bytes before parsing. The process's existing nested `context_savings_detail`
still measures that legacy payload; provider-bound accounting includes the
footer overhead without changing source-omission facts. Additional model-boundary
projection keeps the former cap and prefix/suffix bytes and remeasures
`payload_bytes` for its own final footer. No marker or typed mirror is added to an untruncated result.

Applied filesystem write/create/edit/delete results carry `/effects` in the
same order as the workspace receipt and change ledger paths:

```json
{"effects":[{"kind":"create","name":"fixture.txt","path":"fixtures/fixture.txt","absolute_path":"/workspace/fixtures/fixture.txt","bytes":12}]}
```

The locked fields are `kind` (`write`, `create`, `edit`, or `delete`), `name`
(basename), `path` (workspace-relative), `absolute_path`, and unsigned `bytes`.
`/effects/0/path` and `/effects/0/name` identify the first applied effect.
Byte counts are the installed file size for write/create/edit and the removed
file size for delete. A move declares source delete then destination
create/write; a copy declares destination create/write. Structural directory
operations carry zero content bytes. Paths and sizes are captured by the
mutation, without rereading mutable paths after completion. Attempts that fail before applying a mutation do not invent effects. A failure
after application retains its effects and failed disposition; fatal storage or
ledger errors still fail the run. A fatal post-apply error records one failed
tool result with its landed effects before the existing fatal cleanup. Live
JSONL and replay retain the same facts, using the existing call ids, cursor
allocation rules, and durability boundaries.

Background task completion facts add optional `output_sha256`, the digest of
all observed output before ring-buffer retention. New `task_output` results
use it for both live and completed/evicted tasks. Legacy completion records
without an original digest remain valid and do not fabricate one. Delegated
report results hash the full child summary before bounding it, including any
existing report prefix; recollection derives that provenance from the retained
child journal. SSH shell result wires also add optional `truncation`; received
stdout/stderr bytes are hashed before their shared cap. Web-fetch provenance
covers observed response-body bytes before extraction/capping, including a
received overflow/look-ahead chunk; bytes never fetched remain outside that
count. Model-catalog truncation hashes its complete serialized filtered page.
Lockdown sandbox file-write paths are relative to that effective sandbox root.

## Exactly one typed terminal

An attached run ends with exactly one terminal envelope. It is still the
ordinary durable `payload.type == "run_state"` envelope. The journal retains
its additive `payload.terminal_kind`, and live JSONL and replay serialize that
same retained envelope:

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

Every field inside this terminal `RawEnvelope` is durable. Replay must preserve
the complete envelope and payload byte-for-byte; it may not reconstruct a
smaller terminal from `state`. A compatibility reader may deterministically
add terminal fields omitted by a pre-v0.0.970 journal row, but it does not
rewrite that retained row. Presentation-only derived fields stay outside the
durable payload on both live and replay paths.
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


## Logical request budgets (v0.0.970)

Every logical provider dispatch carries a durable `provider_request_budget_v1`
extension status with used requests, soft tranche, and hard cap. The default
is 32 / 64. The soft-bound note is both model-readable and visible; the hard
checkpoint commits with `run_failed { code: request_budget_exceeded }` and the
single `errored` terminal. CLI exit is 77, with continuation instructions.
These facts replay unchanged and do not discard prior text or tool results.

`haider run --request-tranche 32 --max-requests 96 -p 'task'` pins per-run
request policy. `haider run --resume RUN_ID --output jsonl` accepts a fresh
turn in the original headless root session, restoring tool history and the
source policy unless explicitly overridden. Its stream correlates the new
run and retains the ordinary contiguous cursor contract. The source run and
its terminal remain immutable. Interactive timelines and delegated children
continue through their existing new-turn and `message_subagent` surfaces.
