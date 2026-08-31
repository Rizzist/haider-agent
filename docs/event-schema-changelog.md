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
