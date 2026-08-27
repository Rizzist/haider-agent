# Client contract v1 — wire enum audit

Status: normative appendix for the entries enumerated below
Method-tag and headless additions checked against source: 2026-08-27

This audit records the serialized enums enumerated below, including typed
decoders layered over `RawEnvelope.payload`. Its method-tag spellings and
v0.0.963 headless entries are current at the date above. It is not a refreshed
census of every direct enum added by intervening feature work; §14 of the main
contract supplies explicit supplemental classifications. Local-only Rust enums
such as `WireEncoding`, codec errors, palette implementation values, and
decoder state are outside the serialized client surface.

Every enum listed here has exactly one expansion class:

- **Extensible with Unknown**: the serialized type has `#[serde(other)]` or a
  custom unknown-string carrier. A new variant may use the same field/type; an
  old reader obtains `Unknown` and must keep it non-executable/non-healthy.
- **Raw-preserved**: the typed enum is closed, but it occurs as or below
  `RawEnvelope.payload: serde_json::Value`, an opaque extension value, or a
  native-pipe JSONL line whose unknown `kind` is retained/skipped. A client
  preserves the raw JSON before attempting typed decode. Expansion is made as
  a new raw payload/extension/line shape; failure of the old optional typed
  decode must not discard the surrounding record.
- **Frozen**: an old client must decode this enum directly to use the containing
  request/response/envelope coordinate. The enum cannot grow. Expansion
  requires a new optional field/type, an extensible replacement enum, a new
  feature-gated method, or a wire-version change.

An enum containing a literal variant named `Unknown` is not automatically
extensible. Without `#[serde(other)]` or custom decoding, that variant accepts
only the exact serialized string `"unknown"`; an unfamiliar future string
still fails. Those enums are frozen or raw-preserved as listed below.

## haider-rpc enums

| Enum | Class | Reader law |
|---|---|---|
| `WireFrame` | Extensible with Unknown | custom serde maps unknown `kind` to `Unknown`; top-level `v` remains strict |
| `RequestBody` | Extensible with Unknown | unknown `method` is non-executable and receives a protocol error from a current daemon |
| `ResponseBody` | Extensible with Unknown | unknown response method cannot be treated as success for a known operation |
| `ClientKind` | Extensible with Unknown | unknown client kind has no inferred policy |
| `Capability` | Extensible with Unknown | `Unknown` is never granted |
| `LifecyclePhase` | Extensible with Unknown | unknown phase is not `ready` |
| `ProviderApiFamilyWire` | Extensible with Unknown | display only; do not select a serializer |
| `ProviderAuthRequirementWire` | Extensible with Unknown | do not infer an auth method |
| `ProviderAvailabilityWire` | Extensible with Unknown | unknown is not healthy |
| `SnapshotAvailabilityWire` | Extensible with Unknown | unknown is neither available nor unavailable |
| `OAuthFlowStatusWire` | Extensible with Unknown | unknown is not ready and carries no usable credential reference |
| `AccountAddMethod` | Extensible with Unknown | unknown add method is non-executable |
| `StagePurpose` | Extensible with Unknown | daemon refuses an unknown purpose |
| `AttachMode` | Extensible with Unknown | unknown mode grants no authority |
| `SurfaceInjectOp` | Extensible with Unknown | unknown operation is not applied |
| `ObserveRunStateWire` | Extensible with Unknown | render an unknown state without inventing controls |
| `NeedsInputKindWire` | Extensible with Unknown | card remains displayable; typed styling is unknown |
| `FleetAgentStateWire` | Extensible with Unknown | do not map to done/failed |
| `SubmitDisposition` | Extensible with Unknown | receipt exists, but no known scheduling badge follows |
| `CancelStatus` | Extensible with Unknown | do not infer accepted or terminal |
| `ErrorData` | Extensible with Unknown | behavior follows the stable error code/retryable flag; never parse message |
| `WorkflowInstanceSourceV1` | Extensible with Unknown | do not infer built-in or user ownership for an unknown source |
| `ProviderRemoveRefusalReasonWire` | Extensible with Unknown | unknown refusal remains a refusal |
| `CommandOwnershipWire` | Extensible with Unknown | unknown ownership is non-executable |
| `CommandCatalogItemKindWire` | Extensible with Unknown | row may display generically |
| `CommandInvokeOutcomeWire` | Extensible with Unknown | unknown outcome is non-executable and not a receipt |
| `SessionKindWire` | Frozen | shipped `root | subagent`; add a replacement field/type for more kinds |
| `WaitingWhyKindWire` | Frozen | shipped three-kind legacy field; use extensible `NeedsInputKindWire` instead |
| `HookTrustStateWire` | Frozen | add a new trust field/type rather than another state |
| `MenuInput` | Frozen | new input forms require a new top-level field/type or method |

Provider-row health (`ProviderAvailabilityWire`) and whole-subsystem health
(`SnapshotAvailabilityWire`) are intentionally different types.

## haider-protocol enums: extensible with Unknown

| Module | Enums | Notes |
|---|---|---|
| `error` | `ErrorCode` | unknown code maps to `Unknown`; presentation remains available |
| `headless` | `RunBudgetDimensionV1` | an unknown budget dimension remains terminal but non-actionable |
| `provider` | `CacheBreakpointV1`, `CachePrefixMatchV1`, `CacheControlOmissionReasonV1`, `CacheControlObservationV1`, `CacheRewarmReasonV1`, `CacheMissClassificationV1`, `UsageRequestKind` | unknown cache evidence must not become a hit, miss, or available measurement |
| `session_fork` | `SessionForkMode`, `ForkContextEpoch`, `SessionForkEventPayload` | each has a serde catch-all; unknown event remains non-actionable |
| `usage` | `HaiderCodeAllowanceStateV1` | custom string decoder preserves the exact unknown provider string |
| `usage` | `HaiderCodePlanOutcomeV1`, `UsageHistoryRoleV1` | unknown outcome/role is not available, healthy, root, or subagent |
| `tool` | `ToolResultStatus` | unknown terminal result is not completed or successful |

## haider-protocol enums: raw-preserved

These enums are closed as typed Rust values. They may evolve only through the
raw event/extension rule: preserve `RawEnvelope.payload` first, attempt the
typed decoder second, and retain the envelope unchanged when that decoder does
not know the new shape.

