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
