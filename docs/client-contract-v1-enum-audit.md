# Client contract v1 — wire enum audit

Status: normative appendix to [Haider client contract revision 1](client-contract-v1.md)  
Audited source snapshot: 2026-08-23

This audit covers every serialized enum reachable from the v1 client frame
surface in `haider-rpc` and `haider-protocol`, including typed decoders layered
over `RawEnvelope.payload`. It also records the few serialized protocol IR
enums that can be retained inside raw extension data. It excludes local-only
Rust enums such as `WireEncoding`, codec errors, palette implementation values,
and decoder state because they have no serialized client representation.

Every enum has exactly one expansion class:

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
| `provider` | `CacheBreakpointV1`, `CachePrefixMatchV1`, `CacheControlOmissionReasonV1`, `CacheControlObservationV1`, `CacheRewarmReasonV1`, `CacheMissClassificationV1`, `UsageRequestKind` | unknown cache evidence must not become a hit, miss, or available measurement |
| `session_fork` | `SessionForkMode`, `ForkContextEpoch`, `SessionForkEventPayload` | each has a serde catch-all; unknown event remains non-actionable |
| `usage` | `HaiderCodeAllowanceStateV1` | custom string decoder preserves the exact unknown provider string |
| `usage` | `HaiderCodePlanOutcomeV1` | unknown outcome is not available/healthy |

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
| `tool` | `ToolResultStatus` |
| `verify` | `VerifyVerdict`, `Severity` |

Important consequences:

- `EffectOutcome::Unknown`, `ToolStatus::Unknown`, and
  `ToolResultStatus::Unknown` accept only their known `"unknown"` literal.
  They are not serde catch-alls.
- `TurnItem::Extension { kind, data }` is the preferred additive carrier for
  new item-level facts. The `data` value stays raw.
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