| Module | Enums |
|---|---|
| `agent` | `AgentRole`, `Placement`, `ChipState`, `ReportVerification`, `AgentEventPayload` |
| `branch` | `BranchEventPayload` |
| `cache` | `CacheEpochTransitionReason` |
| `computer` | `ScrollDirection`, `ComputerAction` |
| `credential` | `RotationCause` |
| `effect` | `AuthorizationVerdict`, `AuthorizationSource`, `EffectPhase`, `EffectOutcome` |
| `history` | `NodeKind`, `CompactionResume`, `AnnotationKind`, `TodoState` |
| `headless` | `HeadlessRunEventPayload` |
| `hook` | `HookInput`, `HookEventPayload`, `HookRuntimeKind`, `HookDecisionKind`, `HookSubscriptionState`, `HookVerdict`, `StartupGateKind` |
| `item` | `CommandExecutionOrigin`, `TurnItem`, `ToolStatus`, `ItemEvent`, `ItemDelta`, `OutputStream` |
| crate root | `EventPayload` |
| `menu` | `MenuKind`, `ErrorRecoveryCardKind`, `EffectRecoveryAction`, `DecisionKind`, `MenuScope`, `MenuCloseReason`, `AnswerVia` |
| `permission` | `PermissionEventPayload`, `PermissionGrantAction`, `PermissionGrantResolution` |
| `pipe` | private serializer `SidecarRowKind` |
| `project_instructions` | `ProjectInstructionsEventPayload` |
| `provider` | `Block`, `StreamEvent`, `FinishReason` |
| `retry` | `RunRetryEventPayload` |
| `session` | `SessionConfigEventPayload` |
| `state` | `HarnessStatus`, `SessionState` |
| `task` | `TaskTerminalState`, `TaskCompletionDelivery`, `TaskEventPayload` |
| `verify` | `VerifyVerdict`, `Severity` |

Important consequences:

- `EffectOutcome::Unknown` and `ToolStatus::Unknown` accept only their known
  `"unknown"` literals. They are not serde catch-alls.
- `TurnItem::Extension { kind, data }`, whose wire tag is `item: "extension"`,
  is the preferred additive carrier for new item-level facts. The `data` value
  stays raw.
- `EventPayload` itself is frozen as a typed union but raw-preserved as a wire
  payload. New event families may also use their own additive typed union with
  `to_payload_value`/`from_payload_value` helpers.
- `TodoState` is frozen at `listed | processing | completed`; a new plan state
  needs a new plan item field/type.
- `Block` and `StreamEvent` are provider IR rather than standalone client RPC
  bodies. When provider-native material reaches durable client history it does
  so through raw/opaque payload data, so clients never require exhaustive
  direct decoding of those IR unions.

## haider-protocol enums: frozen

| Module | Enums | Why direct |
|---|---|---|
| `agent` | `AgentMessageDelivery` | nested in the direct `agent.message` receipt |
| `cache` | `CachePolicyMode` | nested in create/session metadata |
| `context` | `ContextFootprintTruth` | nested in summary/read/observe snapshots |
| `credential` | `AuthMethod`, `CredentialStatus`, `CredentialAttentionReason` | nested in direct account/provider/usage snapshots |
| `effect` | `EffectClass` | nested in direct tool inventory |
| `envelope` | `PromptRender` | an outer `RawEnvelope` field; raw payload preservation cannot rescue failure here |
| `error` | `ErrorScope`, `ErrorAction` | nested in direct structured presentations and errors |
| `graph` | `GraphGateKind`, `GraphExecutorShape`, `EvidenceAuthority`, `SubjectSelector`, `GraphTemplateRejection`, `EvidenceVerdict`, `GraphEvidenceSource`, `ComputerObservationKind`, `ChildWorkflowTrigger`, `GraphBlockReason`, `GraphPhase`, `GraphNodeAttemptOutcome`, `GraphRunScope` | reachable from direct graph inspect/status or Loom compiled records |
| crate root | `DeliveryMode` | direct turn-submit request field |
| `loom` | `LoomGate` | direct Loom registry/compiled workflow field |
| `permission` | `SystemPermission` | direct permission-action request/response field |
| `provider` | `CacheStatAvailability`, `ReasoningAccounting`, `UsageSource`, `FeatureResolve` | reachable from direct usage/cache or capability projections |
| `rpc` | `RpcOutcome`, `SubscriptionNotice` | legacy serialized RPC vocabulary has no catch-all; do not expand in place |
| `state` | `RunState`, `WaitReason`, `VerifyStep` | `RunState` is nested in direct `agent.message` receipts; its nested enums share that constraint |
| `tool` | `DispatchMode`, `ToolPermissionDefault`, `RememberedGrantScope`, `AttachmentBlock`, `PdfDeliveryMode` | direct inventory and turn-submit fields |
| `usage` | `AccountMeterStateV1` | direct `usage.report` field |

## Exact wire spellings by Rust enum

The classification tables use Rust enum type names. The values below are
the exact, case-sensitive serialized discriminants; they are not Rust
variant identifiers. For a tagged enum the tag field is shown in
parentheses. A `#[serde(other)]` arm also maps any unrecognized incoming
discriminant to Rust `Unknown`, subject to the reader law above.

Do not guess or normalize acronym/initialism spellings between types. Read a
variant's `#[serde(rename = "...")]` first and its enum-level serde rule second.
In particular, `ProviderAuthRequirementWire::OAuth` is `"o_auth"` because
serde's `snake_case` conversion sees the capital `O` separately from `Auth`
and the variant has no override (`crates/haider-rpc/src/frame.rs:792`), while
`AuthMethod::OAuth` is explicitly renamed to `"oauth"`
(`crates/haider-protocol/src/credential.rs:32-36`). These are two distinct
existing wire spellings.

The complete serialized-variant initialism/acronym sweep is below. Local-only
`WireEncoding::Json` and `CodecError::{InvalidUtf8, Json}` are excluded because
they have no wire discriminant. The detailed per-enum entries later in this
appendix list every spelling in context.

