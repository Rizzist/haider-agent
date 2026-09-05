# Event schema changelog

This is the additive-change ledger for the public automation event schema. The
law is strict: `RawEnvelope.schema_version` bumps only for a non-additive
change, and that bump ships with an upcaster plus golden old/new fixtures.
Adding an optional field or a new kind does not bump it. Consumers must ignore
unknown object fields and treat unknown payload, item, and terminal kinds as
additive—not as an empty result or a reason to stop the stream. The forward
reader is `RawEnvelope`, which preserves an unknown payload as JSON
(`crates/haider-protocol/src/envelope.rs:5-10`, `:14-15`, `:64-65`; the same
policy is stated at `crates/haider-protocol/src/lib.rs:1-11`).

The release history below is derived from the tag-to-tag diffs of the named
protocol sources, not from release memory. `v0.0.964` is the public contract
baseline; its one addition is established against `v0.0.963`, and the requested
`v0.0.964..v0.0.967` chain establishes every later entry. The baseline
inventories are included because a consumer beginning there must recognize
them and because the changelog pin needs a complete current kind set.

## `schema_version = 1`

`SCHEMA_VERSION` remains 1 (`crates/haider-protocol/src/envelope.rs:14-16`).

### v0.0.970 — durable tool discovery and slim provider results

`tool_result.result.data` adds `kind: "tools_discovered"` with `promoted`,
an ordered array of tool names. A successful actor-authored `list_tools` result
promotes only tools in the authorized catalog, after its result is committed.
Session reconstruction requires the matching started discovery call and a
completed result. Promotion survives turns, compaction and workspace selection;
a fork begins a new session scope. Discovery changes presentation, not permission.

Process and filesystem mutation receipts remain unchanged in durable results,
including `/effects[n]` and typed truncation provenance. Only the provider view
reduces them to output, non-zero exit and any required truncation marker; live
and history projections share that implementation. This changes no journal or
JSONL envelope shape and adds no schema-version bump.

### v0.0.970 — durable narrative correlation and compaction announcement

A primary emitted Finish with separate post-stream commits adds a prompt-omitted
`provider_round_terminal_v1` Started/Completed pair in one atomic append after
usage, before continuation.
It preserves the actual reason even if tools already settled before Finish.
The existing atomic final-text suffix instead retains the reason on its
completed item and terminal, preserving that batch. Accounting/delta
metadata never changes according to whether the budget guard flushes usage
before or after Finish. No existing request barrier is moved.

Existing `item` lifecycle events for assistant text (including incomplete text)
and reasoning summaries are the declared durable narrative capture points.
Their `payload.provider_request` adds the existing `ProviderRequestAttemptV1`
coordinates: session_id, run_id, turn_ordinal, request_ordinal, request_kind.
Actor-generated tool/result/state events carry the same optional object. After
an emitted provider Finish, `payload.provider_finish_reason` preserves that
provider reason. Private summarizer items add `provider_purpose: "compaction"`
and capture emitted deltas even on unsuccessful attempts. Without a provider
Finish their closing snapshots retain `provider_terminal_cause` (stream_error,
stream_eof, cancelled or guard/unsupported-response cause). Private summaries
remain in events/provider_rounds but never become the user-facing final response.
These fields are stamped before journal append, retain schema
version 1, and never expose provider-private reasoning that was not emitted.
The envelope already supplies committed_at_ms, schema_version, and seq.

The prompt/UI-omitted `provider_round_terminal_v1` extension records
known private-summarizer terminal outcomes when no narrative item exists to
carry them, including failed replay-open attempts before text-only fallback.
Its Started/Completed pair commits atomically and preserves the frozen item lifecycle.
It carries the same `provider_request` plus actual `provider_finish_reason` or
`provider_terminal_cause` (including `open_error`) metadata. Replayparity policy:
this extension is a durable journal fact replayed unchanged; it does not create
an empty assistant message or alter prompt compilation.

