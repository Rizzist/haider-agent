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
[haider:truncated truncated=true original_bytes=<uint> payload_bytes=<uint> sha256=<64 lowercase hex>]
```

`/truncation` is its typed mirror:

```json
{"truncated":true,"original_bytes":1048576,"payload_bytes":1234,"sha256":"<hex64>"}
```

For output with no redaction, `original_bytes` counts the original captured
bytes before the preview's reduction and `sha256` hashes those bytes. For an
output with any redaction, both fields instead describe the complete redacted
rendering before paging or head/tail reduction. This includes content-addressed
artifact references and process transcript digests published with the result.
Redacted file freshness uses a profile-derived, process-secret keyed digest
in the owner-local journal; after daemon restart a new read is needed before
writing that file. Headless run output never carries a redacted freshness
claim (keyed or not). A file mutation (`fs_write`, `fs_edit`, `fs_path`) whose
path or content would be redacted is marked `redacted_content: true` on its
`workspace_mutation` and `checkpoint_recorded` facts. For such a mutation the
agent-visible result omits `mutation_digest` and `subject_digest` (it keeps
`workspace_revision` and the `workspace_mutation` reference, which
`graph_evidence` resolves), and headless run output replaces its mutation
digest, checkpoint id and checkpoint digests with `withheld:` placeholders,
drops per-path pre-image references and truncation reasons, and omits the
tool result's file `effects` (exact byte counts). The exact facts remain in
`haider events`. Headless `menu_opened` omits `file_review`; its raw diff
digests belong to the interactive Ask surface only. `haider export --masked`
drops integrity digests from transcript previews.
Turn workspace tree receipts and process workspace mutation receipts report
generic incomplete coverage when a path or file content would be redacted.
They do not journal raw filenames, content digests, or byte counts for those
entries. A client may see `workspace tree receipt unavailable: redacted
material` or `reason=redacted_material` instead of an exact tree receipt.
For `fs_search` with redaction, `bytes_scanned` describes processed safe text;
unredacted searches retain their previous source-byte measure. Search matching
and columns use the redacted line. After a secret that spans lines (an open
quote or PEM block), the rest of that file reports line `0` (structured) and
`?` (preview) instead of a physical line number. Paths whose names would be
redacted appear as `[REDACTED:sensitive_path]` in search, glob and listings;
glob patterns (`fs_glob`, `fs_search` `glob` and `file_glob`) match that marker,
not the hidden name. In a file with redacted spans, `fs_edit` anchors match
only visible text: an anchor with no visible match, or whose match touches a
redacted span, gets the typed `anchor_in_redacted_content` refusal (every
anchor does, for a wholly redacted file), and match counts ignore redacted
bytes, so no edit outcome depends on a guess about a secret. Stale-read
refusals in headless output, transcripts and exports omit `current_digest` and
`recorded_digest`, as the provider projection does.
The historical field name remains for older decoders; clients must not use it
as an exact size or digest of a secret-bearing original. Old clients that use
these fields for raw capture integrity checks must treat a redacted result as
a different byte stream. For unredacted processes, stdout and stderr are
hashed in capture order. Bytes observed while draining after a process limit
also count; bytes never read from a terminated producer cannot count.
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

The model-facing view omits the process and filesystem mutation receipt
envelopes: it carries command output or mutation confirmation, a non-zero exit
code when present, and the truncation marker when applicable. A process that
fails without an exit code retains its terminal diagnosis. Output text is opaque;
receipt-shaped JSON read from a file or printed by a command is not unwrapped.
Digests, run/effect coordinates, limits and receipts remain in these durable
events and replay unchanged. Provider output-cost accounting measures the slim
view while preserving the journal's source-omission facts. Graph tools can use
`evidence_from=latest_process|latest_mutation|latest_subject` to resolve this run's
terminal journal facts without copying receipt coordinates into model context;
the store still validates graph authority, freshness and successful exit.

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

## Provider error detail: templates and owner-local raw text (v0.0.973)

Provider error prose is untrusted account data. A `RunFailed` presentation's
`detail` is always one of:

- a **known template** rendering (`haider-provider/src/error_templates.rs`):
  the whole message matched an anchored template for a known Anthropic,
  OpenAI/Codex, Gemini, DeepSeek or ACP error, and only typed slots vary
  and every slot value is corroborated or typed: `{model}` must equal the
  model id Haider requested (or a release-seeded Bedrock/Vertex catalog id),
  `{tool_call_id}` must be a tool-call id present in the request,
  `{request_id}` must equal the captured request-id header (or, when none was
  captured, pass the request-id shape policy and be at most 64 bytes),
  `{param}` names come from a closed list with numeric indices of at most 4
  digits, `{int}` is at most 12 digits (comma grouping such as `40,000`
  allowed), URLs are reduced to an allowlisted public host, and account ids
  and keys render as `[REDACTED]`. An uncorroborated slot makes the whole
  message unknown; or
- the provider-class default explanation followed by ` · details withheld`.

When the prose matched no template, its raw text (credential redactor
applied, control/bidi characters removed, at most 2048 bytes) is kept in
`presentation.provider_raw_detail`, labelled **"Provider detail (local
only)"**. Surfaces:

| Surface | Class | `provider_raw_detail` |
|---|---|---|
| Local journal (`store.sqlite` `run_failed` events) | owner-local | kept |
| TUI error card, recovery-menu card, plain renderer | owner-local | shown |
| `haider run --output print` stderr | owner-local | shown |
| `haider run --output json` / `--output jsonl` stdout (and stderr in those modes) | shareable | stripped from every envelope and from `error.presentation` |
| `haider run --replay <run-id>` (`haider.run.replay.v1`) and SDK `headless_run_events` | shareable | stripped (every headless event ledger strips on record) |
| Session pipe/sidecar rows (`profile/pipe/*.pipe`) | peer/model readable | stripped |
| Rust SDK `HeadlessRunResult` (`events`, `failure.presentation`) | shareable | stripped; only the explicitly typed `provider_raw_detail_local` field carries it |
| `haider export` (masked and unmasked, every format) | shareable | stripped |
| Recovery menus, sub-agent/delegation results, session transcript and pipe rows, RunFailed `message` | model/peer visible | never present (built from `detail` / public message only) |
| Lockdown turns, including a failed manual `/compact` | templates only | stripped |
| Android / mobile chat projection | detail only | not rendered |

**Consumer registry.** `haider_protocol::error::RUN_FAILED_CONSUMERS` classifies
every production file that reads or writes `RunFailed` (owner-local, public
fields only, or shareable-and-stripped with the regression test that proves
it); a protocol test fails when a new consumer is not classified. Lockdown
turns never carry the raw field, including errors whose adapter message
embeds provider text (malformed frames). The output-token-limit retry reads
the provider's stated maximum from the in-memory/owner-local prose and uses
only the integer locally.

**Raw event-frame surfaces (owner-UID only).** The following deliver journal
envelopes as the daemon stores them, so they carry `provider_raw_detail`
verbatim: the RPC `WireFrame::Event` live attach and the descendant stream,
`session.read`, `haider events`, `haider session <id> --watch`, and the SDK
`observe_stream_*` family. Every daemon endpoint refuses a peer whose UID is
not the owner's, so these are owner-local surfaces; treat their output like
the local journal and do not paste it into shared places. `haider run
--replay` reads the same frames but strips the field before writing JSON.

Serialized `ProviderError` values (for example the idle-timeout
extension's `cause`) never carry the raw text: it is not a serialized field
of `ProviderError`, and messages that interpolate provider values are
published only through templates.

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

Logical request budgets are opt-in. An omitted policy is unbounded and emits no
`provider_request_budget_v1` status. With an explicit policy, every logical
provider dispatch carries that durable extension status with used requests,
soft tranche, and hard cap. The soft-bound note is both model-readable and
visible; the hard checkpoint commits with
`run_failed { code: request_budget_exceeded }` and the single `errored`
terminal. CLI exit is **78**, the stable dedicated internal request-ceiling
code (previously shared blocked code 77), with continuation instructions. These
facts replay unchanged and do not discard prior text or tool results.
Interactive TUI and plain transcript rendering suppress progress statuses;
bound checkpoints remain useful, and machine JSON/JSONL retains all facts for
an explicitly budgeted run.
Once a capped run emits its first request status, the TUI status strip and
`haider session <id>` show `request cap N (tranche T)` once; session JSON
exposes the selected run's optional `request_budget` object. Unbounded runs
omit both displays and the JSON field.

### Loop guards (`loop_limit`, v0.0.973)

Three turn-local loop guards stop genuinely stuck loops. None counts
productive work, and none is a request-count cap. They share one fingerprint
set per turn.

Fresh call IDs, request ordinals, usage updates, transport attempts, opaque
replay state, and the automatic `max_tokens` nudge are never progress.

The fingerprints are for comparison only. The journal and the model always
receive the original data. They use structural rules, not per-format noise
patterns:

- **All content:** Unicode NFKC, a Latin fold of look-alike Cyrillic/Greek
  letters, and collapsed whitespace.
- **Tool results and assistant text:** every digit run is masked, so latency,
  clock times, HTTP dates, epochs, counters, and numeric nonces never look new.
  Digits directly after a letter or `_` are kept (`chunk5`, `mod12`, `v0`),
  because they name something. Letters are never masked, so a new git SHA, UUID
  or encoded token is a new result.
- **Tool results only:** the result is compared as an unordered multiset. Each
  line's `,`/`;`/`:`-separated items are sorted, then the lines are sorted. A
  reordered list is therefore a repeat, while an added or removed line is
  new.
- **Assistant text:** case is folded.
- **Assistant text and argument strings:** a run of one repeated punctuation
  character is capped at three.
- **Arguments:** canonical JSON with ordered keys and exact numbers. Digits in
  argument strings are not masked, so a call with different arguments, such as
  another file, is a new call.

The three guards:

1. **Continuation guard.** It counts consecutive `max_tokens`/`pause_turn`
   finishes whose response added no progress. Here progress is new nonblank
   assistant text, a new (tool, arguments, result) call fingerprint, or a new
   provider-side tool result. The default allows eight; the ninth ends the turn
   with `run_failed { code: loop_limit }` (CLI exit 70).
2. **Repeated-tool-call guard** (result level; any finish reason, including
   ordinary `tool_use` rounds). It counts consecutive completed calls, local or
   provider-side, whose (tool name, canonical arguments, normalized result,
   truncation, images / error status) was already seen in the turn. Only a new
   call fingerprint resets it; **assistant text never does**.
   - After 30 such calls, before the next provider request, it commits one
     non-terminal `loop_suspected_v1` item with `guard: "repeated_tool_calls"`.
   - After 30 more, the turn ends with `loop_limit` (exit 70).
   - A pure loop of identical calls, with or without new narration in between,
     is steered before request 32 and stops before request 62.
3. **Repeated-action guard** (action level; result independent). It counts
   consecutive completed calls whose (tool name, canonical arguments) was
   already seen in the turn, with no new (tool, arguments) in between. A
   changing result does not reset it; only a new (tool, arguments) does, and
   assistant text never does.
   - After 100 such calls it commits one `loop_suspected_v1` item with
     `guard: "repeated_actions"`; after 100 more the turn ends with
     `loop_limit` (exit 70).
   - An identical call is steered before request 102 and stops before request
     202 even when its result changes every time (etags, request IDs, a
     growing log). A 150-poll of a changing resource is steered once and
     keeps going; polling longer than 200 identical calls belongs to a
     background task or a `monitor` watch, not to a repeated call.
   - Cycles (A, B, C, A, B, C, ...) and parallel batches count call by call.
   - **Screen steps are exempt while the screen changes.** A call to the
     registered `computer` or `mobile` tool whose arguments parse through
     that tool's typed operation parser is a screen step when it is a
     screenshot, accessibility tree or inspect (observation) or a swipe,
     scroll, tap/left-click or key (navigation). An observation whose result
     (image content address or UI tree) differs from the previous identical
     observation is not counted, and navigation is not counted while the
     latest observation was such a change. Exempt steps neither count nor
     reset the streak. Once an observation repeats the previous identical
     one, screen steps count again. The result-level guard still counts
     every screen step, so identical screenshots (stuck at the end of a list)
     are steered before request 32 and stop before request 62. Unbounded
     swipe + screenshot paging through changing content (for example 300
     pages) is not stopped. The same action strings inside any other tool's
     arguments are ordinary calls.

Both call guards commit their steer before the next provider request. The
`loop_suspected_v1` item carries `run_id`, `guard` (`repeated_tool_calls` or
`repeated_actions`; absent in pre-addendum journals, meaning
`repeated_tool_calls`), `repeated_calls`, `stop_after`, an optional `tool`, and
`label`. It is visible in JSON/JSONL and as a transcript line in the TUI and
plain renderers. The same typed note is added to the model's input and replays
from the journal on every later request, including after a daemon restart. The
note tells the model that writing text does not change the count, and to change
tool or arguments, use a background task or wait/monitor tool instead of
polling, or finish the turn. A new call that resets a streak re-arms its steer.
The steer always precedes the stop, even when a parallel batch crosses both
thresholds at once. When one batch crosses both steer thresholds, both steers
are committed.

**Where the details are.** Every `loop_limit` carries typed details in
`presentation.loop_limit`: on the `run_failed` event payload (JSONL, replay,
`session` journal) and on `error.presentation` of the `haider.run.v1` JSON. The
object is tagged by `loop`:

| `loop` | Fields |
|---|---|
| `no_progress_continuations` | `continuation_count`, `continuation_limit` |
| `repeated_tool_calls` | `repeated_calls`, `suspect_after`, `stop_after_suspected` |
| `repeated_actions` | `repeated_calls`, `suspect_after`, `stop_after_suspected` |

The field is additive and optional: it is absent on every other error code, and
readers must ignore unknown `loop` values. The top-level `run_failed` fields
(`code`, `message`, `retryable`, `presentation`) and `haider.run.v1` `error`
fields (`code`, `message`, `retryable`, `presentation`) are unchanged. The
`message` text repeats the counts for humans; do not parse it.

Accepted residuals:

- **Slowly varying arguments.** A call whose arguments change every time, such
  as `limit=N`, `offset=N` or `page=N` on the same resource, is a new action
  each time and is never counted by any call guard, even when the result is
  empty or identical. Only the continuation guard applies, and only across
  `max_tokens`/`pause_turn` finishes. Explicit request budgets
  (`--max-requests`) remain the bound for such a model.
- **Slow drip.** One genuinely new call at least every 100 calls (or every 30,
  when the repeats also repeat results) keeps resetting the streaks.
- **Letter-only result noise** on an identical call is a new result, so the
  result-level guard does not count it; the action guard bounds it at 200.
- **Changing screens.** Computer-use/mobile-use paging whose screenshots keep
  changing is never stopped by a call guard, including a stuck app whose
  screen changes only in pixels (a clock or animation). Other tools mixed into
  such a loop are still counted.
- **Digit-only result changes** on an identical call (`Completed files: N`,
  `stage N done`) are no new result, indistinguishable from a clock: such a loop
  stops after eight no-progress continuations or 60 repeated tool calls.
- **Repeated identical work that is productive** (for example 200 identical
  `git commit -am step` calls in one turn) is stopped by the action guard.
- The in-memory streaks restart after daemon recovery.

The call guards are on by default. Embedders may disable them or change all
four thresholds through `HarnessConfig::tool_loop_guard` (`ToolLoopGuardV1`).
There is no CLI flag. No guard is related to an explicit request budget or its
`request_budget_exceeded` exit 78.

`haider run --request-tranche 32 --max-requests 96 -p 'task'` pins per-run
request policy. With only `--max-requests N`, the implicit tranche is
`min(32, N)`; for example, `--max-requests 5` permits exactly five requests
without an earlier soft checkpoint. With only `--request-tranche N`, the
implicit hard cap is 64 (so N must be at most 64). An explicit tranche must
not exceed an explicit cap. Supplying neither flag leaves the run unbounded.
`haider run --resume RUN_ID --output jsonl` accepts a fresh
turn in the original headless root session, restoring tool history and the
source policy unless explicitly overridden. Its stream correlates the new
run and retains the ordinary contiguous cursor contract. The source run and
its terminal remain immutable. Interactive timelines and delegated children
continue through their existing new-turn and `message_subagent` surfaces.

Headless hard-cap terminals additionally retain `payload.terminal`, also exposed
as `terminal` in `haider.run.v1` JSON and `haider.run.replay.v1`. Its typed
`end_reason` is `harness_internal_ceiling`, `internal_cap_detected` is true,
and `exit_code` is 78. `ceilings` has `soft`, `hard`, and `used` logical requests;
`continuation` retains session/run and optional branch/agent coordinates.
`workspace_state` is `mutated` or `untouched`, computed by comparing pre/post
tree receipts, never by tool names or Git's dirty flag. `workspace_before` and
`workspace_after` retain their BLAKE3 identities. `partial_progress` contains
sorted `files_written` and `files_deleted`, `tool_calls` with a durable
result, and `last_request_ordinal` (physical requests, including retries).
File lists describe net changes, including process-created files, not authorship
or transient writes that were reverted. Receipt scope includes ignored/hidden
files and symlink targets without following them; Git administrative `.git`
directories/files are excluded. The baseline travels as a hidden, prompt-omitted
`turn_workspace_before_v1` extension in the first request-attempt transaction.
The final block shares the cap checkpoint/failure/terminal transaction. Replay
uses those retained bytes and does not rescan today's workspace.

The baseline is encoded in ordered 16 KiB chunks with a complete-content digest,
so no individual receipt event grows with the tree. Both observations stream
every included regular-file byte; the path map and total journal storage grow
with tree size. Capture runs off the async executor. It is a sequential receipt,
not an atomic filesystem snapshot; detected concurrent changes, unsupported
special files, or read failures make the observation unavailable. These errors
do not prevent a plain chat run or erase a known cap: the cap still exits 78 and
retains counts and continuation. In this exceptional case `workspace_state`,
`workspace_after`, and both file lists are omitted, with a typed
`workspace_receipt_error {phase: before|after, detail}`. No third workspace-state
value or false `untouched` claim is emitted. Legacy recovery without a baseline
uses the same explicit unavailability. Old capped journals have no terminal
receipt block; replay never fabricates one.

The adapter-manifest declaration, including its workspace template, is supplied
in [ceilingdecl evidence](testing/v0.0.970/ceilingdecl.md). Only typed terminal
evidence or the manifest-declared code may establish an internal cap; free-text
messages and a soft-tranche warning do not.

## Prompt retraction before response (v0.0.970)

`turn.retract`, negotiated as `turn_retract_v1`, retracts one accepted, active
turn before its first semantic provider response. It uses the same durable
cancellation intent and worker drain as `turn.cancel`. Acceptance atomically
commits `run_state: cancelling`, the receipt, and a new prompt-omitted fact:

```json
{"type":"prompt_retracted","prompt_seq":42,"prompt_node_id":"node-user-event","text":"editable prompt","attachments":[]}
```

`prompt_seq` names the original accepted `user_message` envelope;
`prompt_node_id` names its committed history node. `text` and the complete
attachment blocks retain the exact draft, including CAS artifact references.
The journal and original prompt stay append-only. Transcript and provider
history projections hide that prompt; raw JSONL and replay retain both the
original and the retraction fact in their original cursor order. Attachments
remain in CAS for composer restoration. The sole terminal is `run_state` with
`state: cancelled`, `terminal_kind: cancellation`, and additive `reason: retracted`.
The fact and terminal survive restart and replay with their retained bytes.

The session writer serializes retraction with a prompt-omitted
`response_started` envelope, whose `delta` is the first normalized provider
`StreamEvent`. This boundary precedes response item publication and delta
coalescing and does not depend on optional timing traces. Text, reasoning,
refusal, tool, opaque response, and source events cross it; empty deltas, finish, usage and
network-control events alone do not. If retraction commits first, an observed
response racing into this boundary becomes `response_delta_discarded`, retaining
`delta` plus `prompt_seq`, and is never applied to response items or history.
Unobserved bytes from the cancelled transport do not invent discarded facts.
If response commits first, RPC returns typed `too_late` without retracting;
the client falls back to one ordinary cancel of the same pinned run.

Request reservations and observed usage retain ordinary cancellation
accounting. A sent request with no final usage remains usage-unavailable under
the existing budget contract. No new request is issued by retract or replay.
SIGINT continues to mean ordinary durable `turn.cancel` with exit 130.

Native `.pipe` sidecars are materialized transcript views: retraction rebuilds
the view into a new generation and replaces the stable root, including crash
reconciliation. A reader retaining an old sidecar handle must reopen the root
to see this change. Full transcript exports omit the hidden prompt; incremental
exports cannot undo already-consumed display rows, so retraction-aware automation
uses raw journal replay and applies the fact to its own projection.