| Rust variant(s) | Exact wire spelling(s) | Source of spelling |
|---|---|---|
| `AuthMethod::{ApiKey, OAuth}` | `"api_key"`, `"oauth"` | snake case; explicit `OAuth` rename |
| `EffectClass::{FsRead, FsWrite, GuiAct}` | `"fs_read"`, `"fs_write"`, `"gui_act"` | snake case |
| `ErrorRecoveryCardKind::{OauthExpired, InvalidApiKey}` | `"oauth_expired"`, `"invalid_api_key"` | snake case |
| `AnswerVia::{Tui, Gui, Rpc}` | `"tui"`, `"gui"`, `"rpc"` | snake case |
| `CacheMissClassificationV1::SamePrefixInTtl` | `"same_prefix_in_ttl"` | snake case |
| `AttachmentBlock::Pdf` | `"pdf"` | snake case |
| `ClientKind::{Cli, Tui, Gui}` | `"cli"`, `"tui"`, `"gui"` | snake case |
| `ProviderApiFamilyWire::{OpenAiResponses, OpenAiChatCompletions}` | `"openai_responses"`, `"openai_chat_completions"` | explicit renames |
| `ProviderAuthRequirementWire::{ApiKey, OAuth}` | `"api_key"`, `"o_auth"` | snake case; `OAuth` is the shipped wart |
| `AccountAddMethod::OAuth` | `"oauth"` | explicit rename |
| `StagePurpose::ApiKey` | `"api_key"` | snake case |
| `RequestBody::{TurnSubmitFromCli, AccountLoginApi, AccountOAuthStart, AccountOAuthStatus, AccountOAuthCancel, AccountOAuthImport}` | `"turn.submit_from_cli"`, `"account.login_api"`, `"account.oauth_start"`, `"account.oauth_status"`, `"account.oauth_cancel"`, `"account.oauth_import"` | explicit method renames |
| `ResponseBody::{AccountLoginApi, AccountOAuthStart, AccountOAuthStatus, AccountOAuthCancel, AccountOAuthImport}` | `"account.login_api"`, `"account.oauth_start"`, `"account.oauth_status"`, `"account.oauth_cancel"`, `"account.oauth_import"` | explicit method renames |
| `ErrorData::{PdfTooLarge, PdfTooManyPages, PdfMalformed}` | `"pdf_too_large"`, `"pdf_too_many_pages"`, `"pdf_malformed"` | snake case |

### `crates/haider-protocol/src/agent.rs`

- `AgentRole` (`crates/haider-protocol/src/agent.rs:49`): `"head"` | `"subagent"` | `"orchestrator"`.
- `Placement` (`crates/haider-protocol/src/agent.rs:59`; `placement` tag): `"local"` | `"device"`.
- `ChipState` (`crates/haider-protocol/src/agent.rs:100`):
  `"idle"` | `"thinking"` | `"streaming"` | `"tool"` | `"waiting"` | `"input_required"` | `"permission_required"` | `"done"` | `"error"` | `"closed"`.
- `ReportVerification` (`crates/haider-protocol/src/agent.rs:126`):
  `"verified"` | `"red"` | `"waived"` | `"unverified"`.
- `AgentMessageDelivery` (`crates/haider-protocol/src/agent.rs:136`):
  `"delivered_steer"` | `"delivered_queued"` | `"delivered_subturn"`.
- `AgentEventPayload` (`crates/haider-protocol/src/agent.rs:245`; `type` tag):
  `"agent_messaged"` | `"agent_metrics"`.

### `crates/haider-protocol/src/branch.rs`

- `BranchEventPayload` (`crates/haider-protocol/src/branch.rs:36`; `type` tag): `"branch_created"`.

### `crates/haider-protocol/src/cache.rs`

- `CachePolicyMode` (`crates/haider-protocol/src/cache.rs:45`): `"economy"` | `"balanced"` | `"mobility"`.
- `CacheEpochTransitionReason` (`crates/haider-protocol/src/cache.rs:85`):
  `"configuration_changed"` | `"instructions_changed"` | `"tool_pack_changed"` | `"system_version_changed"` | `"web_tool_degradation"` | `"compaction"`.

### `crates/haider-protocol/src/computer.rs`

- `ScrollDirection` (`crates/haider-protocol/src/computer.rs:20`): `"up"` | `"down"` | `"left"` | `"right"`.
- `ComputerAction` (`crates/haider-protocol/src/computer.rs:34`; `action` tag):
  `"screenshot"` | `"cursor_position"` | `"inspect"` | `"left_click"` | `"right_click"` | `"middle_click"` | `"double_click"` | `"left_mouse_down"` | `"left_mouse_up"` | `"mouse_move"` | `"left_click_drag"` | `"type"` | `"key"` | `"scroll"` | `"wait"`.

### `crates/haider-protocol/src/context.rs`

- `ContextFootprintTruth` (`crates/haider-protocol/src/context.rs:14`): `"exact"` | `"estimated"`.

### `crates/haider-protocol/src/credential.rs`

- `AuthMethod` (`crates/haider-protocol/src/credential.rs:32`): `"api_key"` | `"oauth"`.
- `CredentialStatus` (`crates/haider-protocol/src/credential.rs:41`; `status` tag):
  `"ok"` | `"limited"` | `"expired"` | `"revoked"` | `"needs_attention"`.
- `CredentialAttentionReason` (`crates/haider-protocol/src/credential.rs:62`):
  `"keychain_denied"` | `"keychain_locked"` | `"keychain_missing"` | `"keychain_unavailable"`.
- `RotationCause` (`crates/haider-protocol/src/credential.rs:80`): `"rate_limit"` | `"error"` | `"manual"`.

### `crates/haider-protocol/src/effect.rs`

- `EffectClass` (`crates/haider-protocol/src/effect.rs:34`; `class` tag):
  `"fs_read"` | `"fs_write"` | `"process_exec"` | `"network"` | `"git_op"` | `"agent_spawn"` | `"credential_access"` | `"gui_act"` | `"screen_observe"` | `"screen_control"`.
- `AuthorizationVerdict` (`crates/haider-protocol/src/effect.rs:68`; `verdict` tag):
  `"allow"` | `"pre_authorized"` | `"ask"` | `"deny"`.
- `AuthorizationSource` (`crates/haider-protocol/src/effect.rs:86`): `"user_typed"`.
- `EffectPhase` (`crates/haider-protocol/src/effect.rs:93`; `phase` tag):
  `"intent"` | `"authorized"` | `"dispatched"` | `"outcome"`.
- `EffectOutcome` (`crates/haider-protocol/src/effect.rs:118`; `outcome` tag):
  `"ok"` | `"cancelled"` | `"cancelled_escalated"` | `"failed"` | `"unknown"`.

### `crates/haider-protocol/src/envelope.rs`

- `PromptRender` (`crates/haider-protocol/src/envelope.rs:27`): `"verbatim"` | `"pruned"` | `"omit"`.

### `crates/haider-protocol/src/error.rs`

- `ErrorScope` (`crates/haider-protocol/src/error.rs:77`):
  `"turn"` | `"session"` | `"profile"` | `"account"` | `"tool"`.
- `ErrorAction` (`crates/haider-protocol/src/error.rs:89`):
  `"retry"` | `"relogin"` | `"reimport"` | `"edit_key"` | `"switch_account"` | `"top_up"` | `"wait"` | `"choose_model"` | `"contact_admin"` | `"continue_partial"` | `"retry_fresh"` | `"none"`.