`journalview:context_compaction` is a new supplemental payload kind, serialized
as `type: "context_compaction"`. It commits atomically with the compaction item
and history node. It carries the run's turn_ordinal, successful summarizer
request_ordinal, operation_id, inclusive covers_from/covers_to node range,
summary_artifact, resume_cause, dropped_item_count, and retained_suffix_size.
Both counts explicitly use `provider_message` units. The dropped count measures
the active prefix actually replaced (including a previous summary once), not
re-expanded historical source messages or the number of journal events. The
retained suffix excludes the new summary and request-only scaffolding. The
journal is never deleted by prompt compaction.

`haider.run.v1` JSON and `haider.run.replay.v1` add the derived `provider_rounds`
array. Entries preserve request coordinates, emitted_text and reasoning_summary
item arrays, tool_calls, results, and terminal_cause. Narrative entries carry
item_id, text, completed, first_seq, last_seq, committed_at_ms, schema_version.
Deltas are assembled once per item/request; completion snapshots do not duplicate
their bytes. Incomplete items remain incomplete. Missing emitted reasoning is
an empty array; a missing terminal cause is null. Old uncorrelated journals
remain intact in events and do not acquire invented round coordinates.
Unknown future request metadata is omitted only from this derived projection;
it never prevents raw event serialization or replay.

Replayparity policy: **zero normalized fields inside RawEnvelope**. Correlation,
finish reason, and compaction scope are durable facts, identical in journal,
live JSONL, and replay. `provider_rounds` is derived container metadata computed
by the same reducer for live JSON and replay. Optional metadata is never added
by a stream serializer. Prompt replay ignores these metadata keys and the
prompt-omitted announcement; provider input semantics stay unchanged.
### v0.0.970 — per-session provider rebind

New supplemental kind: `session_config:session_provider_rebound`. The
prompt-omitted fact carries `rebind_id` (the command identity), `provider`,
optional `base_url`, and optional `account`; credentials are never included.
The event, session metadata update, and `session.provider.rebind` command
receipt commit atomically. Omitted endpoint/account coordinates clear the
previous session override. The selected model and conversation are preserved.
`SessionProviderRebound::apply_to_metadata` reconstructs the same route and
version from journal replay; an already admitted request retains its captured
adapter. Legacy metadata omits the additive `provider_base_url` and
`provider_rebind_id` fields. Raw readers preserve this additive kind without
changing schema version 1.

### v0.0.970 — recoverable invalid tool calls

`tool_result.result.data` adds `kind: "invalid_tool_call"` with `tool` and
`message` strings. The result has failed status and an `invalid-tool-call`
presentation subcode; its JSON preview also carries `error.kind:
"invalid_tool_call"` and repair instructions. Raw malformed arguments remain
in the failed tool item. Provider history uses an empty argument object paired
with this result, identically for live continuation and replay. One malformed
call permits a repair continuation within existing request/budget limits; a
second consecutive malformed call is a terminal provider failure. A valid
tool-argument frame resets this allowance.
The prompt-omitted `tool_call_repair_reset` extension records that reset before
dispatch, only after a malformed call. Recovery reads invalid results and reset
markers in durable order across request epochs, including deferred tool calls.

Unique case/underscore matches in the advertised tool pack use the canonical
name and add `tool_name_correction: {requested, resolved}` to the result preview.
Exact names take precedence, and ambiguous or unadvertised names are not repaired.
Legacy bounded results retain their existing encodings. The protocol round-trip
pin is `invalid_tool_call_data_has_a_typed_round_trip_without_changing_legacy_results`.

### v0.0.964 — contract baseline and additive payload

Diff audited for the baseline addition: `v0.0.963..v0.0.964` over the same
named protocol sources.

- New payload kind: `payload:checkpoint_recorded`
  (`crates/haider-protocol/src/lib.rs:146-147`; payload type at
  `crates/haider-protocol/src/checkpoint.rs:87-96`). A consumer upgrading from
  v0.0.963 had to preserve or project this append-only workspace-mutation fact.

A consumer beginning at the v0.0.964 baseline also had to preserve every
envelope field and understand—or ignore as additive—the following kinds.

Payload kinds (`EventPayload`, current type at
`crates/haider-protocol/src/lib.rs:55-158`; the `v0.0.964` type already includes
`Item` at tag lines 97-99):

