//! Transport-independent wire contracts for the Haider daemon.
//!
//! [`WireFrame`] is an explicit, versioned protocol union. It is not JSON-RPC:
//! the Hello/Welcome handshake uses JSON, after which peers may continue with
//! JSON or negotiate MessagePack. Unix-domain sockets add a four-byte
//! big-endian length prefix around either body encoding.
//!
//! # The sequence-cursor law
//!
//! [`haider_protocol::envelope::RawEnvelope::seq`] is the only replay cursor.
//! Event frames intentionally have no event ID, notification counter, snapshot
//! generation, or other competing resume position. A client resumes after the
//! greatest sequence it has fully applied.
//!
//! # Boundary
//!
//! This crate contains wire shapes, framing codecs, and handshake negotiation
//! only. It owns no listeners, connection/session policy, authorization,
//! persistence, or daemon lifecycle orchestration. `haider-daemon` builds those
//! semantics on this crate.

mod codec;
mod command;
mod frame;
mod negotiation;

pub mod uds_codec;
pub mod ws_codec;

/// Domain types carried by the RPC frames. Re-exporting the existing public
/// dependency lets transport clients reduce envelopes without adding a second
/// direct workspace dependency.
pub use haider_protocol;

pub use codec::{CodecError, WireEncoding, decode_msgpack, encode_msgpack};
pub use command::{
    COMMANDS, CommandCatalogItemKindWire, CommandCatalogItemWire, CommandDynamicSlotsWire,
    CommandInvokeOutcomeWire, CommandOwnershipWire, CommandSpec, PALETTE_MAX_ROWS, PaletteItem,
    command_catalog_items, command_spec, has_arg_slots, offers_arg_completions, palette_items,
};
pub use frame::{
    ARTIFACT_PUT_MAX_BYTES, AccountAddMethod, AttachMode, AttachState, AttachmentId, CancelStatus,
    Capability, CapabilitySet, ClientKind, CommandId, DEFAULT_FRAME_LIMIT,
    DESCENDANT_STREAM_MAX_CHILDREN, DescendantChangeKindWire, DescendantFanoutWire,
    DescendantIdentityWire, DescendantParentAnchorsWire, DescendantReplayCursorWire,
    DescendantStreamNodeWire, DescendantTruncationWire, DeviceCredentialCandidateWire,
    ERROR_CODE_ALREADY_RESOLVED, ERROR_CODE_ARTIFACT_TOO_LARGE,
    ERROR_CODE_ATTACHMENT_MIME_UNSUPPORTED, ERROR_CODE_ATTACHMENT_NOT_FOUND,
    ERROR_CODE_ATTACHMENT_TOO_LARGE, ERROR_CODE_ATTACHMENTS_TOO_LARGE, ERROR_CODE_BUSY,
    ERROR_CODE_CACHE_EPOCH_CONFIRMATION_REQUIRED, ERROR_CODE_CAPABILITY_DENIED,
    ERROR_CODE_CREDENTIAL_MISSING, ERROR_CODE_CURSOR_AHEAD, ERROR_CODE_DRAINING,
    ERROR_CODE_EFFORT_UNSUPPORTED, ERROR_CODE_FAST_UNSUPPORTED, ERROR_CODE_GRAPH_ALREADY_ACTIVE,
    ERROR_CODE_GRAPH_NOT_ACTIVE, ERROR_CODE_GRAPH_WRONG_NODE, ERROR_CODE_INVALID_ARGUMENT,
    ERROR_CODE_INVALID_CURSOR, ERROR_CODE_MODEL_UNKNOWN, ERROR_CODE_NOT_FOUND,
    ERROR_CODE_OAUTH_FLOW_NOT_FOUND, ERROR_CODE_OAUTH_UNAVAILABLE, ERROR_CODE_OVERLOADED,
    ERROR_CODE_PDF_MALFORMED, ERROR_CODE_PDF_TOO_LARGE, ERROR_CODE_PDF_TOO_MANY_PAGES,
    ERROR_CODE_PERMISSION_DENIED, ERROR_CODE_PROVIDER_ERROR, ERROR_CODE_PROVIDER_MODELS_UNKNOWN,
    ERROR_CODE_PROVIDER_REMOVE_REFUSED, ERROR_CODE_PROVIDER_UNAVAILABLE,
    ERROR_CODE_RESTAGE_REQUIRED, ERROR_CODE_REVISION_CONFLICT, ERROR_CODE_RUN_NOT_ACTIVE,
    ERROR_CODE_STALE_GENERATION, ERROR_CODE_SURFACE_TEXT_TOO_LARGE,
    ERROR_CODE_TOO_MANY_ATTACHMENTS, ERROR_CODE_UNAUTHORIZED, ERROR_CODE_UNSUPPORTED_SHELL_BUILTIN,
    ERROR_CODE_VAULT_UNSUPPORTED, ERROR_CODE_VISION_UNSUPPORTED, ErrorData,
    FEATURE_ACCOUNT_DEVICE_DISCOVERY_V1, FEATURE_ACCOUNT_LABEL_V1, FEATURE_ACCOUNT_LIST_WATCH_V1,
    FEATURE_ACCOUNT_LOGIN_API_V1, FEATURE_ACCOUNT_MANAGEMENT_V1, FEATURE_ACCOUNT_OAUTH_DEVICE_V1,
    FEATURE_ACCOUNT_OAUTH_IMPORT_SOURCES_V1, FEATURE_ACCOUNT_OAUTH_IMPORT_V1,
    FEATURE_ACCOUNT_OAUTH_PKCE_V1, FEATURE_ACCOUNT_ROTATION_V1, FEATURE_AGENT_MESSAGE_V1,
    FEATURE_ARTIFACT_PUT_V1, FEATURE_AUTONOMOUS_INTERACTION_V1, FEATURE_BRANCH_CREATE_V1,
    FEATURE_COMMAND_DOOR_V1, FEATURE_COMPACTION_GUARD_V1, FEATURE_COMPUTER_PERMISSION_ACTIONS_V1,
    FEATURE_CONTEXT_COMPACTION_V1, FEATURE_CONVERGENCE_GRAPH_V1, FEATURE_CONVERGENCE_GRAPH_V2,
    FEATURE_CONVERGENCE_GRAPH_V3, FEATURE_CONVERGENCE_GRAPH_V4, FEATURE_EFFECT_RECOVERY_V1,
    FEATURE_EXPORT_SEQ_V1, FEATURE_FALLBACK_CHAIN_V1, FEATURE_HAIDER_CODE_PLAN_STATUS_V1,
    FEATURE_HEADLESS_RUN_V1, FEATURE_HOOKS_SERVER_V1, FEATURE_HOOKS_V1,
    FEATURE_INPUT_MIRROR_ATTACHMENTS_V1, FEATURE_INPUT_MIRROR_V1, FEATURE_LOOM_AUTHORING_V1,
    FEATURE_LOOM_CLI_PRESENCE_V1, FEATURE_LOOM_PIPE_DAG_V1, FEATURE_LOOM_REGISTRY_ARCHIVE_V1,
    FEATURE_LOOM_REGISTRY_CAS_V1, FEATURE_LOOM_REGISTRY_WATCH_V1, FEATURE_LOOM_V1,
    FEATURE_LOOM_VALIDATION_V1, FEATURE_MODELS_LIST_V1, FEATURE_MONITOR_CONTROL_V1,
    FEATURE_MONITOR_DELIVERY_V1, FEATURE_MONITOR_V1, FEATURE_PIPE_NATIVE_V2,
    FEATURE_PIPE_TOOL_STATUS_V1, FEATURE_PROVIDER_CONFIGURE_V1, FEATURE_PROVIDER_MANAGEMENT_V1,
    FEATURE_PROVIDER_MODELS_V1, FEATURE_PROVIDER_REMOVE_V1, FEATURE_QUEUE_CONTROL_V1,
    FEATURE_RESIDENT_SESSION_BINDING_TOKEN_V1, FEATURE_RESIDENT_SESSION_BINDING_V1,
    FEATURE_RESIDENT_TURN_SUBMIT_V1, FEATURE_RUN_BUDGET_V1, FEATURE_RUN_RETRY_V1,
    FEATURE_SESSION_AGENT_TYPE_SELECT_V1, FEATURE_SESSION_ATTACH_SEALED_V1,
    FEATURE_SESSION_CONFIG_V1, FEATURE_SESSION_DESCENDANT_STREAM_V1,
    FEATURE_SESSION_EFFORT_SELECT_V1, FEATURE_SESSION_FAST_SELECT_V1, FEATURE_SESSION_FLEET_V1,
    FEATURE_SESSION_FORK_V1, FEATURE_SESSION_LINEAGE_V1, FEATURE_SESSION_LIST_WATCH_V1,
    FEATURE_SESSION_MODEL_SELECT_V1, FEATURE_SESSION_MUTATION_V1, FEATURE_SESSION_NEEDS_INPUT_V1,
    FEATURE_SESSION_OBSERVE_BATCH_V1, FEATURE_SESSION_OBSERVE_V1,
    FEATURE_SESSION_PERMISSION_OVERRIDES_V1, FEATURE_SESSION_RENAME_V1, FEATURE_SESSION_RUN_ID_V1,
    FEATURE_SESSION_SEEN_V1, FEATURE_SESSION_WORKFLOW_STATE_V1, FEATURE_SHELL_EXEC_V1,
    FEATURE_STATUS_SEGMENT_STRUCTURED_V1, FEATURE_STATUS_SEGMENT_V1, FEATURE_STORE_HEALTH_V1,
    FEATURE_TOOL_INVENTORY_V1, FEATURE_TRANSCRIPTION_V1, FEATURE_TUI_ATTACH_ANNOUNCE_V1,
    FEATURE_TURN_CONTROL_V1, FEATURE_TYPED_AGENT_INSTALL_CANCEL_V1,
    FEATURE_TYPED_AGENT_INSTALL_CONTROL_V1, FEATURE_TYPED_AGENT_INSTALL_V1,
    FEATURE_USAGE_HISTORY_V1, FEATURE_USAGE_REPORT_V1, FEATURE_USER_COMMAND_V1,
    FEATURE_VAULT_STAGE_V1, FEATURE_WIRE_MSGPACK_V1, FEATURE_WORKFLOW_CATALOG_V1,
    FEATURE_WORKFLOW_GRAPH_V1, FEATURE_WORKFLOW_INSTANCE_V1, FLEET_MAX_DEPTH, FLEET_MAX_NODES,
    FleetAgentStateWire, FleetMetricsTotalsWire, FleetNodeWire, FleetRollupWire,
    FleetStateCountsWire, Hello, HookSummaryWire, HookTrustStateWire, LifecyclePhase,
    LoomArchiveOutcomeWire, LoomArchiveReceiptWire, MODEL_INVENTORY_TTL_MS, MenuInput,
    ModelDetailWire, MonitorActionWire, MonitorControlPolicyWire, MonitorControlRejectionWire,
    MonitorDeliveryDedupeWire, MonitorDeliveryReportWire, MonitorEventPayloadWire,
    MonitorEventWire, MonitorFilterFieldWire, MonitorFilterOperatorWire, MonitorFilterWire,
    MonitorLifetimeWire, MonitorListOutcomeWire, MonitorListReceiptWire, MonitorOccurrenceWire,
    MonitorRegisterOutcomeWire, MonitorRegisterReceiptWire, MonitorRegistrationWire,
    MonitorRemoveOutcomeWire, MonitorRemoveReceiptWire, MonitorReportStatusWire,
    MonitorSourceAvailabilityStateWire, MonitorSourceAvailabilityWire, MonitorSourceKindWire,
    MonitorSourceUnavailableReasonWire, MonitorSourceWire, MonitorWatchOutcomeWire,
    MonitorWatchReceiptWire, NeedsInputKindWire, NeedsInputWire, OAuthAuthorizationWire,
    OAuthAvailabilityWire, OAuthFlowId, OAuthFlowStatusWire, OAuthImportSourceUnavailableCodeWire,
    OAuthImportSourceUnavailableReasonWire, OAuthImportSourceWire, OAuthReadyRefWire,
    ObserveMenuOptionWire, ObserveMenuWire, ObserveRunStateWire, ObserveSubagentWire,
    ProtocolError, ProviderActiveWire, ProviderApiFamilyWire, ProviderAuthRequirementWire,
    ProviderAvailabilityWire, ProviderDefaultWire, ProviderProbeFailureWire,
    ProviderRemoveRefusalReasonWire, ProviderSummaryWire, RESIDENT_BINDING_TOKEN_MAX_BYTES,
    RequestBody, RequestId, ResponseBody, SURFACE_INPUT_MAX_BYTES, SURFACE_STATUS_MAX_BYTES,
    SecretWire, SeqRange, SessionDescendantBaselineWire, SessionDescendantStreamEventWire,
    SessionFleetSnapshot, SessionKindWire, SessionObserveDigest, SessionReadResult, SessionSummary,
    SnapshotAvailabilityWire, StagePurpose, SubmitDisposition, SurfaceInjectOp,
    SurfaceInputPublishWire, SurfaceInputWire, SurfaceStatusPublishWire, SurfaceStatusWire,
    TodoGraphOpenedWire, TypedAgentInstallCancelOutcomeWire, TypedAgentInstallCancelReceiptWire,
    TypedAgentInstallRetryOutcomeWire, TypedAgentInstallRetryReceiptWire,
    TypedAgentInstallRetryRejectionWire, TypedAgentInstallWatchOutcomeWire,
    TypedAgentInstallWatchReceiptWire, TypedAgentInstallWatchRejectionWire, WIRE_PROTOCOL_VERSION,
    WaitingWhyKindWire, WaitingWhyWire, Welcome, WireFrame, WorkflowCatalogEntryV1,
    WorkflowInstanceSourceV1, WorkflowInstanceV1, resident_binding_token_is_valid,
};
pub use haider_protocol::queue::{QueueChange, QueueDelta, QueueRow};
pub use negotiation::{Negotiated, ServerRange, negotiate};

/// Crate marker used by the workspace self-test.
pub const CRATE_NAME: &str = "haider-rpc";