- `ErrorCode` (`crates/haider-protocol/src/error.rs:301`):
  `"invalid_argument"` | `"unknown_method"` | `"protocol_mismatch"` | `"unauthorized"` | `"credential_missing"` | `"credential_limited"` | `"session_not_found"` | `"run_not_active"` | `"menu_not_found"` | `"menu_already_answered"` | `"single_writer_violation"` | `"busy"` | `"revision_conflict"` | `"loop_limit"` | `"workflow_unfinished"` | `"graph_already_active"` | `"graph_not_active"` | `"graph_wrong_node"` | `"provider_error"` | `"provider_timeout"` | `"vision_unsupported"` | `"store_corrupt"` | `"store_locked"` | `"store_full"` | `"store_read_only"` | `"store_unavailable"` | `"permission_denied"` | `"effect_unknown_outcome"` | `"internal"` | `"budget_exhausted"` | `"unknown"`; any other string → Rust `Unknown`.

### `crates/haider-protocol/src/headless.rs`

- `HeadlessRunEventPayload` (`type` tag): `"headless_run_configured"` | `"run_budget_exhausted"`. Unknown headless event kinds remain preserved in `RawEnvelope.payload`; this focused typed decoder returns no value for them.
- `RunBudgetDimensionV1`: `"tokens"` | `"cost"` | `"time"` | `"unknown"`; any other string → Rust `Unknown`.

### `crates/haider-protocol/src/graph.rs`

- `GraphGateKind` (`crates/haider-protocol/src/graph.rs:129`; `kind` tag):
  `"command-green"` | `"all-of-n"` | `"human-confirm"`.
- `GraphExecutorShape` (`crates/haider-protocol/src/graph.rs:137`): `"inline"` | `"fan-out"` | `"human"`.
- `EvidenceAuthority` (`crates/haider-protocol/src/graph.rs:146`): `"daemon_verified"` | `"model_attested"`.
- `SubjectSelector` (`crates/haider-protocol/src/graph.rs:154`):
  `"workspace_revision"` | `"command"` | `"freeform"`.
- `GraphTemplateRejection` (`crates/haider-protocol/src/graph.rs:200`):
  `"duplicate_node"` | `"no_start"` | `"multiple_starts"` | `"cycle"` | `"unreachable_node"` | `"unknown_dependency"` | `"over_ceiling"` | `"invalid_gate"`.
- `EvidenceVerdict` (`crates/haider-protocol/src/graph.rs:250`): `"green"` | `"red"`.
- `GraphEvidenceSource` (`crates/haider-protocol/src/graph.rs:257`; `kind` tag):
  `"model"` | `"process_signal"` | `"workspace_mutation"` | `"computer_observation"` | `"child_contract"`.
- `ComputerObservationKind` (`crates/haider-protocol/src/graph.rs:301`): `"screenshot"` | `"inspect"`.
- `ChildWorkflowTrigger` (`crates/haider-protocol/src/graph.rs:570`):
  `"mutation_with_independent_verification"` | `"dependent_phases"` | `"fan_out"` | `"distinct_review"` | `"crash_recovery"`.
- `GraphBlockReason` (`crates/haider-protocol/src/graph.rs:779`):
  `"rounds-exhausted"` | `"no-progress"` | `"human-hold"`.
- `GraphPhase` (`crates/haider-protocol/src/graph.rs:817`):
  `"active"` | `"blocked"` | `"completed"` | `"abandoned"` | `"superseded"`.
- `GraphNodeAttemptOutcome` (`crates/haider-protocol/src/graph.rs:1103`):
  `"open"` | `"satisfied"` | `"retried"` | `"blocked"` | `"completed"` | `"abandoned"` | `"superseded"`.
- `GraphRunScope` (`crates/haider-protocol/src/graph.rs:1163`; `kind` tag):
  `"todo_child"` | `"run_set_aggregate"`.

### `crates/haider-protocol/src/history.rs`

- `NodeKind` (`crates/haider-protocol/src/history.rs:25`; `kind` tag):
  `"user_turn"` | `"assistant_commit"` | `"tool_exchange"` | `"child_spawn"` | `"child_result"` | `"compaction"` | `"result_import"` | `"annotation"` | `"todos"`.
- `CompactionResume` (`crates/haider-protocol/src/history.rs:76`): `"auto_mid_turn"` | `"manual_idle"`.
- `AnnotationKind` (`crates/haider-protocol/src/history.rs:102`):
  `"auto_title"` | `"blurb"` | `"head_callsign"` | `"other"`.
- `TodoState` (`crates/haider-protocol/src/history.rs:121`): `"listed"` | `"processing"` | `"completed"`.

### `crates/haider-protocol/src/hook.rs`

- `HookInput` (`crates/haider-protocol/src/hook.rs:18`; `event` tag): `"user_message"`.
- `HookEventPayload` (`crates/haider-protocol/src/hook.rs:52`; `type` tag):
  `"hook_notice"` | `"hook_fired"` | `"hook_subscription"` | `"update_available"` | `"account_expired"` | `"hook_run_trust"` | `"hook_trust_changed"`.
- `HookRuntimeKind` (`crates/haider-protocol/src/hook.rs:109`): `"exec"` | `"subscribe"` | `"decision"`.
- `HookDecisionKind` (`crates/haider-protocol/src/hook.rs:148`): `"allow"` | `"deny"`.
- `HookSubscriptionState` (`crates/haider-protocol/src/hook.rs:167`):
  `"started"` | `"exited"` | `"restart_scheduled"` | `"stopped"`.
- `HookVerdict` (`crates/haider-protocol/src/hook.rs:190`; `verdict` tag):
  `"allow"` | `"ask"` | `"deny"` | `"narrow_capability"` | `"mutate"`.
- `StartupGateKind` (`crates/haider-protocol/src/hook.rs:216`): `"trust_hook"` | `"update"`.

### `crates/haider-protocol/src/item.rs`

- `CommandExecutionOrigin` (`crates/haider-protocol/src/item.rs:25`): `"user_command"`.
- `TurnItem` (`crates/haider-protocol/src/item.rs:71`; `item` tag):
  `"agent_message"` | `"incomplete_agent_message"` | `"reasoning"` | `"tool_call"` | `"command_execution"` | `"file_change"` | `"child_spawn"` | `"child_result"` | `"plan"` | `"context_compaction"` | `"extension"` | `"refusal"`.
- `ToolStatus` (`crates/haider-protocol/src/item.rs:138`):
  `"pending"` | `"in_progress"` | `"completed"` | `"failed"` | `"cancelled"` | `"rejected"` | `"conflict"` | `"unknown"`.
- `ItemEvent` (`crates/haider-protocol/src/item.rs:158`; `event` tag): `"started"` | `"delta"` | `"completed"`.
- `ItemDelta` (`crates/haider-protocol/src/item.rs:166`; `delta` tag):
  `"text"` | `"reasoning"` | `"tool_args"` | `"command_output"`.