- `payload:harness_status`, `payload:session_state`, `payload:run_state`,
  `payload:run_failed`, `payload:client_diagnostic`, `payload:idle_decayed`
- `payload:menu_opened`, `payload:menu_answered`, `payload:menu_closed`,
  `payload:user_message`, `payload:queue_changed`, `payload:item`
- `payload:effect`, `payload:tool_result`, `payload:node_committed`
- `payload:agent_spawned`, `payload:agent_report`, `payload:agent_chip_state`
- `payload:gate_report`, `payload:graph_pinned`,
  `payload:graph_attempt_opened`, `payload:evidence_recorded`,
  `payload:graph_gate_satisfied`, `payload:graph_advanced`,
  `payload:graph_node_readied`, `payload:graph_blocked`,
  `payload:graph_completed`, `payload:graph_abandoned`,
  `payload:graph_superseded`, `payload:graph_finalization_deferred`,
  `payload:process_signal_recorded`, `payload:graph_run_set_opened`,
  `payload:todo_graph_attached`, `payload:child_graph_attached`,
  `payload:child_template_observed`, `payload:child_template_promoted`
- `payload:rotation`, `payload:usage`, `payload:checkpoint_recorded`

Item kinds (`TurnItem`, current type at
`crates/haider-protocol/src/item.rs:72-143`; all are present in the v0.0.964
baseline at tag lines 72-135):

- `item:agent_message`, `item:incomplete_agent_message`, `item:reasoning`,
  `item:tool_call`, `item:command_execution`, `item:file_change`
- `item:child_spawn`, `item:child_result`, `item:plan`,
  `item:context_compaction`, `item:extension`, `item:refusal`

Supplemental session-configuration kinds (the additive
`SessionConfigEventPayload` union) are
`session_config:model_selected`, `session_config:session_renamed`,
`session_config:session_seen`, `session_config:effort_selected`,
`session_config:fast_mode_selected`, and
`session_config:agent_type_selected`. They remain separate from the closed
core `EventPayload` union so older typed consumers can preserve them through
the raw-envelope path.

The five automation terminal kinds did not yet exist at this baseline.

### v0.0.965 — additive kinds

Diff audited: `v0.0.964..v0.0.965` over `lib.rs`, `item.rs`, `envelope.rs`,
`history.rs`, `agent.rs`, `interaction.rs`, and `state.rs`.

- New payload kinds: `payload:peer.message`, `payload:lockdown.refused`,
  `payload:lockdown.quota`, `payload:provider.trust_changed`, and
  `payload:provider.auth_changed`. The dotted serialized names are explicit
  Serde renames (`crates/haider-protocol/src/lib.rs:96-100`, `:148-157`). Their
  payload types are `PeerMessage` (`crates/haider-protocol/src/peer.rs:90-102`),
  `LockdownRefused` (`crates/haider-protocol/src/lockdown.rs:6-12`),
  `LockdownQuota` (`:14-20`), `ProviderTrustChanged` (`:22-28`), and
  `ProviderAuthChanged` (`:30-38`). A consumer had to add projections for these
  five facts or preserve/ignore them as unknown kinds.
- New history node kind: `peer_turn` (`NodeKind::PeerTurn` at
  `crates/haider-protocol/src/history.rs:26-38`). A history reducer had to keep
  the peer coordinates distinct from user-turn semantics.
- No envelope field, `TurnItem` kind, or automation terminal kind changed.

### v0.0.966 — additive wait reason

Diff audited: `v0.0.965..v0.0.966` over the same source set.

- New `WaitReason` kind: `network_unavailable`
  (`crates/haider-protocol/src/state.rs:104-111`). A state renderer had to add
  this reason; it must not relabel the provider as having failed. This was
  shipped as additive, but there is a historical compatibility limitation:
  `WaitReason` is a closed Serde enum nested in typed `RunState`, so an older
  consumer that bypasses `RawEnvelope` and deserializes the payload directly
  rejects the unknown reason instead of preserving it. Forward-compatible
  consumers must retain the raw envelope path for this case.
- No payload kind, envelope field, `TurnItem` kind, or automation terminal kind
  changed.

### v0.0.967 — additive field and terminal taxonomy

Diff audited: `v0.0.966..v0.0.967` over the same source set, plus the public
JSONL terminal type introduced in this release.

- New optional/defaulted item field: `context_compaction.tokens_estimated`
  (`crates/haider-protocol/src/item.rs:118-132`). Omission is `false`; a
  consumer that shows compaction counts had to distinguish estimates from
  non-estimated counts when it is true.
- New JSONL terminal kinds: `terminal:success`, `terminal:failure`,
  `terminal:cancellation`, `terminal:timeout`, and `terminal:provider_error`
  (`crates/haider-client/src/headless.rs:468-486`; normative meanings at
  `docs/jsonl-run-contract-v1.md:78-102`). An attached automation consumer had
  to accept all five and continue treating the envelope `seq` as the cursor.
- `AgentManifest::provider()` was added in `agent.rs`, but it is an accessor
  over the already-existing `coordinates` field and adds no serialized field
  or kind (`crates/haider-protocol/src/agent.rs:48-61`), so it is intentionally
  not a schema entry.
- `EventPayload::Item` was not added in this release. It is part of the
  v0.0.964 baseline above; the tag diff contains no `EventPayload` change.

### v0.0.968 — additive budget, deadline, and route-wait detail

Diff audited: `8952219..lane-968-resume` over `crates/haider-protocol` and
`crates/haider-core`, plus the integrated max-cost and public observation
surfaces.

- Current durable headless-kind inventory:
  `headless:headless_run_configured`, `headless:run_budget_exhausted`, and the
  additive `headless:run_deadline_exceeded`. The new deadline fact carries the
  required absolute `RunDeadlineExceededV1.deadline_unix_ms: u64`; it records
  one run-deadline terminal independently of provider timeout presentation.
- Additive new JSONL terminal kind: `terminal:budget`
  (`crates/haider-client/src/headless.rs:474`). It identifies the typed
  `budget_exhausted` run failure and remains distinct from caller timeout and
  provider failures.
- Additive optional/defaulted field: `RunBudgetExhaustedV1.decision`
  (`crates/haider-protocol/src/headless.rs:132-140`). Older facts may omit the
  field and still decode; a writer also omits it when no decision detail is
  available.
- Additive nested decision fields: `spent: u64`, defaulted
  `projected: Option<u64>`, `cap: u64`, and typed `reason`
  (`crates/haider-protocol/src/headless.rs:119-130`). An input may omit
  `projected`; a writer represents an unavailable projection as JSON `null`.
- Additive `reason.type` kinds: `actual_usage`, `time_elapsed`,
  `projected_request`, `pricing_unavailable`, and `usage_unavailable`, with
  `unknown` as the forward-reader fallback
  (`crates/haider-protocol/src/headless.rs:99-117`). The two unavailable kinds
  add required `provider` and `model` string fields
  (`crates/haider-protocol/src/headless.rs:106-113`).
- Additive public observation state `waiting_for_route` identifies a run
  durably parked only on confirmed network unavailability. Status snapshots
  add defaulted `waiting_for_route_count: u64`, omitted by writers when zero;
  older snapshots therefore decode as zero.
- Two durable `item:extension` subkinds support exact reconnect replay:
  `haider.route_replay_attempt.v1` carries `response_epoch`, while
  `haider.route_replay_event.v1` carries `response_epoch` and `stream_event`.
  They are prompt-omitted recovery metadata, not new core `TurnItem` variants.
- No envelope field or core `EventPayload`/`TurnItem` kind changed.
  `RunBudgetDimensionV1`, `HeadlessRunUsageV1`, and
  `HeadlessRunEventPayload::RunBudgetExhausted` all predate this release.

### v0.0.969 — tool-argument rejection clarification

- No automation schema changed. A model-authored tool call whose arguments are
  valid JSON but fail the tool's argument shape now settles through the
  existing `payload:tool_result` carrier with status `rejected` and a preview
  error kind of `invalid_argument`; the result is fed back for a corrective
  provider request. Previously the daemon incorrectly emitted a turn-scope
  `run_failed` and no tool result.