- `OutputStream` (`crates/haider-protocol/src/item.rs:186`): `"stdout"` | `"stderr"`.

### `crates/haider-protocol/src/lib.rs`

- `EventPayload` (`crates/haider-protocol/src/lib.rs:50`; `type` tag):
  `"harness_status"` | `"session_state"` | `"run_state"` | `"run_failed"` | `"client_diagnostic"` | `"idle_decayed"` | `"menu_opened"` | `"menu_answered"` | `"menu_closed"` | `"user_message"` | `"item"` | `"effect"` | `"tool_result"` | `"node_committed"` | `"agent_spawned"` | `"agent_report"` | `"agent_chip_state"` | `"gate_report"` | `"graph_pinned"` | `"graph_attempt_opened"` | `"evidence_recorded"` | `"graph_gate_satisfied"` | `"graph_advanced"` | `"graph_node_readied"` | `"graph_blocked"` | `"graph_completed"` | `"graph_abandoned"` | `"graph_superseded"` | `"graph_finalization_deferred"` | `"process_signal_recorded"` | `"graph_run_set_opened"` | `"todo_graph_attached"` | `"child_graph_attached"` | `"child_template_observed"` | `"child_template_promoted"` | `"rotation"` | `"usage"`.
- `DeliveryMode` (`crates/haider-protocol/src/lib.rs:134`): `"steer"` | `"queue"` | `"subturn"`.

### `crates/haider-protocol/src/loom.rs`

- `LoomGate` (`crates/haider-protocol/src/loom.rs:116`): `"cmd"` | `"ship"` | `"all-of"` | `"human"`.

### `crates/haider-protocol/src/menu.rs`

- `MenuKind` (`crates/haider-protocol/src/menu.rs:35`; `kind` tag):
  `"permission"` | `"recovery"` | `"error_recovery"` | `"exhausted"` | `"trust_hook"` | `"update"` | `"question"` | `"choice"` | `"secret"` | `"file"` | `"conflict"` | `"graph_human_confirm"` | `"graph_abandon_confirm"`.
- `ErrorRecoveryCardKind` (`crates/haider-protocol/src/menu.rs:106`):
  `"oauth_expired"` | `"invalid_api_key"` | `"account_revoked"` | `"account_deleted"` | `"rate_limit"` | `"quota_exhausted"` | `"partial_stream"` | `"keychain_relink"` | `"store_unwritable"` | `"generic"`.
- `EffectRecoveryAction` (`crates/haider-protocol/src/menu.rs:124`):
  `"probe"` | `"mark_done"` | `"retry"` | `"abandon"`.
- `DecisionKind` (`crates/haider-protocol/src/menu.rs:203`):
  `"allow_once"` | `"allow_always"` | `"reject_once"` | `"reject_always"`.
- `MenuScope` (`crates/haider-protocol/src/menu.rs:216`; `scope` tag):
  `"session"` | `"subagent"` | `"harness"`.
- `MenuCloseReason` (`crates/haider-protocol/src/menu.rs:241`):
  `"cancelled"` | `"dismissed"` | `"recovery_interrupted"`.
- `AnswerVia` (`crates/haider-protocol/src/menu.rs:250`):
  `"tui"` | `"gui"` | `"rpc"` | `"hook"` | `"voice"` | `"timeout"`.

### `crates/haider-protocol/src/permission.rs`

- `PermissionEventPayload` (`crates/haider-protocol/src/permission.rs:16`; `type` tag):
  `"permission_grant_needed"` | `"permission_grant_resolved"`.
- `SystemPermission` (`crates/haider-protocol/src/permission.rs:37`): `"screen_recording"` | `"accessibility"`.
- `PermissionGrantAction` (`crates/haider-protocol/src/permission.rs:61`):
  `"open_settings"` | `"retry"` | `"restart_daemon"`.
- `PermissionGrantResolution` (`crates/haider-protocol/src/permission.rs:100`):
  `"granted"` | `"timed_out"` | `"restart_required"` | `"cancelled"`.

### `crates/haider-protocol/src/pipe.rs`

- `SidecarRowKind` (`crates/haider-protocol/src/pipe.rs:738`): untagged; there is no enum
  discriminant string on the wire.

### `crates/haider-protocol/src/project_instructions.rs`

- `ProjectInstructionsEventPayload` (`crates/haider-protocol/src/project_instructions.rs:28`; `type` tag):
  `"project_instructions_loaded"`.

### `crates/haider-protocol/src/provider.rs`

- `Block` (`crates/haider-protocol/src/provider.rs:27`; `block` tag):
  `"text"` | `"reasoning"` | `"tool_call"` | `"tool_result"` | `"attachment"` | `"provider_opaque"`.
- `StreamEvent` (`crates/haider-protocol/src/provider.rs:60`; `event` tag):
  `"text_delta"` | `"reasoning_delta"` | `"refusal_delta"` | `"provider_opaque"` | `"tool_call_start"` | `"tool_call_args_delta"` | `"tool_call_end"` | `"server_tool_use"` | `"server_tool_result"` | `"web_sources"` | `"usage_update"` | `"finish"`.
- `FinishReason` (`crates/haider-protocol/src/provider.rs:117`):
  `"end_turn"` | `"tool_use"` | `"max_tokens"` | `"refusal"` | `"cancelled"` | `"error"` | `"pause_turn"`.
- `CacheBreakpointV1` (`crates/haider-protocol/src/provider.rs:238`):
  `"system"` | `"tools"` | `"history"` | `"unknown"`; any other string → Rust `Unknown`.
- `CachePrefixMatchV1` (`crates/haider-protocol/src/provider.rs:250`; `state` tag):
  `"same"` | `"changed"` | `"unavailable"` | `"unknown"`; any other string → Rust `Unknown`.
- `CacheControlOmissionReasonV1` (`crates/haider-protocol/src/provider.rs:264`):
  `"invalid_boundaries"` | `"missing_account_scope"` | `"provider_mismatch"` | `"unsupported_model"` | `"unverified"` | `"adapter_unavailable"` | `"unknown"`; any other string → Rust `Unknown`.
- `CacheControlObservationV1` (`crates/haider-protocol/src/provider.rs:281`; `state` tag):
  `"emitted"` | `"not_required"` | `"not_emitted"` | `"unavailable"` | `"unknown"`; any other string → Rust `Unknown`.
- `CacheRewarmReasonV1` (`crates/haider-protocol/src/provider.rs:299`):
  `"planned_compaction"` | `"configuration_change"` | `"unknown"`; any other string → Rust `Unknown`.
- `CacheMissClassificationV1` (`crates/haider-protocol/src/provider.rs:310`; `class` tag):
  `"prefix_changed"` | `"control_not_emitted"` | `"below_minimum"` | `"expired"` | `"planned_compaction"` | `"configuration_change"` | `"same_prefix_in_ttl"` | `"unavailable"` | `"unknown"`; any other string → Rust `Unknown`.