- The provider call id, durable sequence, JSONL framing, and exactly-one
  terminal rules are unchanged. This is a behavioral correction to the
  existing carrier, not an additive field or kind; the normative automation
  wording is in `docs/jsonl-run-contract-v1.md` under “Tool-call identity.”

### v0.0.970 — retained terminal projection and replay law

- New additive prompt-omitted monitor journal kinds:
  `monitor_registered`, `monitor_removed`, `monitor_tool_receipt`,
  `monitor_client_receipt`, `monitor_report_pending`, and
  `monitor_report_delivered`. Registrations add daemon-owned
  process/file/poll/timer/CLI source configuration and runtime fences; pending
  reports retain bounded typed source events, occurrence, omission counts,
  and the exact follow-up action. Raw-envelope readers must preserve unknown
  monitor kinds even when they do not project the monitor subsystem.
- Additive client wire surface: `monitor.mutate`, negotiated independently as
  `monitor_mutate_v1`, supplies receipt-backed update/pause/resume/trigger
  controls; monitor rows add defaulted state,
  last-event/fire-count/next-fire fields and a source summary; process and CLI
  delivery payloads add structured/terminal/exit-code detail; timer payloads
  add a tick counter; and `cli` joins the source-kind and availability unions.
  `SCHEMA_VERSION` remains 1 because these are additive kinds and fields.

- Additive durable terminal fields: the terminal `payload:run_state` now
  retains `terminal_kind` in the journal. Failure, budget, timeout, and
  provider-error terminals also retain `error_code`; success and ordinary
  cancellation terminals omit it. The values use
  the terminal vocabulary introduced in v0.0.967 and extended in v0.0.968.
  This changes no live JSONL shape: those fields were already emitted there.
  It makes session replay and `haider run --replay` serve the same retained
  terminal envelope instead of reconstructing a smaller payload. Readers of
  pre-v0.0.970 journals may add the missing fields with the documented
  deterministic classifier, but must not rewrite the retained journal row.
- Replay preservation rule: a durable event shape is additive-only and every
  addition must be announced in this ledger. Every field inside a
  `RawEnvelope`, including its payload, is durable and must survive replay
  with identical JSON encoding for the same retained row. There are no
  declared non-durable fields inside `RawEnvelope`, so live/replay event-byte
  comparisons normalize nothing. Acceptance announcements and replay-document
  metadata are separate protocol objects and are outside that comparison.
- Derived-field boundary: an ephemeral or presentation-only derived field
  never sits inside a durable payload. If a classifier is part of the durable
  event contract, as `terminal_kind` now is, the writer stamps it before
  commit and the journal retains it; subsequent live and replay surfaces
  serialize that retained value. Other derived values belong outside the
  `RawEnvelope` on both paths.

### v0.0.970 — provider request-attempt correlation

- New prompt-omitted payload kind `payload:provider_operation_reserved`
  records `request_kind` for session-owned provider inference that needs a
  durable turn identity but is not a conversation run. It contributes to the
  session-monotonic turn ordinal and is deliberately excluded from run-state
  observation, user hooks, and agent-usage timing. v0.0.970 uses it for
  `loom.author.draft` with `request_kind: side`. Session forks omit the entire
  reserved operation run because its correlation facts remain parent-owned.
- Additive optional field `cache_request_attempt_v1.correlation` records the
  exact turn-owned HTTP attempt identity: `session_id`, `run_id`, nonzero
  `turn_ordinal`, nonzero `request_ordinal`, and `request_kind` (`primary`,
  `side`, or reserved explicit `warmup`). The enclosing legacy `ordinal` and
  `correlation.request_ordinal` must agree. Older markers omit `correlation`
  and continue to decode; new writers always include it.
- New prompt-omitted `item:extension` subkind
  `provider_request_attempt_v1` carries the same five fields for turn-owned
  provider HTTP calls that have no prompt-cache diagnostic, including
  subscription `web_search`, session-owned `loom.author.draft`, Gemini
  cache-resource operations, and explicit connection prewarm. It commits
  before network I/O and shares the operation's physical request-ordinal
  allocator. Prewarm uses `warmup`; Loom, cache, and tool support use `side`.
- These markers are durable correlation metadata, not provider request-body
  content. `RawEnvelope.schema_version` remains 1. Readers must preserve an
  unknown extension subkind and may ignore it; recovery-aware readers that
  interpret it must reject zero, ambiguous, mismatched, or reused correlated
  coordinates as store corruption. Recovery tracks the first logical model
  boundary separately from the maximum physical ordinal so a preceding
  warmup/cache request cannot make an interrupted first model call reuse an
  identity; queued retries and manual compaction likewise resume at the
  validated maximum plus one.

The following payload kinds were present in the AHRB v0.0.969 capture but had
not all been called out together in this ledger. Their schema status is:

- `payload:session_state` — core `EventPayload` baseline kind from v0.0.964;
  its value is the typed session lifecycle state.
- `payload:usage` — core `EventPayload` baseline kind from v0.0.964; it carries
  the correlated typed token/cache usage record and is independent of the
  terminal classifier.
- `payload:effect` — core `EventPayload` baseline kind from v0.0.964; it
  carries the typed effect phase and must remain in the durable tool trace.
- `payload:node_committed` — core `EventPayload` baseline kind from v0.0.964;
  it carries the committed history `TreeNode`.
- `headless:headless_run_configured` — supplemental durable headless kind
  already present at the v0.0.964 baseline. It carries the fully resolved
  `HeadlessRunSpecV1` and is prompt-omitted replay metadata.
- `payload:session_renamed` — supplemental `SessionConfigEventPayload` kind,
  introduced before the v0.0.964 baseline. It carries optional `title`; an
  omitted title records that the title was cleared.
- `payload:process_signal_recorded` — core `EventPayload` baseline kind from
  v0.0.964; it carries the typed workflow/process-signal evidence fact.
- `payload:node_renamed` — observed by AHRB as an additive raw kind, but this
  repository's v0.0.969/v0.0.970 protocol sources and tag history contain no
  typed producer or decoder for it. No field schema is therefore claimed
  here. Forward readers must preserve its complete opaque JSON payload and
  must not reject, drop, or reinterpret it while replaying the envelope.
### v0.0.970 — workspace availability and recovery

- New additive workspace facts outside the closed core `EventPayload` union:
  `workspace:workspace_unavailable` and `workspace:workspace_selected`.
  The former carries the stored `path`, a typed `reason` (`missing`,
  `not_directory`, or `not_readable`), and a bounded `detail`; it is emitted
  once for each degraded turn. The latter records the canonical root selected
  by the receipt-backed `session.workspace.set` mutation and the additive
  optional `previous_path` used to retire old-root hook processes (absent only
  on legacy facts).
- New error code `workspace_unavailable` (presentation subcode
  `workspace-unavailable`). It classifies cwd-dependent tool/direct-shell
  refusals separately from provider failures.
- Raw-envelope and JSONL readers must preserve these additive facts. Existing
  valid-workspace streams and `schema_version = 1` remain byte-stable.

### v0.0.970 — session-list durable recency (RPC only)

- Additive `session.list.order` request field with values `id_asc` and
  `recency_desc`. Omission retains the original id-ascending behavior and is
  still omitted when encoded, so existing request bytes and meaning do not
  change. Daemons advertise support through `session_list_recency_v1`.
- In `recency_desc` mode, the opaque cursor represents the total durable key
  `(last_activity_ms DESC, session_id ASC)`. Clients pass it back verbatim.
  `SessionSummary.last_activity_ms` is the maximum of the indexed durable
  journal-head timestamp, shared `seen_at_ms`, and creation time. The value in
  each summary is exactly the value used for ordering and its cursor.
- Owner-approved semantic correction: `SessionSummary.last_activity_ms`
  previously meant only daemon-reduced user-relevant activity and could be
  absent for a cold session. From 0.0.970 it is the durable roster-recency
  scalar above and is populated for cold rows; older clients already tolerate
  the optional field and may continue treating it as a recency hint.
- No `RawEnvelope`, automation event kind, or `schema_version` changed. This
  entry records an additive RPC request field and the explicit summary-field
  semantic correction above.

### v0.0.970 — model catalog and selector guidance