- `CacheStatAvailability` (`crates/haider-protocol/src/provider.rs:361`): `"present"` | `"unavailable"`.
- `ReasoningAccounting` (`crates/haider-protocol/src/provider.rs:370`):
  `"subset_of_output"` | `"additional_to_output"` | `"unavailable"`.
- `UsageRequestKind` (`crates/haider-protocol/src/provider.rs:422`):
  `"main_turn"` | `"compaction"` | `"delegated_agent"` | `"unknown"`; any other string → Rust `Unknown`.
- `UsageSource` (`crates/haider-protocol/src/provider.rs:496`):
  `"provider_reported"` | `"locally_exact"` | `"estimated"`.
- `FeatureResolve` (`crates/haider-protocol/src/provider.rs:518`):
  `"native"` | `"explicitly_emulated"` | `"unsupported"`.

### `crates/haider-protocol/src/retry.rs`

- `RunRetryEventPayload` (`crates/haider-protocol/src/retry.rs:13`; `type` tag): `"run_retried"`.

### `crates/haider-protocol/src/rpc.rs`

- `RpcOutcome` (`crates/haider-protocol/src/rpc.rs:56`): `"result"` | `"error"`.
- `SubscriptionNotice` (`crates/haider-protocol/src/rpc.rs:73`; `notice` tag):
  `"synchronized"` | `"lagged"` | `"ended"`.

### `crates/haider-protocol/src/session.rs`

- `SessionConfigEventPayload` (`crates/haider-protocol/src/session.rs:166`; `type` tag):
  `"model_selected"` | `"session_renamed"` | `"session_seen"` | `"effort_selected"` | `"fast_mode_selected"` | `"agent_type_selected"`.

### `crates/haider-protocol/src/session_fork.rs`

- `SessionForkMode` (`crates/haider-protocol/src/session_fork.rs:86`):
  `"fork"` | `"metafork"` | `"unknown"`; any other string → Rust `Unknown`.
- `ForkContextEpoch` (`crates/haider-protocol/src/session_fork.rs:96`):
  `"fresh"` | `"unknown"`; any other string → Rust `Unknown`.
- `SessionForkEventPayload` (`crates/haider-protocol/src/session_fork.rs:139`; `type` tag):
  `"session_forked"` | `"unknown"`; any other string → Rust `Unknown`.

### `crates/haider-protocol/src/state.rs`

- `HarnessStatus` (`crates/haider-protocol/src/state.rs:19`; `status` tag):
  `"starting"` | `"ready"` | `"shutting_down"`.
- `SessionState` (`crates/haider-protocol/src/state.rs:40`; `state` tag):
  `"created"` | `"restoring"` | `"idle"` | `"active_run"` | `"pausing"` | `"paused"` | `"closing"` | `"closed"`.
- `RunState` (`crates/haider-protocol/src/state.rs:55`; `state` tag):
  `"queued"` | `"thinking"` | `"streaming"` | `"running_tool"` | `"waiting"` | `"retrying"` | `"input_required"` | `"permission_required"` | `"compacting"` | `"verifying"` | `"concluding"` | `"effect_outcome_unknown"` | `"cancelling"` | `"done"` | `"errored"` | `"cancelled"`.
- `WaitReason` (`crates/haider-protocol/src/state.rs:105`; `reason` tag):
  `"provider_backoff"` | `"rate_limit"` | `"remote_child"` | `"local_child"` | `"device_unreachable"` | `"blocking_hook"` | `"dependency"` | `"verify_hold"` | `"verify_queue"` | `"workspace_settlement"` | `"workspace_verify"` | `"other"`.
- `VerifyStep` (`crates/haider-protocol/src/state.rs:129`): `"check"` | `"format"` | `"test"`.

### `crates/haider-protocol/src/task.rs`

- `TaskTerminalState` (`crates/haider-protocol/src/task.rs:28`; `state` tag):
  `"completed"` | `"failed"` | `"killed"`.
- `TaskCompletionDelivery` (`crates/haider-protocol/src/task.rs:45`):
  `"delivered_steer"` | `"delivered_queued"`.
- `TaskEventPayload` (`crates/haider-protocol/src/task.rs:99`; `type` tag):
  `"task_started"` | `"task_completed"`.

### `crates/haider-protocol/src/tool.rs`

- `DispatchMode` (`crates/haider-protocol/src/tool.rs:44`): `"await"` | `"fire_and_forget"` | `"deferred"`.
- `ToolPermissionDefault` (`crates/haider-protocol/src/tool.rs:57`):
  `"not_applicable"` | `"allow"` | `"ask"` | `"deny"`.
- `RememberedGrantScope` (`crates/haider-protocol/src/tool.rs:75`; `scope` tag): `"class"` | `"command_shape"`.
- `ToolResultStatus` (`crates/haider-protocol/src/tool.rs:147`):
  `"completed"` | `"rejected"` | `"conflict"` | `"failed"` | `"cancelled"` | `"unknown"`; any other string → Rust `Unknown`.
- `AttachmentBlock` (`crates/haider-protocol/src/tool.rs:180`; `kind` tag):
  `"image"` | `"pasted_text"` | `"file"` | `"pdf"` | `"skill"`.
- `PdfDeliveryMode` (`crates/haider-protocol/src/tool.rs:223`): `"native_document"` | `"extracted_text"`.

### `crates/haider-protocol/src/usage.rs`

- `HaiderCodeAllowanceStateV1` (`crates/haider-protocol/src/usage.rs:24`): `"ok"` is the one named value;
  every other string is preserved verbatim as Rust `Unknown(String)`.
- `HaiderCodePlanOutcomeV1` (`crates/haider-protocol/src/usage.rs:112`; `state` tag):
  `"available"` | `"indeterminate"` | `"halted"` | `"unauthorized"` | `"unknown"`; any other string → Rust `Unknown`.
- `AccountMeterStateV1` (`crates/haider-protocol/src/usage.rs:163`; `state` tag):
  `"metered"` | `"unavailable"` | `"local_only"`.
- `UsageHistoryRoleV1` (`crates/haider-protocol/src/usage.rs`; role field):
  `"root"` | `"subagent"` | `"unknown"`; any other string → Rust `Unknown`.

### `crates/haider-protocol/src/verify.rs`

- `VerifyVerdict` (`crates/haider-protocol/src/verify.rs:9`; `verdict` tag):
  `"verified"` | `"included_in_aggregate"` | `"deferred"` | `"waived"` | `"unverified"` | `"incomplete"` | `"acknowledged_red"` | `"errored_with_report"` | `"failed_env"` | `"not_applicable"`.
- `Severity` (`crates/haider-protocol/src/verify.rs:52`): `"error"` | `"warning"` | `"info"`.

### `crates/haider-rpc/src/command.rs`

- `CommandOwnershipWire` (`crates/haider-rpc/src/command.rs:15`):
  `"daemon_operation"` | `"client_view"` | `"unknown"`; any other string → Rust `Unknown`.
- `CommandCatalogItemKindWire` (`crates/haider-rpc/src/command.rs:434`):
  `"built_in"` | `"argument"` | `"custom"` | `"unknown"`; any other string → Rust `Unknown`.
- `CommandInvokeOutcomeWire` (`crates/haider-rpc/src/command.rs:531`; `kind` tag):
  `"receipt"` | `"parked"` | `"client_owned"` | `"unsupported"` | `"unknown"`; any other string → Rust `Unknown`.

### `crates/haider-rpc/src/frame.rs`

- `ClientKind` (`crates/haider-rpc/src/frame.rs:456`):
  `"cli"` | `"tui"` | `"gui"` | `"headless"` | `"unknown"`; any other string → Rust `Unknown`.
- `WorkflowInstanceSourceV1` (`crates/haider-rpc/src/frame.rs`):
  `"built_in"` | `"user"` | `"unknown"`; any other string → Rust `Unknown`.
- `Capability` (`crates/haider-rpc/src/frame.rs:472`):
  `"view"` | `"control"` | `"unknown"`; any other string → Rust `Unknown`.
- `LifecyclePhase` (`crates/haider-rpc/src/frame.rs:526`):
  `"starting"` | `"recovering"` | `"ready"` | `"draining"` | `"finalizing"` | `"stopped"` | `"failed"` | `"unknown"`; any other string → Rust `Unknown`.
- `ProviderApiFamilyWire` (`crates/haider-rpc/src/frame.rs:772`):
  `"anthropic_messages"` | `"openai_responses"` | `"openai_chat_completions"` | `"gemini_generate_content"` | `"unknown"`; any other string → Rust `Unknown`.
- `ProviderAuthRequirementWire` (`crates/haider-rpc/src/frame.rs:792`):
  `"api_key"` | `"o_auth"` | `"none"` | `"unknown"`; any other string → Rust `Unknown`.
- `ProviderAvailabilityWire` (`crates/haider-rpc/src/frame.rs:804`):
  `"available"` | `"unavailable"` | `"unknown"`; any other string → Rust `Unknown`.
- `SnapshotAvailabilityWire` (`crates/haider-rpc/src/frame.rs:819`; `state` tag):
  `"available"` | `"unavailable"` | `"unknown"`; any other string → Rust `Unknown`.
- `OAuthFlowStatusWire` (`crates/haider-rpc/src/frame.rs:926`; `status` tag):
  `"waiting_browser"` | `"waiting_device"` | `"exchanging"` | `"ready"` | `"failed"` | `"expired"` | `"cancelled"` | `"unknown"`; any other string → Rust `Unknown`.
- `OAuthImportSourceUnavailableCodeWire` (`crates/haider-rpc/src/frame.rs:807`):
  `"not_found"` | `"unreadable"` | `"unknown"`; any other string → Rust `Unknown`, while the containing reason's `message` remains available for display.
- `AccountAddMethod` (`crates/haider-rpc/src/frame.rs:948`):
  `"oauth"` | `"unknown"`; any other string → Rust `Unknown`.
- `StagePurpose` (`crates/haider-rpc/src/frame.rs:960`):
  `"api_key"` | `"menu_secret"` | `"unknown"`; any other string → Rust `Unknown`.
- `AttachMode` (`crates/haider-rpc/src/frame.rs:987`):
  `"view"` | `"control"` | `"unknown"`; any other string → Rust `Unknown`.
- `SessionKindWire` (`crates/haider-rpc/src/frame.rs:1168`): `"root"` | `"subagent"`.
- `SurfaceInjectOp` (`crates/haider-rpc/src/frame.rs:1228`; `kind` tag):
  `"set"` | `"insert"` | `"clear"` | `"submit"` | `"unknown"`; any other string → Rust `Unknown`.
- `ObserveRunStateWire` (`crates/haider-rpc/src/frame.rs:1266`):
  `"idle"` | `"running"` | `"effect_unknown"` | `"parked_permission"` | `"parked_input"` | `"errored"` | `"cancelled"` | `"unknown"`; any other string → Rust `Unknown`.
- `WaitingWhyKindWire` (`crates/haider-rpc/src/frame.rs:1282`): `"permission"` | `"question"` | `"approval"`.
- `NeedsInputKindWire` (`crates/haider-rpc/src/frame.rs:1303`):
  `"permission"` | `"question"` | `"approval"` | `"recovery"` | `"secret"` | `"update"` | `"trust_hook"` | `"choice"` | `"conflict"` | `"file"` | `"exhausted"` | `"unknown"`; any other string → Rust `Unknown`.
- `FleetAgentStateWire` (`crates/haider-rpc/src/frame.rs:1453`):
  `"queued"` | `"live"` | `"waiting"` | `"done"` | `"failed"` | `"cancelled"` | `"unknown"`; any other string → Rust `Unknown`.
- `HookTrustStateWire` (`crates/haider-rpc/src/frame.rs:1565`):
  `"trusted"` | `"untrusted"` | `"revoked_by_edit"`.
- `RequestBody` (`crates/haider-rpc/src/frame.rs:2426`; `method` tag):
  `"daemon.shutdown"` | `"command.list"` | `"command.invoke"` | `"artifact.put"` | `"session.create"` | `"session.list"` | `"session.list_watch"` | `"session.surface_publish"` | `"session.surface_watch"` | `"session.input_inject"` | `"session.pipe_path"` | `"session.read"` | `"session.observe"` | `"session.observe_batch"` | `"session.fleet"` | `"graph.pin"` | `"graph.run_set.open"` | `"graph.switch"` | `"graph.abandon"` | `"graph.status"` | `"loom.list"` | `"loom.register_agent_type"` | `"loom.install.status"` | `"loom.register_workflow"` | `"graph.inspect"` | `"session.diagnostic"` | `"hooks.list"` | `"hooks.trust"` | `"hooks.revoke"` | `"session.attach"` | `"session.detach"` | `"branch.create"` | `"session.fork"` | `"session.metafork"` | `"agent.message"` | `"turn.submit"` | `"turn.submit_from_cli"` | `"turn.submit_with_hook_trust"` | `"queue.list"` | `"queue.remove"` | `"queue.promote_steer"` | `"turn.cancel"` | `"run.retry"` | `"session.compact"` | `"session.select_model"` | `"session.rename"` | `"session.seen"` | `"session.select_effort"` | `"session.select_agent_type"` | `"session.select_fast"` | `"shell.exec"` | `"tools.inventory"` | `"vault.stage"` | `"account.login_api"` | `"account.oauth_start"` | `"account.oauth_status"` | `"account.oauth_cancel"` | `"account.oauth_import_sources"` | `"account.oauth_import"` | `"account.device_candidates"` | `"account.import_device"` | `"account.add"` | `"account.set_active"` | `"account.remove"` | `"account.set_default_model"` | `"account.set_label"` | `"account.list_watch"` | `"account.list"` | `"provider.list"` | `"provider.models_refresh"` | `"provider.configure"` | `"provider.remove"` | `"transcription.secret_get"` | `"transcription.secret_set"` | `"usage.report"` | `"usage.history_day"` | `"usage.history_range"` | `"computer.permission_open_settings"` | `"workflow.instance"` | `"session.descendants.attach"` | `"monitor.list"` | `"monitor.register"` | `"monitor.remove"` | `"monitor.watch"` | `"loom.install.retry"` | `"loom.install.watch"` | `"headless.run.start"` | `"headless.run.status"` | `"headless.run.stop"`; any other string → Rust `Unknown`.