- New effectless `list_models` agent tool. Its bounded result projects the
  daemon's already-discovered provider summaries and model-detail rows; it
  does not refresh inventory or perform a network request. The tool result is
  retained and replayed under the existing `payload:tool_result` contract.
- Additive `suggestions` array at
  `payload:tool_result.preview.error.details.suggestions` for typed
  `spawn_subagent` selector refusals. The daemon ranks and caps these catalog
  rows before committing the tool result. Existing `kind`, `model`,
  `provider`, `candidates`, and `inventory_age` fields keep their meanings;
  the refusal error codes are unchanged.
- Replay must emit the retained tool-result preview byte-for-byte. Suggestions
  are never recomputed or decorated during replay. `SCHEMA_VERSION` remains 1
  because this is an additive nested field on an existing payload kind.


### v0.0.970 — logical request budgets and continuation

Additive extension kind `provider_request_budget_v1` uses the existing
`item:extension` carrier. Its typed data records used logical requests, the
soft tranche and hard cap, a progress/soft-bound/hard-bound phase, and durable
session/run/branch/agent continuation coordinates. Only bound notes contribute
to model history; progress is UI/journal telemetry. Transport retries retain
the same logical count. Hard checkpoint items and the adjacent
`run_failed { code: request_budget_exceeded }` / `run_state: errored` pair are
one transaction, preserving the single-terminal law and replay parity.

Optional `RunBudgetV1.request_budget` and
`HeadlessRunSpecV1.continuation_of` fields are omitted for legacy values.
`spawn_subagent` can pin request policy in manifest coordinates. Capability
`request_budget_v1` is required for explicit policies and the dedicated resume
client so older daemons cannot silently ignore the settings. Default policy
is 32 soft / 64 hard. The schema version remains 1; unknown extension data and
new error codes retain the established forward-compatibility behavior.

### v0.0.970 — tool-result truncation provenance and applied file effects

- Optional `payload.truncation` and `payload.effects` on `tool_result`, mirrored
  on `payload.result` / standalone `BoundedResult`. They are absent when unused;
  legacy results decode with no metadata. `SCHEMA_VERSION` remains 1.
- Truncation has exactly `truncated: true`, unsigned `original_bytes` and
  `payload_bytes`, and `sha256` (64 lowercase hexadecimal digits of the ORIGINAL
  captured bytes). The text preview appends one standalone final line:
  `[haider:truncated truncated=true original_bytes=<uint> payload_bytes=<uint> sha256=<hex64>]`.
  The former preview and its prefix/suffix policy are retained; the new marker
  and its added separator are excluded from `payload_bytes`.
- Applied file effects are ordered `{kind,name,path,absolute_path,bytes}` records,
  with `kind` in `write|create|edit|delete`; relative and absolute paths agree with
  the workspace receipt. Moves declare source delete then destination create/write.
  Counts describe installed bytes (removed bytes for delete; zero for directory
  structure). Failures before application add no invented effect; post-apply
  failures retain effects while preserving the original failure disposition.
- SHA-256 is accumulated on original process bytes before lossless capture is
  bounded or bytes are converted to text; existing BLAKE3 receipt digests retain
  their meanings. Raw replay preserves the metadata. Fatal post-apply failures
  record one failed tool result with landed effects before existing cleanup;
  event kinds, terminal semantics, sequence allocation rules, and persistence
  boundaries stay the same. Details and byte-count scope are
  in [the run JSONL contract](jsonl-run-contract-v1.md#tool-result-byte-provenance-and-file-effects-v00970).

- Optional `output_sha256` on `task_completed` persists original background
  output provenance for eviction/recovery; legacy facts omit it. Optional
  `truncation` on `SshShellResultWire` carries the same typed captured-byte
  provenance. Both are additive and are omitted when unavailable/unused.
- v0.0.970 ceiling declaration: additive headless `run_state` terminal evidence
  (`terminal.end_reason = harness_internal_ceiling`, ceilings, continuation,
  pre/post workspace receipts and partial progress), retained unchanged in JSON
  and durable replay. Dedicated request-ceiling exit 78 replaces shared 77.
  The prompt-omitted `turn_workspace_before_v1` extension is committed before
  first dispatch. `schema_version` remains 1; legacy events omit these fields.