- `ResponseBody` (`crates/haider-rpc/src/frame.rs:3260`; `method` tag):
  `"daemon.shutdown"` | `"command.list"` | `"command.invoke"` | `"artifact.put"` | `"session.create"` | `"session.list"` | `"session.list_watch"` | `"session.surface_publish"` | `"session.surface_watch"` | `"session.input_inject"` | `"session.pipe_path"` | `"session.read"` | `"session.observe"` | `"session.observe_batch"` | `"session.fleet"` | `"queue.list"` | `"queue.remove"` | `"queue.promote_steer"` | `"graph.pin"` | `"graph.run_set.open"` | `"graph.switch"` | `"graph.abandon"` | `"graph.status"` | `"loom.list"` | `"loom.registered"` | `"loom.install.status"` | `"graph.inspect"` | `"session.diagnostic"` | `"hooks.list"` | `"hooks.trust"` | `"hooks.revoke"` | `"session.attach"` | `"session.detach"` | `"branch.create"` | `"session.fork"` | `"session.metafork"` | `"agent.message"` | `"turn.submit"` | `"turn.submit.on_branch"` | `"turn.cancel"` | `"run.retry"` | `"session.compact"` | `"session.compact.on_branch"` | `"session.select_model"` | `"session.rename"` | `"session.seen"` | `"session.select_effort"` | `"session.select_agent_type"` | `"session.select_fast"` | `"shell.exec"` | `"tools.inventory"` | `"vault.stage"` | `"account.login_api"` | `"account.oauth_start"` | `"account.oauth_status"` | `"account.oauth_cancel"` | `"account.oauth_import_sources"` | `"account.oauth_import"` | `"account.device_candidates"` | `"account.import_device"` | `"account.add"` | `"account.set_active"` | `"account.remove"` | `"account.set_default_model"` | `"account.set_label"` | `"account.list_watch"` | `"account.list"` | `"provider.list"` | `"provider.models_refresh"` | `"provider.configure"` | `"provider.remove"` | `"transcription.secret_get"` | `"transcription.secret_set"` | `"usage.report"` | `"usage.history_day"` | `"usage.history_range"` | `"computer.permission_open_settings"` | `"menu.answer"` | `"error"` | `"workflow.instance"` | `"session.descendants.attach"` | `"monitor.list"` | `"monitor.register"` | `"monitor.remove"` | `"monitor.watch"` | `"loom.install.retry"` | `"loom.install.watch"` | `"headless.run.start"` | `"headless.run.status"` | `"headless.run.stop"`; any other string → Rust `Unknown`.
- `SubmitDisposition` (`crates/haider-rpc/src/frame.rs:3953`):
  `"started"` | `"queued"` | `"steer_pending"` | `"subturn_pending"` | `"unknown"`; any other string → Rust `Unknown`.
- `CancelStatus` (`crates/haider-rpc/src/frame.rs:3966`):
  `"accepted"` | `"already_terminal"` | `"unknown"`; any other string → Rust `Unknown`.
- `ErrorData` (`crates/haider-rpc/src/frame.rs:3981`; `kind` tag):
  `"artifact_too_large"` | `"attachment_not_found"` | `"attachment_mime_unsupported"` | `"attachment_too_large"` | `"pdf_too_large"` | `"pdf_too_many_pages"` | `"pdf_malformed"` | `"too_many_attachments"` | `"attachments_too_large"` | `"vision_unsupported"` | `"cursor_ahead"` | `"already_resolved"` | `"revision_conflict"` | `"surface_text_too_large"` | `"provider_models_unavailable"` | `"provider_unavailable"` | `"model_unknown"` | `"effort_unsupported"` | `"fast_unsupported"` | `"cache_epoch_confirmation_required"` | `"provider_remove_refused"` | `"workflow_revision_conflict"` | `"unknown"`; any other string → Rust `Unknown`.
- `ProviderRemoveRefusalReasonWire` (`crates/haider-rpc/src/frame.rs:3015`):
  `"not_found"` | `"release_owned"` | `"blocking_accounts"` | `"unknown"`; any other string → Rust `Unknown`.
- `MenuInput` (`crates/haider-rpc/src/frame.rs:3055`; `kind` tag): `"text"` | `"secret_vault_reference"`.
- `WireFrame` (`crates/haider-rpc/src/frame.rs:3238`; `kind` tag):
  `"hello"` | `"welcome"` | `"request"` | `"response"` | `"event"` | `"attach_caught_up"` | `"session_roster_delta"` | `"accounts_changed"` | `"haider_code_plan_status"` | `"resident_session_binding"` | `"session_surface_delta"` | `"session_input_injected"` | `"menu_answer"` | `"lagged"` | `"server_draining"` | `"ping"` | `"pong"` | `"protocol_error"` | `"unknown"`; any other string → Rust `Unknown`.

## Expansion checklist

Before adding any serialized variant:

1. Find the enum in this audit.
2. For **Extensible with Unknown**, add the variant and a golden old-reader
   decode proving the catch-all still receives a synthetic future value.
3. For **Raw-preserved**, add a raw-envelope or native-line fixture proving an
   old typed decoder may fail while the original JSON survives byte-for-value,
   plus a new typed opt-in decoder if needed.
4. For **Frozen**, do not add the variant. Add an optional replacement
   field/type or a feature-gated method and pin omission for the old shape.
5. Run both N-1 directions: old reader/new payload and new reader/old payload.

The additive law is a property of the named enum, not a convention to apply
from memory.
