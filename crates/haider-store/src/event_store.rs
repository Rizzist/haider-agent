//! SQLite-backed event journal. Owns the sequence-allocation law:
//!
//! - `seq` is per-session, starts at 1, and is allocated only at commit time
//!   as `MAX(seq) + 1` inside an IMMEDIATE transaction, so committed
//!   sequences are monotonic and gap-free even across processes.
//! - An envelope is TRUE only once [`EventStore::append`] returns. Publishing
//!   committed envelopes to live subscribers is the caller's duty.
//! - The `envelope_json` column stores the authoritative encoded record: JSON
//!   text for legacy rows and MessagePack for current rows. Each row's bytes
//!   remain authoritative and immutable;
//!   the `seq` / `event_id` / `committed_at_ms` columns are denormalized
//!   copies for indexing, cross-checked against the record on every read.
//! - `worker_generation` is profile-owned and advances once per successful
//!   open while the exclusive profile lock is held, fencing actor identities
//!   across process restarts even when the wall clock repeats.

use crate::cas::FileCas;
use crate::migrations;
use crate::profile_lock::ProfileLock;
use crate::provider_view_store::{ProviderViewStore, default_expiry_ms};
use crate::usage_ledger::{
    UsageJournalReducer, UsageLedgerWriter, read_usage_day, slot_start_ms as usage_slot_start_ms,
};
use crate::{Cas, StoreResult, now_ms, store_error, to_sqlite_integer, validate_image_block};
use haider_protocol::agent::{AgentManifest, ChildReport, ReportVerification};
use haider_protocol::branch::{BranchCreated, BranchDescriptor};
use haider_protocol::cache::{ProviderViewBlobV1, ProviderViewBlockRefV1, ProviderViewLedgerV1};
use haider_protocol::credential::CredentialDescriptor;
use haider_protocol::effect::{
    AuthorizationVerdict, EffectClass, EffectOutcome, EffectPhase, WorkspaceMutation,
};
use haider_protocol::envelope::{
    EventEnvelope, PromptRender, RawEnvelope, RenderTargets, SCHEMA_VERSION, envelope_weight_bytes,
};
use haider_protocol::error::{ErrorAction, ErrorCode, ErrorPresentation, ErrorScope, HaiderError};
use haider_protocol::graph::{
    ChildContractRef, ChildGraphAttached, ChildTemplateCacheKey, ChildTemplateObserved,
    ChildTemplatePromoted, ComputerObservationKind, EvidenceAuthority, EvidenceRecorded,
    EvidenceSlotSpec, EvidenceVerdict, GRAPH_INSPECT_MAX_RUNS,
    GRAPH_INSPECT_MAX_TOOL_SELECTION_ROWS, GRAPH_MAX_CONDITIONAL_HOPS, GRAPH_MAX_TODO_CHILDREN,
    GRAPH_TELEMETRY_MAX_ATTEMPT_ROWS, GRAPH_TELEMETRY_MAX_RUN_ROWS,
    GRAPH_TELEMETRY_MAX_TEMPLATE_ROWS, GraphAbandoned, GraphAdvanced, GraphAttemptOpened,
    GraphBlockReason, GraphBlocked, GraphCompleted, GraphEvidenceProvenanceRow,
    GraphEvidenceSource, GraphFinalizationDeferred, GraphGateKind, GraphGateSatisfied,
    GraphInspectSnapshot, GraphNodeAttemptRow, GraphNodeName, GraphNodeReadied, GraphPhase,
    GraphPinned, GraphReduction, GraphReductions, GraphRunRow, GraphRunScope, GraphRunSetOpened,
    GraphSignalProvenance, GraphStatus, GraphSuperseded, GraphTelemetryAccumulator,
    GraphTelemetryProjection, GraphTemplateRejection, GraphTemplateRollup, GraphTemplateSpec,
    GraphWorkspaceMutationProvenance, ProcessSignalRecorded, ProcessSignalRef, SubjectSelector,
    TodoGraphAttached, WORKFLOW_GRAPH_WATCH_MAX_EVENTS, WorkflowActivationCause, WorkflowEdgeKind,
    WorkflowGraphJournalEvent, WorkflowGraphPhase, WorkflowGraphStarted, WorkflowGraphState,
    WorkflowGraphWatchEvent, WorkflowGraphWatchPage, WorkflowNodeActivated, WorkflowNodeCompleted,
    WorkflowNodeInput, WorkflowNodePhase, WorkflowNodeRejectCode, WorkflowNodeRejected,
    WorkspaceMutationRef, build_node, child_contract_subject_digest, child_gate_structure,
    computer_observation_subject_digest, evidence_fingerprint, graph_template,
    graph_template_digest, graph_template_rollups, normalize_evidence_detail,
    process_signal_subject_digest, reduce_graphs, todo_child_graph_id, todo_run_set_id,
    validate_graph_template, workflow_activation_ast_digest, workflow_activation_ast_from_loom,
    workflow_evidence_ledger_digest, workflow_input_ledger_digest,
    workspace_mutation_subject_digest,
};
use haider_protocol::headless::HeadlessRunEventPayload;
use haider_protocol::history::{COMPACTION_INTENT_EXTENSION_KIND, NodeKind, TodoItem, TreeNode};
use haider_protocol::hook::HookEventPayload;
use haider_protocol::ids::{
    AgentId, ArtifactRef, BranchId, DeviceId, EffectId, EventId, GraphId, GraphRunSetId, ItemId,
    MenuId, NodeId, RunId, SessionId, WorkspaceRevision,
};
use haider_protocol::item::{CommandExecutionOrigin, ItemEvent, TurnItem, UserCommandOriginV1};
use haider_protocol::loom::{
    LoomAgentType, LoomRegistration, LoomRegistryDelta, LoomRegistryDeltaKind,
    LoomRegistryEntryKind, LoomRegistryEntryRef, LoomRegistryRecord, LoomRegistrySnapshot,
    LoomRegistrySnapshotEntry, LoomRevisionConflict, LoomRevisionExpectation, LoomWorkflow,
    compile_pipe, parse_pipe,
};
use haider_protocol::menu::{Menu, MenuAnswer, MenuCloseReason, MenuKind};
use haider_protocol::permission::PermissionEventPayload;
use haider_protocol::pipe::InstructEvidenceRef;
use haider_protocol::project_instructions::ProjectInstructionsLoaded;
use haider_protocol::queue::{QueueChange, QueueDelta, QueueRow};
use haider_protocol::retry::RunRetryEventPayload;
use haider_protocol::session::{
    EffortSelected, FastModeSelected, ModelSelected, SessionMetadataV1,
    SessionPermissionOverridesV1,
};
use haider_protocol::session_fork::{
    ForkCacheSegmentV1, ForkContextEpoch, SessionForkMode, SessionForked, SessionHistoryOmission,
    SessionMetaforkProposal, SessionMetaforkReviewManifest,
};
use haider_protocol::state::{RunState, SessionState};
use haider_protocol::task::TaskEventPayload;
use haider_protocol::tool::{AttachmentBlock, ImageBlockRef};
use haider_protocol::typed_agent::{
    TYPED_AGENT_INSTALL_STATUS_MAX_JOBS, TYPED_AGENT_INSTALL_WATCH_PAGE_MAX_EVENTS,
    TypedAgentContract, TypedAgentContractError, TypedAgentInstallEvent, TypedAgentInstallItem,
    TypedAgentInstallJob, TypedAgentInstallProgress, TypedAgentInstallState,
    TypedAgentInstallTerminalStateV1, TypedAgentRequiredCli,
};
use haider_protocol::{DeliveryMode, EventPayload};
use rusqlite::{
    Connection, Error as SqliteError, ErrorCode as SqliteErrorCode, OptionalExtension, Transaction,
    TransactionBehavior, params,
    types::{Value as SqlValue, ValueRef},
};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const LOOM_AGENT_REVISIONS_DIR: &str = "loom-agent-revisions";
static LOOM_AGENT_REVISION_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
const REPLAY_PAGE_SIZE: usize = 1_024;
const USAGE_REDUCER_PAYLOAD_KINDS: &[&str] =
    &["usage", "agent_spawned", "run_failed", "session_forked"];
const QUEUE_REDUCER_PAYLOAD_KINDS: &[&str] = &["user_message", "queue_changed"];
const RUN_PROMPT_SOURCE_PAYLOAD_KINDS: &[&str] = &["user_message", "run_retried"];
const FAILED_TURN_REDUCER_PAYLOAD_KINDS: &[&str] = &["user_message", "run_failed", "run_retried"];
const TREE_HEAD_REDUCER_PAYLOAD_KINDS: &[&str] = &["node_committed"];
const USAGE_REPLAY_PAGE_BYTES: usize = 4 * 1_024 * 1_024;
const USAGE_CHECKPOINT_PROJECTION: &str = "usage_history";
const USAGE_CHECKPOINT_TIMELINE: &str = "session";
const USAGE_CHECKPOINT_SHAPE_VERSION: u32 = 1;
const USAGE_CHECKPOINT_REDUCER_VERSION: &str = "usage-history-v1";

#[derive(serde::Serialize, serde::Deserialize)]
struct DurableUsageCheckpoint {
    shape_version: u32,
    reducer_version: String,
    through_seq: u64,
    reducer: UsageJournalReducer,
}

#[derive(serde::Serialize)]
struct DurableUsageCheckpointRef<'a> {
    shape_version: u32,
    reducer_version: &'static str,
    through_seq: u64,
    reducer: &'a UsageJournalReducer,
}

/// Deployment escape hatch for the journal connection's SQLite `synchronous`
/// pragma. Accepted values are the lower-case strings `normal` and `full`.
const STORE_SYNCHRONOUS_ENV: &str = "HAIDER_STORE_SYNCHRONOUS";

/// The default WAL commit policy. `NORMAL` can lose commits from the most
/// recent checkpoint window after an OS crash or power loss; the WAL itself is
/// not corrupted. This is deliberate: on macOS, `FULL` without `F_FULLFSYNC`
/// already does not promise power-cut survival, so `NORMAL` states that
/// boundary honestly while making the commit path roughly 3–10× faster.
/// Deployments that require SQLite's `FULL` mode can set
/// `HAIDER_STORE_SYNCHRONOUS=full`; both modes are applied on every open.
const DEFAULT_STORE_SYNCHRONOUS: StoreSynchronous = StoreSynchronous::Normal;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StoreSynchronous {
    Normal,
    Full,
}

impl StoreSynchronous {
    const fn pragma_value(self) -> &'static str {
        match self {
            Self::Normal => "NORMAL",
            Self::Full => "FULL",
        }
    }
}

/// Device-profile-wide hard admission bound for durable live delegations.
///
/// Admission is serialized by the same SQLite `IMMEDIATE` transaction that
/// inserts a delegation. The count is always rebuilt from delegation rows and
/// each candidate child's exact durable run head; it is never cached in
/// process memory.
pub const SUBAGENT_LIVE_LIMIT: u64 = 512;

/// The inclusive sequence range allocated by one atomic append.
///
/// [`EventStore::append`] rejects empty batches, so it never returns an empty
/// range; `is_empty` exists for ranges constructed elsewhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommittedSeqRange {
    pub session_id: SessionId,
    pub first_seq: u64,
    pub last_seq: u64,
}

/// One payload-kind-filtered reducer page plus the committed journal head
/// observed under the same connection lock. The head carries only indexed
/// coordinates, so a reducer can advance across an irrelevant suffix without
/// fetching or decoding that suffix's envelopes.
#[derive(Debug, Clone, PartialEq)]
pub struct ReducerPage {
    pub envelopes: Vec<RawEnvelope>,
    pub observed_head: Option<(u64, EventId)>,
}

/// Opaque, rebuildable projection state anchored to one immutable journal
/// event. The event journal remains authoritative; consumers must reject an
/// unreadable payload and replay rather than infer state from these bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionProjectionCheckpoint {
    pub session_id: SessionId,
    pub projection: String,
    pub timeline_key: String,
    pub through_seq: u64,
    pub boundary_event_id: EventId,
    pub payload: Vec<u8>,
}

/// Durable coordinates for one menu-resolution compare-and-set.
///
/// `command_id` is the cross-connection idempotency key. The selected answer
/// stays in the ordinary protocol payload; this side structure supplies only
/// the version and fencing coordinates the transaction must validate.
#[derive(Debug, Clone, PartialEq)]
pub struct MenuResolutionCommand {
    pub command_id: String,
    pub session_id: SessionId,
    pub request_seq: u64,
    pub worker_generation: u64,
    /// Internal recovery authority. The daemon session actor may elevate this
    /// after registering the exact durable request-input checkpoint; the RPC
    /// command door may request it only after validating its own durable menu
    /// origin and exact answer coordinates. Ordinary wire menus remain false.
    pub allow_prior_generation: bool,
    pub answer: MenuAnswer,
    pub device_id: DeviceId,
    /// Preserves the wire distinction between ordinary text and a vault
    /// reference after both normalize into `MenuAnswer.value`.
    pub input_is_secret_reference: bool,
}

/// Result of the durable menu compare-and-set.
#[derive(Debug, Clone, PartialEq)]
pub enum MenuResolutionOutcome {
    /// This call appended the authoritative event. Publish this envelope only
    /// after the transaction has returned successfully.
    Committed {
        envelope: Box<RawEnvelope>,
        /// Graph SHIP answers settle their gate in the same transaction.
        /// Ordinary menus keep this empty.
        follow_up: Vec<RawEnvelope>,
        /// The validated opening card, returned so the daemon actor can run
        /// typed post-CAS recovery handlers without parsing display text.
        menu: Menu,
    },
    /// The same durable command was retried after its response was lost.
    IdempotentReplay { resolution_seq: u64 },
    /// A different command already resolved the menu.
    AlreadyResolved { resolution_seq: u64 },
}

/// Secret-free coordinates for one receipt-backed built-in graph pin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphPinCommand {
    pub command_id: String,
    pub request_digest: String,
    pub request_json: String,
    pub session_id: SessionId,
    pub worker_generation: u64,
    pub graph_id: GraphId,
    pub template: String,
    pub device_id: DeviceId,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PinnedGraph {
    pub session_id: SessionId,
    pub graph_id: GraphId,
    pub template: String,
    pub digest: String,
    pub pinned_seq: u64,
    pub opened_seq: u64,
    pub worker_generation: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum GraphPinOutcome {
    Committed {
        pinned: PinnedGraph,
        envelopes: Vec<RawEnvelope>,
    },
    IdempotentReplay {
        pinned: PinnedGraph,
    },
}

/// Receipt-backed cross-session attachment. The child graph has already been
/// pinned; this transaction proves the parent attempt/slot is still exact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildGraphAttachCommand {
    pub command_id: String,
    pub request_digest: String,
    pub request_json: String,
    pub session_id: SessionId,
    pub parent_branch_id: Option<BranchId>,
    pub worker_generation: u64,
    pub attachment: ChildGraphAttached,
    pub device_id: DeviceId,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AttachedChildGraph {
    pub parent_session_id: SessionId,
    pub child_session_id: SessionId,
    pub child_graph_id: GraphId,
    pub attached_seq: u64,
    pub worker_generation: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ChildGraphAttachOutcome {
    Committed {
        attached: AttachedChildGraph,
        envelopes: Vec<RawEnvelope>,
    },
    IdempotentReplay {
        attached: AttachedChildGraph,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildTemplateObservationCommand {
    pub key: ChildTemplateCacheKey,
    pub parent_session_id: SessionId,
    pub parent_attempt: haider_protocol::graph::ParentGraphAttempt,
    pub collapse_evidence_seq: u64,
    pub child_contract: ChildContractRef,
    pub template: GraphTemplateSpec,
    pub worker_generation: u64,
    pub device_id: DeviceId,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ChildTemplateCacheEntry {
    pub key: ChildTemplateCacheKey,
    pub template: GraphTemplateSpec,
    pub digest: String,
    pub distinct_attempts: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChildTemplateObservation {
    pub distinct_attempts: u32,
    pub promoted: bool,
    pub envelopes: Vec<RawEnvelope>,
}

/// Receipt-backed request to instantiate the selected template once per todo
/// in one exact G1 Plan fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphRunSetOpenCommand {
    pub command_id: String,
    pub request_digest: String,
    pub request_json: String,
    pub session_id: SessionId,
    pub worker_generation: u64,
    pub plan_item_id: ItemId,
    pub plan_event_seq: u64,
    pub device_id: DeviceId,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OpenedTodoGraph {
    pub todo_id: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depends_on_todo_id: Option<u32>,
    pub child_graph_id: GraphId,
    pub attached_seq: u64,
    pub pinned_seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opened_seq: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OpenedGraphRunSet {
    pub session_id: SessionId,
    pub run_set_id: GraphRunSetId,
    pub root_graph_id: GraphId,
    pub plan_item_id: ItemId,
    pub plan_event_seq: u64,
    pub template: String,
    pub digest: String,
    pub run_set_opened_seq: u64,
    pub through_seq: u64,
    pub children: Vec<OpenedTodoGraph>,
    pub worker_generation: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum GraphRunSetOpenOutcome {
    Committed {
        opened: OpenedGraphRunSet,
        envelopes: Vec<RawEnvelope>,
    },
    IdempotentReplay {
        opened: OpenedGraphRunSet,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphAbandonCommand {
    pub command_id: String,
    pub request_digest: String,
    pub request_json: String,
    pub session_id: SessionId,
    pub worker_generation: u64,
    pub why: String,
    pub device_id: DeviceId,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AbandonedGraph {
    pub session_id: SessionId,
    pub graph_id: GraphId,
    pub abandoned_seq: u64,
    pub worker_generation: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum GraphAbandonOutcome {
    Committed {
        abandoned: AbandonedGraph,
        envelopes: Vec<RawEnvelope>,
    },
    IdempotentReplay {
        abandoned: AbandonedGraph,
    },
}

/// Daemon-internal coordinates for the provider EndTurn guardrail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphFinalizationCommand {
    pub session_id: SessionId,
    pub branch_id: Option<BranchId>,
    pub run_id: RunId,
    pub worker_generation: u64,
    pub device_id: DeviceId,
}

/// Durable graph authority's decision at one provider finalization boundary.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum GraphFinalizationOutcome {
    AllowDone,
    Deferred {
        graph_id: GraphId,
        /// True only for the first committed deferral for `(graph, run)`.
        emit_reminder: bool,
        envelopes: Vec<RawEnvelope>,
    },
    ConfirmRequired {
        menu: Menu,
        /// Empty when an already-open durable menu is replayed.
        envelopes: Vec<RawEnvelope>,
    },
    /// Autonomous recurrence after the one durable continue-work deferral.
    /// No abandon menu is opened and no graph authority is mutated.
    WorkflowUnfinished {
        graph_id: GraphId,
        state_digest: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphInspectResult {
    pub snapshot: GraphInspectSnapshot,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct GraphInspectCursor {
    graph_id: GraphId,
    through_seq: u64,
    after_seq: u64,
}

/// Internal receipt-backed graph testimony. The identity derives from the
/// provider tool-call coordinates, closing the crash window before the
/// generic tool result is committed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphEvidenceCommand {
    pub command_id: String,
    pub request_digest: String,
    pub request_json: String,
    pub session_id: SessionId,
    pub worker_generation: u64,
    pub run_id: RunId,
    pub call_id: String,
    /// Active-root snapshot taken before this evidence command entered the
    /// session actor queue. It prevents a late call from crossing a switch.
    pub graph_id: GraphId,
    pub node: GraphNodeName,
    pub verdict: EvidenceVerdict,
    pub detail: String,
    pub slot: Option<String>,
    pub subject_digest: Option<String>,
    pub signal: Option<ProcessSignalRef>,
    pub workspace_mutation: Option<WorkspaceMutationRef>,
    pub child_contract: Option<ChildContractRef>,
    pub device_id: DeviceId,
}

/// Daemon-internal command for attaching an admitted computer observation to
/// the graph node that is active when the store serializes this command.
/// Unlike `GraphEvidenceCommand`, no graph, node, authority, verdict, subject,
/// or workspace revision is caller-selectable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComputerEvidenceCommand {
    pub command_id: String,
    pub request_digest: String,
    pub request_json: String,
    pub session_id: SessionId,
    pub worker_generation: u64,
    pub run_id: RunId,
    pub call_id: String,
    pub effect_id: EffectId,
    pub effect_args_digest: String,
    /// Graph coordinates snapshotted before native observation begins. The
    /// store requires this exact attempt to remain current at commit time.
    pub graph_id: GraphId,
    pub node: GraphNodeName,
    pub attempt: u32,
    pub observation: ComputerObservationKind,
    pub image: ImageBlockRef,
    pub detail: String,
    pub device_id: DeviceId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessSignalCommand {
    pub session_id: SessionId,
    pub worker_generation: u64,
    pub branch_id: Option<BranchId>,
    pub signal: ProcessSignalRecorded,
    /// Production workers request post-effect revision stamping. `false`
    /// preserves exact validation/replay behavior for legacy signal writers.
    pub stamp_workspace_revision: bool,
    pub device_id: DeviceId,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RecordedProcessSignal {
    pub effect_id: EffectId,
    pub signal_seq: u64,
    pub worker_generation: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ProcessSignalOutcome {
    Committed {
        recorded: RecordedProcessSignal,
        signal: ProcessSignalRecorded,
        envelopes: Vec<RawEnvelope>,
    },
    IdempotentReplay {
        recorded: RecordedProcessSignal,
        signal: ProcessSignalRecorded,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RecordedGraphEvidence {
    pub graph_id: GraphId,
    pub node: GraphNodeName,
    pub attempt: u32,
    pub fingerprint: String,
    pub evidence_seq: u64,
    pub through_seq: u64,
    pub worker_generation: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum GraphEvidenceOutcome {
    Committed {
        recorded: RecordedGraphEvidence,
        envelopes: Vec<RawEnvelope>,
    },
    IdempotentReplay {
        recorded: RecordedGraphEvidence,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum ComputerEvidenceOutcome {
    Committed {
        recorded: RecordedGraphEvidence,
        envelopes: Vec<RawEnvelope>,
    },
    IdempotentReplay {
        recorded: RecordedGraphEvidence,
    },
    /// The exact graph attempt observed before backend execution is no longer
    /// active. Computer tool success is unchanged and no cross-epoch evidence
    /// fact is emitted.
    StaleGraph,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphSwitchCommand {
    pub command_id: String,
    pub request_digest: String,
    pub request_json: String,
    pub session_id: SessionId,
    pub worker_generation: u64,
    pub old_graph_id: GraphId,
    pub new_graph_id: GraphId,
    pub template: String,
    /// Daemon-internal authored replacement. Ordinary RPC callers leave this
    /// absent and continue selecting the immutable catalog by name.
    pub template_spec: Option<GraphTemplateSpec>,
    pub device_id: DeviceId,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SwitchedGraph {
    pub session_id: SessionId,
    pub old_graph_id: GraphId,
    pub new_graph_id: GraphId,
    pub template: String,
    pub digest: String,
    pub superseded_seq: u64,
    pub pinned_seq: u64,
    pub opened_seq: u64,
    pub worker_generation: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum GraphSwitchOutcome {
    Committed {
        switched: SwitchedGraph,
        envelopes: Vec<RawEnvelope>,
    },
    IdempotentReplay {
        switched: SwitchedGraph,
    },
}

/// Secret-free, stable coordinates for an atomic `session.create`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCreateCommand {
    pub command_id: String,
    pub request_digest: String,
    pub request_json: String,
    pub session_id: SessionId,
    pub cwd: String,
    pub provider: String,
    pub model: String,
    pub max_tokens: u64,
    pub permission_overrides: Option<SessionPermissionOverridesV1>,
    /// Creation-time effort selection (G3). `None` — the wire `session.create`
    /// path — means the provider default; delegation passes the parent's
    /// CURRENT effort so children inherit tuning through the metadata clone.
    pub effort: Option<String>,
    /// Creation-time fast-mode flag (G3); same inheritance seam as `effort`.
    pub fast: bool,
    /// Creation-time cache warning policy (CM3).
    pub cache_policy: haider_protocol::cache::CachePolicySettingsV1,
    pub system_prompt_version: String,
    pub event_id: EventId,
    pub device_id: DeviceId,
}

/// Durable response coordinates stored in a committed command receipt.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CreatedSession {
    pub session_id: SessionId,
    pub created_seq: u64,
    pub worker_generation: u64,
    pub metadata: SessionMetadataV1,
}

/// Durable parent↔child relation. Callsigns/task are presentation fields;
/// every operational coordinate is opaque and receipt-stable.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DelegationRecord {
    pub agent_id: AgentId,
    pub child_session_id: SessionId,
    pub child_run_id: RunId,
    pub parent_session_id: SessionId,
    pub parent_run_id: RunId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_branch_id: Option<BranchId>,
    pub call_id: String,
    pub tool_item_id: ItemId,
    pub parent_agent_id: Option<AgentId>,
    pub root_session_id: SessionId,
    pub depth: u32,
    pub task: String,
    pub prompt: String,
    pub manifest: AgentManifest,
    pub state: DelegationState,
    pub report: Option<ChildReport>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationState {
    Spawned,
    Running,
    Reported,
    Collected,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DelegationCreateOutcome {
    Committed(DelegationRecord),
    IdempotentReplay(DelegationRecord),
}

/// One durable descendant together with its distance from the session used
/// as the traversal root. `record.depth` remains the agent's absolute tree
/// depth; `relative_depth` exists solely to enforce a bounded subtree read.
#[derive(Debug, Clone, PartialEq)]
pub struct DelegationDescendant {
    pub record: DelegationRecord,
    pub relative_depth: u32,
    /// Exact durable direct-child count for `record.child_session_id`.
    /// Bounded consumers use this to distinguish a leaf from a node whose
    /// children fell outside their traversal bounds.
    pub direct_child_count: u32,
}

/// Bounded breadth-first descendant reduction.
#[derive(Debug, Clone, PartialEq)]
pub struct DelegationDescendants {
    pub descendants: Vec<DelegationDescendant>,
    /// True only when at least one durable edge was observed outside either
    /// requested bound. Consumers must treat rollups over `descendants` as
    /// partial when this is set.
    pub truncated: bool,
}

/// Result of the atomic session-creation transaction.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionCreateOutcome {
    /// This call committed the session, its `Created` event, and the receipt.
    /// The caller may publish the returned envelope after this result.
    Committed {
        created: CreatedSession,
        envelope: Box<RawEnvelope>,
    },
    /// The same semantic command already committed. Nothing may be published
    /// or executed again.
    IdempotentReplay { created: CreatedSession },
}

/// Secret-free coordinates for one atomic named-branch creation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchCreateCommand {
    pub command_id: String,
    pub request_digest: String,
    pub request_json: String,
    pub session_id: SessionId,
    pub worker_generation: u64,
    pub branch_id: BranchId,
    pub source_branch_id: Option<BranchId>,
    pub fork_node_id: NodeId,
    pub fork_seq: u64,
    pub name: Option<String>,
    pub event_id: EventId,
    pub device_id: DeviceId,
}

/// Stable response stored in the committed `branch.create` receipt.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CreatedBranch {
    pub session_id: SessionId,
    pub branch_id: BranchId,
    pub source_branch_id: Option<BranchId>,
    pub fork_node_id: NodeId,
    pub fork_seq: u64,
    pub created_seq: u64,
    pub worker_generation: u64,
    pub name: String,
}

/// Result of the atomic branch registry/event/receipt transaction.
#[derive(Debug, Clone, PartialEq)]
pub enum BranchCreateOutcome {
    Committed {
        created: CreatedBranch,
        envelope: Box<RawEnvelope>,
    },
    IdempotentReplay {
        created: CreatedBranch,
    },
}

/// Accepted metafork coordinates. The digest covers the complete reviewed
/// operation and is the exact content address journaled in the child.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionMetaforkCommit {
    pub description: String,
    pub model_proposal: SessionMetaforkProposal,
    pub accepted_proposal_digest: String,
}

/// Secret-free coordinates for one atomic session-level fork.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionForkCommand {
    pub command_id: String,
    pub request_digest: String,
    pub request_json: String,
    pub source_session_id: SessionId,
    pub session_id: SessionId,
    pub worker_generation: u64,
    pub source_branch_id: Option<BranchId>,
    pub fork_node_id: NodeId,
    pub fork_seq: u64,
    pub name: Option<String>,
    pub metafork: Option<SessionMetaforkCommit>,
    pub audit_event_id: EventId,
    pub device_id: DeviceId,
}

/// Provider-rendered child view proposed for exact-prefix cache inheritance.
///
/// The store deliberately accepts an opaque JSON value here instead of
/// duplicating the provider-view ledger schema. It compares this value with
/// the authoritative ledger event in the copied source lineage, permits only
/// append-only history after that exact prefix, and extracts the bounded,
/// non-secret coordinates needed by the fork audit.
#[derive(Debug, Clone, PartialEq)]
pub struct ForkCacheInheritanceCandidate {
    pub provider_view: serde_json::Value,
}

/// Stable response stored in the committed fork/metafork command receipt.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CreatedSessionFork {
    pub session_id: SessionId,
    pub source_session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_branch_id: Option<BranchId>,
    pub fork_node_id: NodeId,
    pub fork_seq: u64,
    pub created_seq: u64,
    pub worker_generation: u64,
    pub metadata: SessionMetadataV1,
    pub mode: SessionForkMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_proposal: Option<SessionMetaforkProposal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposal_digest: Option<String>,
    pub omission_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inherited_cache_segment: Option<ForkCacheSegmentV1>,
}

/// Result of the atomic child metadata/history/audit/receipt transaction.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionForkOutcome {
    Committed {
        created: CreatedSessionFork,
        envelopes: Vec<RawEnvelope>,
    },
    IdempotentReplay {
        created: CreatedSessionFork,
    },
}

/// Secret-free coordinates for one atomic live-session model selection.
///
/// `provider` and `model` are the RESOLVED pair — the daemon resolves and
/// validates the selection before this command exists; the store applies it
/// verbatim. Sessions are provider-agnostic: this mutates the session's
/// current model selection (and its provider attribute), never its identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSelectModelCommand {
    pub command_id: String,
    pub request_digest: String,
    pub request_json: String,
    pub session_id: SessionId,
    pub worker_generation: u64,
    pub provider: String,
    pub model: String,
    /// rev933b finding 7: an AUTOMATIC mid-turn switch is only valid
    /// against the pair it observed (compare-and-swap). `Some` refuses the
    /// commit with RevisionConflict when the durable pair moved underneath
    /// it (a concurrent explicit selection wins). Explicit selections pass
    /// `None` — the user's latest word is unconditional.
    pub expected_pair: Option<(String, String)>,
    pub event_id: EventId,
    pub device_id: DeviceId,
}

/// Stable response stored in the committed `session.select_model` receipt.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SelectedModel {
    pub session_id: SessionId,
    pub provider: String,
    pub model: String,
    pub selected_seq: u64,
    pub worker_generation: u64,
}

/// Result of the atomic metadata-update/event/receipt transaction.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionSelectModelOutcome {
    Committed {
        selected: SelectedModel,
        envelope: Box<RawEnvelope>,
    },
    IdempotentReplay {
        selected: SelectedModel,
    },
}

/// Secret-free coordinates for one atomic live-session rename (G2).
///
/// `title` is the NORMALIZED value — the daemon trims, strips control
/// characters, caps at 80 chars, and collapses empty to `None` before this
/// command exists; the store applies it verbatim. `only_if_untitled` is the
/// auto-title guard: when set, an existing title turns the whole command
/// into a durable no-op ([`SessionRenameOutcome::Skipped`]) — auto-title
/// must never overwrite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRenameCommand {
    pub command_id: String,
    pub request_digest: String,
    pub request_json: String,
    pub session_id: SessionId,
    pub worker_generation: u64,
    pub title: Option<String>,
    pub only_if_untitled: bool,
    pub event_id: EventId,
    pub device_id: DeviceId,
}

/// Stable response stored in the committed `session.rename` receipt.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RenamedSession {
    pub session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub renamed_seq: u64,
    pub worker_generation: u64,
}

/// Result of the atomic title-update/event/receipt transaction.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionRenameOutcome {
    Committed {
        renamed: RenamedSession,
        envelope: Box<RawEnvelope>,
    },
    IdempotentReplay {
        renamed: RenamedSession,
    },
    /// `only_if_untitled` found an existing title: nothing was claimed,
    /// journaled, or updated. Only the internal auto-title path can see
    /// this — an explicit rename never sets the guard.
    Skipped,
}

/// Secret-free coordinates for one atomic durable attention acknowledgement.
///
/// `session.seen` is ordered through the same actor as every session write.
/// The store stamps `seen_at_ms` and preserves the greater already-durable
/// value, so a wall-clock regression can never make a session unseen again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSeenCommand {
    pub command_id: String,
    pub request_digest: String,
    pub request_json: String,
    pub session_id: SessionId,
    pub worker_generation: u64,
    pub event_id: EventId,
    pub device_id: DeviceId,
}

/// Stable response stored in the committed `session.seen` receipt.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SeenSession {
    pub session_id: SessionId,
    pub seen_at_ms: u64,
    pub seen_seq: u64,
    pub worker_generation: u64,
}

/// Result of the atomic attention acknowledgement transaction.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionSeenOutcome {
    Committed {
        seen: SeenSession,
        envelope: Box<RawEnvelope>,
    },
    IdempotentReplay {
        seen: SeenSession,
    },
}

/// Secret-free coordinates for one atomic live-session effort selection (G3).
///
/// `effort` is the RESOLVED, ladder-validated value — the daemon validates
/// against the current pair's declared ladder before this command exists; the
/// store applies it verbatim. `None` reverts to the provider default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSelectEffortCommand {
    pub command_id: String,
    pub request_digest: String,
    pub request_json: String,
    pub session_id: SessionId,
    pub worker_generation: u64,
    pub effort: Option<String>,
    pub event_id: EventId,
    pub device_id: DeviceId,
}

/// Stable response stored in the committed `session.select_effort` receipt.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SelectedEffort {
    pub session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    pub selected_seq: u64,
    pub worker_generation: u64,
}

/// Result of the atomic effort metadata-update/event/receipt transaction.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionSelectEffortOutcome {
    Committed {
        selected: SelectedEffort,
        envelope: Box<RawEnvelope>,
    },
    IdempotentReplay {
        selected: SelectedEffort,
    },
}

/// Secret-free coordinates for one atomic live-session agent-type binding
/// (W-flow inline identity).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSelectAgentTypeCommand {
    pub command_id: String,
    pub request_digest: String,
    pub request_json: String,
    pub session_id: SessionId,
    pub worker_generation: u64,
    pub agent_type: Option<String>,
    pub event_id: EventId,
    pub device_id: DeviceId,
}

/// Stable response stored in the committed `session.select_agent_type`
/// receipt.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SelectedAgentType {
    pub session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    pub selected_seq: u64,
    pub worker_generation: u64,
}

/// Result of the atomic agent-type metadata-update/event/receipt transaction.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionSelectAgentTypeOutcome {
    Committed {
        selected: SelectedAgentType,
        envelope: Box<RawEnvelope>,
    },
    IdempotentReplay {
        selected: SelectedAgentType,
    },
}

/// Secret-free coordinates for one atomic live-session fast-mode toggle (G3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSelectFastCommand {
    pub command_id: String,
    pub request_digest: String,
    pub request_json: String,
    pub session_id: SessionId,
    pub worker_generation: u64,
    pub enabled: bool,
    pub event_id: EventId,
    pub device_id: DeviceId,
}

/// Stable response stored in the committed `session.select_fast` receipt.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SelectedFast {
    pub session_id: SessionId,
    pub enabled: bool,
    pub selected_seq: u64,
    pub worker_generation: u64,
}

/// Result of the atomic fast-mode metadata-update/event/receipt transaction.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionSelectFastOutcome {
    Committed {
        selected: SelectedFast,
        envelope: Box<RawEnvelope>,
    },
    IdempotentReplay {
        selected: SelectedFast,
    },
}

/// Receipt/session coordinates shared by the G3 session-config transactions.
struct SessionConfigSelection<'a> {
    command_id: &'a str,
    request_digest: &'a str,
    request_json: &'a str,
    session_id: &'a SessionId,
    worker_generation: u64,
    /// Durable receipt method name, e.g. `session.select_effort`.
    method: &'static str,
    /// Human description for receipt diagnostics.
    description: &'static str,
    event_id: EventId,
    device_id: DeviceId,
}

/// Typed result of the shared session-config transaction.
enum SessionConfigOutcome<R> {
    Committed {
        selected: R,
        envelope: Box<RawEnvelope>,
    },
    IdempotentReplay {
        selected: R,
    },
}

/// Secret-free coordinates for atomically accepting a live turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnAcceptCommand {
    pub command_id: String,
    pub request_digest: String,
    pub request_json: String,
    pub session_id: SessionId,
    pub worker_generation: u64,
    pub run_id: RunId,
    pub agent_id: Option<AgentId>,
    pub branch_id: Option<BranchId>,
    pub text: String,
    pub attachments: Vec<AttachmentBlock>,
    pub mode: DeliveryMode,
    pub queued_event_id: EventId,
    pub user_event_id: EventId,
    pub active_event_id: EventId,
    pub device_id: DeviceId,
}

/// Durable execution disposition selected at the serialized acceptance point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnAdmissionDisposition {
    Started,
    Queued,
    SteerPending,
    SubturnPending,
}

/// Durable response coordinates stored in a committed `turn.submit` receipt.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AcceptedTurn {
    pub session_id: SessionId,
    pub run_id: RunId,
    pub accepted_seq: u64,
    pub worker_generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_id: Option<BranchId>,
    pub disposition: TurnAdmissionDisposition,
    /// Additive G2 fact: this acceptance committed the session's FIRST
    /// main-timeline user node (its tree parent was empty). The daemon's
    /// auto-title fires only on such accepts. `false` stays off the wire so
    /// pre-G2 receipt bytes are unchanged, and legacy receipts replay as
    /// `false` — a replay from before the feature never titles.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub first_user_turn: bool,
    /// Daemon-verified PDF facts persisted in the command receipt. These are
    /// derived from the exact canonical blocks journaled with the user turn.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pdf_attachments: Vec<haider_protocol::tool::PdfAttachmentReceipt>,
}

/// Result of the atomic turn-acceptance transaction.
#[derive(Debug, Clone, PartialEq)]
pub enum TurnAcceptOutcome {
    Committed {
        accepted: AcceptedTurn,
        envelopes: Vec<RawEnvelope>,
    },
    IdempotentReplay {
        accepted: AcceptedTurn,
    },
}

/// Secret-free semantic input for one manual retry acceptance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRetryCommand {
    pub command_id: String,
    pub request_digest: String,
    pub request_json: String,
    pub session_id: SessionId,
    pub worker_generation: u64,
    pub run_id: RunId,
    pub queued_event_id: EventId,
    pub retried_event_id: EventId,
    pub active_event_id: EventId,
    pub device_id: DeviceId,
}

/// Durable coordinates stored in a committed `run.retry` receipt.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AcceptedRunRetry {
    pub session_id: SessionId,
    pub run_id: RunId,
    pub failed_run_id: RunId,
    pub prompt_run_id: RunId,
    pub user_seq: u64,
    pub accepted_seq: u64,
    pub worker_generation: u64,
    /// Present when the receipt accepts a wake of this exact durable
    /// `Retrying` fact instead of minting a fresh terminal-failure run.
    /// Event identity (rather than attempt number) prevents a delayed receipt
    /// replay from waking a later provider request's new backoff ladder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backoff_event_id: Option<EventId>,
}

/// Result of the atomic manual-retry acceptance transaction.
#[derive(Debug, Clone, PartialEq)]
pub enum RunRetryOutcome {
    Committed {
        accepted: AcceptedRunRetry,
        envelopes: Vec<RawEnvelope>,
    },
    IdempotentReplay {
        accepted: AcceptedRunRetry,
    },
}

/// Secret-free coordinates for atomically recording cancellation intent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnCancelCommand {
    pub command_id: String,
    pub request_digest: String,
    pub request_json: String,
    pub session_id: SessionId,
    pub worker_generation: u64,
    pub run_id: RunId,
    pub cancelling_event_id: EventId,
    pub device_id: DeviceId,
}

/// Durable cancellation status stored in a committed receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnCancellationStatus {
    Accepted,
    AlreadyTerminal,
}

/// Durable response coordinates for `turn.cancel`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CancelledTurn {
    pub session_id: SessionId,
    pub run_id: RunId,
    pub status: TurnCancellationStatus,
    pub terminal_seq: Option<u64>,
}

/// Method tag of the durable `account.login_api` command (R10).
const LOGIN_METHOD: &str = "account.login_api";
/// Method tag of the durable OAuth `account.add` command.
const ACCOUNT_ADD_METHOD: &str = "account.add";
pub const ACCOUNT_SET_ACTIVE_METHOD: &str = "account.set_active";
pub const ACCOUNT_REMOVE_METHOD: &str = "account.remove";
pub const ACCOUNT_SET_DEFAULT_MODEL_METHOD: &str = "account.set_default_model";
pub const PROVIDER_CONFIGURE_METHOD: &str = "provider.configure";
pub const PROVIDER_REMOVE_METHOD: &str = "provider.remove";
const PROVIDER_CONFIGURE_NOOP_RESPONSE_FIELD: &str = "revision_unchanged_response";
pub const HOOKS_TRUST_METHOD: &str = "hooks.trust";
pub const HOOKS_REVOKE_METHOD: &str = "hooks.revoke";

fn is_management_method(method: &str) -> bool {
    matches!(
        method,
        LOGIN_METHOD
            | ACCOUNT_ADD_METHOD
            | ACCOUNT_SET_ACTIVE_METHOD
            | ACCOUNT_REMOVE_METHOD
            | ACCOUNT_SET_DEFAULT_MODEL_METHOD
            | PROVIDER_CONFIGURE_METHOD
            | PROVIDER_REMOVE_METHOD
    )
}

/// Secret-free semantic input for one receipted hook trust mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookTrustCommand {
    pub command_id: String,
    pub request_digest: String,
    pub request_json: String,
    pub digest: String,
    pub trusted: bool,
    /// Present only for the automatic first-digest pin created by the
    /// `trust_workspace` profile policy.
    pub workspace: Option<String>,
}

/// Stable response persisted in a hook trust/revoke command receipt.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HookTrustChange {
    pub digest: String,
    pub trusted: bool,
    /// Monotonic position in the hook-trust receipt domain. Older stored
    /// receipts decode as zero and are repaired at the service boundary.
    #[serde(default)]
    pub revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
}

/// Committed login response persisted in the receipt: the descriptor only —
/// receipt metadata NEVER contains the secret or the ephemeral vault
/// reference.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LoginReceiptResponse {
    pub descriptor: CredentialDescriptor,
}

/// Committed OAuth account-add response. Like login receipts, it contains
/// only the public descriptor; the ready ref and token bundle never enter
/// SQLite.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AccountAddReceiptResponse {
    pub descriptor: CredentialDescriptor,
}

/// Outcome of [`Store::account_add_claim_receipt`].
#[derive(Debug, Clone, PartialEq)]
pub enum AccountAddClaim {
    Fresh,
    ResumePending,
    Committed(Box<AccountAddReceiptResponse>),
}

/// One pending/committed OAuth account-add receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountAddReceiptRow {
    pub command_id: String,
    pub state: String,
    pub request_json: String,
    pub response_json: Option<String>,
    pub final_revision: Option<u64>,
}

/// Durable response for the idle-only `session.compact` command.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ContextCompactionReceiptResponse {
    pub session_id: SessionId,
    pub run_id: RunId,
    pub accepted_seq: u64,
    pub worker_generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_id: Option<BranchId>,
}

/// Claim result for one manual context-compaction command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextCompactionClaim {
    Fresh,
    ResumePending,
    Committed(Box<ContextCompactionReceiptResponse>),
}

/// Global command-id claim for the typed client monitor mutations. The
/// response stays as JSON here so the storage layer does not depend on the
/// RPC crate; the daemon decodes it into the method-specific typed receipt.
#[derive(Debug, Clone, PartialEq)]
pub enum MonitorControlClaim {
    Fresh,
    ResumePending,
    Committed(serde_json::Value),
}

/// Secret-free coordinates for atomically accepting a direct user shell
/// command. The command bytes themselves live in the ordinary started item
/// and in the canonical receipt request JSON, never in this response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellExecAcceptCommand {
    pub command_id: String,
    pub request_digest: String,
    pub request_json: String,
    pub session_id: SessionId,
    pub worker_generation: u64,
    pub branch_id: Option<BranchId>,
    pub agent_id: Option<AgentId>,
    pub run_id: RunId,
    pub item_id: ItemId,
    pub command: String,
    pub running_event_id: EventId,
    pub item_event_id: EventId,
    pub active_event_id: EventId,
    pub device_id: DeviceId,
}

/// Durable acceptance response for `shell.exec`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AcceptedShellExec {
    pub session_id: SessionId,
    pub run_id: RunId,
    pub item_id: ItemId,
    pub accepted_seq: u64,
    pub worker_generation: u64,
}

/// Result of the atomic direct-shell acceptance transaction.
#[derive(Debug, Clone, PartialEq)]
pub enum ShellExecAcceptOutcome {
    Committed {
        accepted: AcceptedShellExec,
        envelopes: Vec<RawEnvelope>,
    },
    IdempotentReplay {
        accepted: AcceptedShellExec,
    },
}

/// Receipt claim for one W5 durable account/provider mutation.
#[derive(Debug, Clone, PartialEq)]
pub enum ManagementClaim<T> {
    Fresh,
    ResumePending { recovery_json: Option<String> },
    Committed { response: Box<T>, revision: u64 },
}

/// Pending/committed durable account/provider mutation receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagementReceiptRow {
    pub command_id: String,
    pub method: String,
    pub state: String,
    pub request_json: String,
    pub recovery_json: Option<String>,
    pub response_json: Option<String>,
    pub final_revision: Option<u64>,
}

/// Durable remove reservation joined to its command receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountRemoveReceiptRow {
    pub receipt: ManagementReceiptRow,
    pub alias: String,
    pub provider: String,
    pub was_active: bool,
}

/// One provider's durable last-known model catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedModels {
    pub models_json: String,
    pub etag: Option<String>,
    pub fetched_at_ms: u64,
}

/// Atomic result of registering one typed Loom specialist. `install_job` is
/// present only when this transaction created the job and therefore owns
/// daemon-runner adoption. `install_job_id` also projects an already-existing
/// exact-revision job for an idempotent registration; types without required
/// CLIs carry neither.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoomAgentTypeRegistration {
    pub registration: LoomRegistration,
    pub install_job: Option<TypedAgentInstallJob>,
    pub install_job_id: Option<String>,
}

/// Store result for a CAS-fenced registry mutation. The publication cursor is
/// allocated in the same transaction as `value`; callers publish only after
/// this result returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoomRegistryMutation<T> {
    Applied {
        value: T,
        publication_cursor: Option<u64>,
    },
    Conflict(LoomRevisionConflict),
}

/// Archive/unarchive result before wire projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoomArchiveResult {
    Changed {
        entry: LoomRegistryEntryRef,
        publication_cursor: u64,
    },
    Already(LoomRegistryEntryRef),
    NotFound,
    Conflict(LoomRevisionConflict),
}

/// Compare-and-swap coordinates for one optional per-program transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypedAgentInstallItemCas {
    pub expected: TypedAgentInstallItem,
    pub next: TypedAgentInstallItem,
}

/// One atomic install lifecycle update. The expected snapshots are the CAS
/// fence; the store validates all identity, progress, timestamp, and state
/// transitions before replacing the job and optional item together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypedAgentInstallCas {
    pub expected_job: TypedAgentInstallJob,
    pub next_job: TypedAgentInstallJob,
    pub item: Option<TypedAgentInstallItemCas>,
}

/// One transactionally coherent status view for reconnecting callers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypedAgentInstallSnapshot {
    pub jobs: Vec<TypedAgentInstallJob>,
    pub items: Vec<TypedAgentInstallItem>,
}

/// Result of the explicit failed-install reset. Rejections are facts returned
/// as typed data by the RPC layer, not errors whose prose must be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypedAgentInstallRetryResult {
    Requeued(TypedAgentInstallJob),
    JobNotFound,
    StateNotRetryable { state: TypedAgentInstallState },
    ContractNotCurrent,
}

/// Result of the explicit install cancellation door.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypedAgentInstallCancelResult {
    Cancelled,
    AlreadyTerminal {
        state: TypedAgentInstallTerminalStateV1,
    },
    Unknown,
}

/// One bounded durable registry replay page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoomRegistryWatchPage {
    pub replay_through_cursor: u64,
    pub next_cursor: u64,
    pub deltas: Vec<LoomRegistryDelta>,
}

/// One bounded, exact-job page from the durable progress history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypedAgentInstallWatchPage {
    pub requested_after_cursor: u64,
    pub replay_through_cursor: u64,
    pub next_cursor: u64,
    pub events: Vec<TypedAgentInstallEvent>,
}

/// Lookup result for the replayable install progress door.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypedAgentInstallWatchResult {
    Watching(TypedAgentInstallWatchPage),
    JobNotFound,
    CursorAhead { requested: u64, head: u64 },
}

/// Definitive login failure persisted in a failed receipt (401/403 class):
/// stable code + human message, never provider body or key text.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LoginReceiptFailure {
    pub code: String,
    pub message: String,
}

/// Outcome of [`Store::login_claim_receipt`].
#[derive(Debug, Clone, PartialEq)]
pub enum LoginClaim {
    /// No prior receipt: this attempt owns the command.
    Fresh,
    /// A pending receipt already existed (crashed or retryable earlier
    /// attempt); the caller reconciles vault/descriptor state first.
    ResumePending,
    /// The command already committed; replay this exact response.
    Committed(Box<LoginReceiptResponse>),
}

/// One pending/committed login receipt row for startup reconciliation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginReceiptRow {
    pub command_id: String,
    /// `"pending"` or `"committed"`.
    pub state: String,
    pub request_json: String,
    pub response_json: Option<String>,
    pub final_revision: Option<u64>,
}

/// Result of the atomic cancellation-intent transaction.
#[derive(Debug, Clone, PartialEq)]
pub enum TurnCancelOutcome {
    Committed {
        cancelled: CancelledTurn,
        envelope: Option<Box<RawEnvelope>>,
    },
    IdempotentReplay {
        cancelled: CancelledTurn,
    },
}

/// One coherent held-message snapshot and its mutation fence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueSnapshot {
    pub revision: u64,
    pub rows: Vec<QueueRow>,
}

/// Stable coordinates for a revision-fenced removal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueRemoveCommand {
    pub session_id: SessionId,
    pub id: EventId,
    pub revision: u64,
    pub cancelling_event_id: EventId,
    pub delta_event_id: EventId,
    pub device_id: DeviceId,
}

/// Stable coordinates for a revision-fenced promotion into the active run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuePromoteCommand {
    pub session_id: SessionId,
    pub id: EventId,
    pub revision: u64,
    /// Filled by the daemon after its read-only preview and live-harness
    /// reservation. The commit rechecks it inside the write transaction.
    pub expected_active_run_id: Option<RunId>,
    pub cancelling_event_id: EventId,
    pub delivery_event_id: EventId,
    pub delta_event_id: EventId,
    pub device_id: DeviceId,
}

/// Read-only half of a promotion used to reserve the exact live harness
/// before the revision-fenced mutation commits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuePromotePreview {
    pub active_run_id: RunId,
    pub text: String,
}

/// Worker-owned transition from held to delivery-attempted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueConsumeCommand {
    pub session_id: SessionId,
    pub run_id: RunId,
    pub delta_event_id: EventId,
    pub device_id: DeviceId,
}

/// Committed removal plus the revision-bearing event to publish.
#[derive(Debug, Clone, PartialEq)]
pub struct QueueRemoveOutcome {
    pub revision: u64,
    pub envelopes: Vec<RawEnvelope>,
}

/// Committed promotion coordinates. `delivery_seq` is the one live-worker
/// deduplication key for the newly journaled steer delivery.
#[derive(Debug, Clone, PartialEq)]
pub struct QueuePromoteOutcome {
    pub revision: u64,
    pub active_run_id: RunId,
    pub delivery_seq: u64,
    pub text: String,
    pub envelopes: Vec<RawEnvelope>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct QueueConsumeOutcome {
    pub revision: u64,
    pub id: EventId,
    pub envelope: Box<RawEnvelope>,
}

impl CommittedSeqRange {
    pub fn len(&self) -> u64 {
        if self.is_empty() {
            0
        } else {
            self.last_seq - self.first_seq + 1
        }
    }

    pub fn is_empty(&self) -> bool {
        self.first_seq > self.last_seq
    }
}

/// Synchronous durability port for the committed event stream.
pub trait EventStore: Send + Sync {
    /// Atomically appends one same-session batch.
    ///
    /// Sequence and commit-time fields are assigned at commit. The caller's
    /// envelopes are updated only after the transaction succeeds, making them
    /// safe to publish once this method returns. Empty and mixed-session
    /// batches are rejected with `InvalidArgument`.
    fn append(&self, envelopes: &mut [RawEnvelope]) -> StoreResult<CommittedSeqRange>;

    /// Reads committed envelopes with `seq > since_seq`, ordered by sequence.
    fn read(
        &self,
        session: &SessionId,
        since_seq: u64,
        limit: usize,
    ) -> StoreResult<Vec<RawEnvelope>>;

    /// Returns the latest committed sequence, or zero for an empty session.
    fn latest_seq(&self, session: &SessionId) -> StoreResult<u64>;
}

/// One logical actor append admitted to a shared journal transaction.
///
/// The group-commit coordinator may mix ordinary and live-worker appends from
/// different sessions. Each batch keeps its own validation, timestamp, and
/// result boundary; only the final SQLite commit is shared. The caller must
/// supply batches in arrival order and only after they are already available:
/// this type is not a license to delay a lone append for speculative batching.
pub struct JournalAppendBatch {
    pub envelopes: Vec<RawEnvelope>,
    pub validate_worker_transitions: bool,
}

/// Opaque ownership of the profile's OS-held lifetime lock.
///
/// W3b1 seam (additive): daemon startup acquires this before opening SQLite
/// or examining an endpoint — the lock is the singleton authority (d1 report
/// R1), so it must be held before any stale-socket cleanup. The lease is then
/// transferred into [`Store::open_locked`]; [`Store::open`] remains the
/// one-step path for everyone else. Dropping an unconsumed lease releases the
/// lock. The lease deliberately exposes no store access.
pub struct ProfileLease {
    root: PathBuf,
    lock: ProfileLock,
}

/// A locked profile containing a SQLite event journal and filesystem CAS.
///
/// One connection lives for the full profile lifetime. A mutex serializes its
/// synchronous journal calls, while SQLite's statement cache avoids preparing
/// the hot append/read queries again on every event.
pub struct Store {
    root: PathBuf,
    database_path: PathBuf,
    worker_generation: u64,
    connection: Mutex<Connection>,
    graph_reductions: Mutex<HashMap<SessionId, CachedGraphReduction>>,
    graph_telemetry: Mutex<GraphTelemetryCache>,
    cas: FileCas,
    provider_views: ProviderViewStore,
    _lock: ProfileLock,
}

struct CachedGraphReduction {
    // Volatile graph-forest projection. The connection lock serializes
    // cache extension after commit with every reader; journal facts remain
    // the authority and a restart simply rebuilds this value.
    envelopes: Vec<RawEnvelope>,
    reductions: GraphReductions,
}

#[derive(Default)]
struct GraphTelemetryCache {
    by_session: HashMap<SessionId, CachedSessionGraphTelemetry>,
}

struct CachedSessionGraphTelemetry {
    through_seq: u64,
    accumulator: GraphTelemetryAccumulator,
    projection: GraphTelemetryProjection,
}

const GRAPH_TELEMETRY_REDUCER_VERSION: u32 = 3;

fn trim_to_latest<T>(rows: &mut Vec<T>, limit: usize) {
    if rows.len() > limit {
        rows.drain(..rows.len() - limit);
    }
}

impl Store {
    /// Acquires the profile lifetime lock without opening its durable store.
    pub fn acquire_profile(root: impl AsRef<Path>) -> StoreResult<ProfileLease> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(&root).map_err(|error| {
            store_error(
                ErrorCode::Internal,
                format!("cannot create store root {}: {error}", root.display()),
                false,
            )
        })?;
        let lock = ProfileLock::acquire(&root)?;
        Ok(ProfileLease { root, lock })
    }

    /// Opens or creates a durable profile after its lifetime lock is held.
    pub fn open_locked(lease: ProfileLease) -> StoreResult<Self> {
        let ProfileLease {
            root,
            lock: profile_lock,
        } = lease;
        let database_path = root.join("store.sqlite");
        let mut connection = open_connection(&database_path)?;
        migrations::migrate(&mut connection)?;
        backfill_payload_kinds(&mut connection)?;
        backfill_run_head_projections(&mut connection)?;
        connection.set_prepared_statement_cache_capacity(16);
        let cas = FileCas::open(&root)?;
        let provider_views = ProviderViewStore::open(&root)?;
        provider_views.sweep_expired(&mut connection, now_ms()?)?;
        let worker_generation = next_worker_generation(&mut connection)?;
        backfill_workflow_graph_journals(&mut connection, &cas, worker_generation)?;
        let graph_telemetry = rebuild_graph_telemetry_cache(&connection)?;

        Ok(Self {
            root,
            database_path,
            worker_generation,
            connection: Mutex::new(connection),
            graph_reductions: Mutex::new(HashMap::new()),
            graph_telemetry: Mutex::new(graph_telemetry),
            cas,
            provider_views,
            _lock: profile_lock,
        })
    }

    /// Acquires the profile lifetime lock and opens its durable store.
    pub fn open(root: impl AsRef<Path>) -> StoreResult<Self> {
        let lease = Self::acquire_profile(root)?;
        Self::open_locked(lease)
    }

    /// The profile root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The SQLite database path.
    pub fn database_path(&self) -> &Path {
        &self.database_path
    }

    /// Durable fencing generation allocated by this successful profile open.
    pub fn worker_generation(&self) -> u64 {
        self.worker_generation
    }

    /// Persists one provider-rendered request view into the dedicated CAS and
    /// returns the hashes-only ledger with its durable request cursor.
    pub fn persist_provider_view(
        &self,
        session_id: &SessionId,
        ledger: ProviderViewLedgerV1,
        blobs: Vec<ProviderViewBlobV1>,
    ) -> StoreResult<ProviderViewLedgerV1> {
        let mut connection = self.connection()?;
        let _ = self
            .provider_views
            .sweep_expired_if_due(&mut connection, now_ms()?)?;
        self.provider_views.persist(
            &mut connection,
            session_id,
            ledger,
            blobs,
            default_expiry_ms()?,
        )
    }

    /// Testable storage seam with an explicit expiry; production callers use
    /// [`Self::persist_provider_view`].
    pub fn persist_provider_view_until(
        &self,
        session_id: &SessionId,
        ledger: ProviderViewLedgerV1,
        blobs: Vec<ProviderViewBlobV1>,
        expires_at_ms: u64,
    ) -> StoreResult<ProviderViewLedgerV1> {
        let mut connection = self.connection()?;
        self.provider_views
            .persist(&mut connection, session_id, ledger, blobs, expires_at_ms)
    }

    /// Verifies every referenced block directly from disk without retaining
    /// a complete request view.
    pub fn verify_provider_view(&self, ledger: &ProviderViewLedgerV1) -> StoreResult<()> {
        let connection = self.connection()?;
        self.provider_views.verify(&connection, ledger)
    }

    /// Re-reads one exact block for request replay/keepalive work. Callers
    /// release the returned block before advancing to the next cursor.
    pub fn read_provider_view_block(
        &self,
        ledger: &ProviderViewLedgerV1,
        block: &ProviderViewBlockRefV1,
    ) -> StoreResult<Vec<u8>> {
        let connection = self.connection()?;
        self.provider_views.read_block(&connection, ledger, block)
    }

    /// Deletes expired request indexes and unreferenced provider-view blobs.
    pub fn sweep_expired_provider_views(&self, through_ms: u64) -> StoreResult<usize> {
        let mut connection = self.connection()?;
        self.provider_views
            .sweep_expired(&mut connection, through_ms)
    }

    /// Resident bytes used by the byte-capped provider-view hot-block LRU.
    pub fn provider_view_resident_bytes(&self) -> StoreResult<usize> {
        self.provider_views.resident_bytes()
    }

    /// Durably advances the daemon-process generation for one guarded start.
    ///
    /// W3b1 seam (additive): intentionally distinct from `worker_generation`,
    /// which is consumed by *every* store open (including read-only tooling).
    /// The daemon generation counts daemon starts only and is what the daemon
    /// advertises in `Welcome`/`ServerDraining` for client-side fencing.
    pub fn advance_daemon_generation(&self) -> StoreResult<u64> {
        let mut connection = self.connection()?;
        next_profile_counter(&mut connection, "daemon_generation", "daemon generation")
    }

    /// Commits a harmless write transaction to prove the profile is writable.
    /// Updating the singleton to its existing value avoids consuming a
    /// semantic revision while still exercising SQLite, WAL, and fsync.
    pub fn probe_writable(&self) -> StoreResult<()> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction().map_err(map_sqlite_error)?;
        transaction
            .execute(
                "UPDATE profile_meta SET management_revision = management_revision WHERE singleton = 1",
                [],
            )
            .map_err(map_sqlite_error)?;
        transaction.commit().map_err(map_sqlite_error)
    }

    /// Reads the revision of the coherently published account/provider
    /// management snapshot.
    pub fn management_revision(&self) -> StoreResult<u64> {
        let connection = self.connection()?;
        let revision: i64 = connection
            .query_row(
                "SELECT management_revision FROM profile_meta WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .map_err(map_sqlite_error)?;
        u64::try_from(revision)
            .map_err(|_| corrupt("database contains a negative management revision"))
    }

    /// Returns the durable profile installation identity used by usage-day
    /// provenance. It is minted lazily from the OS RNG and never shares the
    /// journal's deliberately per-process [`DeviceId`] semantics.
    pub fn profile_installation_id(&self) -> StoreResult<String> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        let stored: Option<String> = transaction
            .query_row(
                "SELECT installation_id FROM profile_meta WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .map_err(map_sqlite_error)?;
        if let Some(stored) = stored {
            validate_profile_installation_id(&stored)?;
            transaction.commit().map_err(map_sqlite_error)?;
            return Ok(stored);
        }

        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).map_err(|error| {
            store_error(
                ErrorCode::Internal,
                format!("OS randomness unavailable for profile installation id: {error}"),
                false,
            )
        })?;
        let installation_id = format!("dev-{}", hex::encode(random));
        let updated = transaction
            .execute(
                "UPDATE profile_meta SET installation_id = ?1
                 WHERE singleton = 1 AND installation_id IS NULL",
                [&installation_id],
            )
            .map_err(map_sqlite_error)?;
        if updated != 1 {
            return Err(corrupt(
                "profile installation id lost its singleton initialization claim",
            ));
        }
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(installation_id)
    }

    /// Runs the v1 journal backfill once, then reconciles any closed slots a
    /// prior process left only in the journal. Files are complete before the
    /// SQLite version marker advances, so a crash retries the reducer.
    pub fn initialize_usage_history(&self) -> StoreResult<()> {
        let installation_id = self.profile_installation_id()?;
        let backfill_version = self.usage_backfill_version()?;
        let slots = self.fold_usage_journals()?;
        let now = now_ms()?;
        if backfill_version < 1 {
            self.install_usage_backfill(&installation_id, &slots, now)?;
            self.set_usage_backfill_version(1)?;
        }
        let writer = UsageLedgerWriter::new(&self.root, installation_id, env!("CARGO_PKG_VERSION"));
        for (address, slot) in slots {
            let address_start = usage_slot_start_ms(&address.date, address.slot)?;
            if address_start.saturating_add(15 * 60 * 1_000) <= now {
                writer.append_slot(&address, &slot, false)?;
            }
        }
        Ok(())
    }

    /// Re-scans authoritative journals and appends only newly closed slots.
    /// Existing slot records make the operation idempotent across restarts.
    pub fn reconcile_usage_history(&self) -> StoreResult<()> {
        let installation_id = self.profile_installation_id()?;
        let writer = UsageLedgerWriter::new(&self.root, installation_id, env!("CARGO_PKG_VERSION"));
        let now = now_ms()?;
        for (address, slot) in self.fold_usage_journals()? {
            let address_start = usage_slot_start_ms(&address.date, address.slot)?;
            if address_start.saturating_add(15 * 60 * 1_000) <= now {
                writer.append_slot(&address, &slot, false)?;
            }
        }
        Ok(())
    }

    pub fn usage_history_day(
        &self,
        date: &str,
    ) -> StoreResult<Option<haider_protocol::usage::UsageHistoryDayV1>> {
        let expected_device_id = self.profile_installation_id()?;
        let day = crate::usage_ledger::read_usage_day(&self.root, date)?;
        if let Some(day) = &day
            && day.device_id != expected_device_id
        {
            return Err(corrupt(format!(
                "usage-history day {date} belongs to device {}, expected {expected_device_id}",
                day.device_id
            )));
        }
        Ok(day)
    }

    pub fn usage_history_range(
        &self,
        through_date: &str,
        days: u16,
    ) -> StoreResult<Vec<haider_protocol::usage::UsageHistoryRangeDayV1>> {
        let expected_device_id = self.profile_installation_id()?;
        crate::usage_ledger::read_usage_range_for_device(
            &self.root,
            through_date,
            days,
            &expected_device_id,
        )
    }

    pub fn append_usage_meter_sample(
        &self,
        sample: &haider_protocol::usage::UsageHistoryMeterSampleV1,
    ) -> StoreResult<()> {
        let installation_id = self.profile_installation_id()?;
        UsageLedgerWriter::new(&self.root, installation_id, env!("CARGO_PKG_VERSION"))
            .append_meter_sample(sample)
    }

    fn usage_backfill_version(&self) -> StoreResult<u32> {
        let connection = self.connection()?;
        let version: i64 = connection
            .query_row(
                "SELECT usage_backfill_version FROM profile_meta WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .map_err(map_sqlite_error)?;
        u32::try_from(version)
            .map_err(|_| corrupt("database contains an invalid usage backfill version"))
    }

    fn set_usage_backfill_version(&self, version: u32) -> StoreResult<()> {
        let connection = self.connection()?;
        let updated = connection
            .execute(
                "UPDATE profile_meta SET usage_backfill_version = ?1
                 WHERE singleton = 1 AND usage_backfill_version < ?1",
                [version],
            )
            .map_err(map_sqlite_error)?;
        if updated != 1 {
            return Err(corrupt(
                "usage backfill version did not advance exactly once",
            ));
        }
        Ok(())
    }

    /// Folds each session a page at a time. The only profile-wide allocation
    /// is the compact slot result; decoded envelopes are dropped after their
    /// page is reduced. Each completed session advances a journal-anchored
    /// checkpoint through the observed committed journal head so later
    /// restarts read only that session's missing suffix. Filtering happens in
    /// SQLite before envelope materialization; the returned boundary is read
    /// under the same connection lock so a concurrent group commit can never
    /// be checkpointed before its reducer facts are visible.
    fn fold_usage_journals(
        &self,
    ) -> StoreResult<
        BTreeMap<crate::usage_ledger::UsageSlotAddress, crate::usage_ledger::UsageLedgerSlot>,
    > {
        let mut slots = BTreeMap::new();
        for session_id in self.session_ids()? {
            let (mut reducer, mut cursor) = self.load_usage_checkpoint(&session_id)?;
            let checkpoint_cursor = cursor;
            let mut boundary = None;
            loop {
                let page = self.read_reducer_page_with_boundary(
                    &session_id,
                    cursor,
                    REPLAY_PAGE_SIZE,
                    USAGE_REPLAY_PAGE_BYTES,
                    USAGE_REDUCER_PAYLOAD_KINDS,
                )?;
                let Some(last) = page.envelopes.last() else {
                    boundary = page.observed_head.or(boundary);
                    break;
                };
                for envelope in &page.envelopes {
                    reducer.fold(envelope);
                }
                cursor = last.seq;
                boundary = Some((last.seq, last.event_id.clone()));
            }
            if let Some((through_seq, boundary_event_id)) = boundary
                && through_seq > checkpoint_cursor
            {
                self.put_usage_checkpoint(&session_id, through_seq, boundary_event_id, &reducer)?;
            }
            reducer.merge_into(&mut slots);
        }
        Ok(slots)
    }

    fn load_usage_checkpoint(
        &self,
        session_id: &SessionId,
    ) -> StoreResult<(UsageJournalReducer, u64)> {
        let checkpoint = match self.session_projection_checkpoint(
            session_id,
            USAGE_CHECKPOINT_PROJECTION,
            USAGE_CHECKPOINT_TIMELINE,
        ) {
            Ok(checkpoint) => checkpoint,
            Err(error) if error.code == ErrorCode::StoreCorrupt => return Ok(Default::default()),
            Err(error) => return Err(error),
        };
        let Some(checkpoint) = checkpoint else {
            return Ok(Default::default());
        };
        let decoded = match rmp_serde::from_slice::<DurableUsageCheckpoint>(&checkpoint.payload) {
            Ok(decoded) => decoded,
            Err(_) => return Ok(Default::default()),
        };
        if decoded.shape_version != USAGE_CHECKPOINT_SHAPE_VERSION
            || decoded.reducer_version != USAGE_CHECKPOINT_REDUCER_VERSION
            || decoded.through_seq != checkpoint.through_seq
        {
            return Ok(Default::default());
        }
        Ok((decoded.reducer, decoded.through_seq))
    }

    fn put_usage_checkpoint(
        &self,
        session_id: &SessionId,
        through_seq: u64,
        boundary_event_id: EventId,
        reducer: &UsageJournalReducer,
    ) -> StoreResult<()> {
        let payload = rmp_serde::to_vec_named(&DurableUsageCheckpointRef {
            shape_version: USAGE_CHECKPOINT_SHAPE_VERSION,
            reducer_version: USAGE_CHECKPOINT_REDUCER_VERSION,
            through_seq,
            reducer,
        })
        .map_err(|error| corrupt(format!("usage checkpoint encode failed: {error}")))?;
        self.put_session_projection_checkpoint(&SessionProjectionCheckpoint {
            session_id: session_id.clone(),
            projection: USAGE_CHECKPOINT_PROJECTION.to_owned(),
            timeline_key: USAGE_CHECKPOINT_TIMELINE.to_owned(),
            through_seq,
            boundary_event_id,
            payload,
        })
    }

    #[cfg(test)]
    #[allow(clippy::expect_used)]
    fn all_usage_reducer_envelopes(&self) -> StoreResult<Vec<RawEnvelope>> {
        let mut envelopes = Vec::new();
        for session_id in self.session_ids()? {
            let mut cursor = 0;
            loop {
                let page = self.read_reducer_page(
                    &session_id,
                    cursor,
                    REPLAY_PAGE_SIZE,
                    USAGE_REPLAY_PAGE_BYTES,
                    USAGE_REDUCER_PAYLOAD_KINDS,
                )?;
                if page.is_empty() {
                    break;
                }
                cursor = page.last().map_or(cursor, |envelope| envelope.seq);
                envelopes.extend(page);
            }
        }
        Ok(envelopes)
    }

    #[cfg(test)]
    #[allow(clippy::expect_used)]
    fn all_journal_envelopes(&self) -> StoreResult<Vec<RawEnvelope>> {
        let mut envelopes = Vec::new();
        for session_id in self.session_ids()? {
            envelopes.extend(self.journal_replay(&session_id)?);
        }
        Ok(envelopes)
    }

    fn install_usage_backfill(
        &self,
        installation_id: &str,
        slots: &BTreeMap<
            crate::usage_ledger::UsageSlotAddress,
            crate::usage_ledger::UsageLedgerSlot,
        >,
        now_ms: u64,
    ) -> StoreResult<()> {
        let mut random = [0_u8; 8];
        getrandom::fill(&mut random).map_err(|error| {
            store_error(
                ErrorCode::Internal,
                format!("OS randomness unavailable for usage backfill staging: {error}"),
                false,
            )
        })?;
        let staging_root = self
            .root
            .join(format!(".usage-backfill-{}", hex::encode(random)));
        fs::create_dir_all(&staging_root)
            .map_err(|error| store_io_error("create usage backfill staging directory", error))?;
        let writer = UsageLedgerWriter::new(
            &staging_root,
            installation_id.to_owned(),
            env!("CARGO_PKG_VERSION"),
        );
        for (address, slot) in slots {
            let address_start = usage_slot_start_ms(&address.date, address.slot)?;
            if address_start.saturating_add(15 * 60 * 1_000) <= now_ms {
                writer.append_slot(address, slot, true)?;
            } else {
                writer.ensure_day(&address.date, true)?;
            }
        }

        let source_dir = staging_root.join("usage");
        let destination_dir = self.root.join("usage");
        fs::create_dir_all(&destination_dir)
            .map_err(|error| store_io_error("create usage history directory", error))?;
        if source_dir.exists() {
            let entries = fs::read_dir(&source_dir)
                .map_err(|error| store_io_error("list staged usage backfill", error))?;
            for entry in entries {
                let entry = entry
                    .map_err(|error| store_io_error("read staged usage backfill entry", error))?;
                let destination = destination_dir.join(entry.file_name());
                if destination.exists() {
                    let date = entry
                        .file_name()
                        .to_string_lossy()
                        .strip_suffix(".jsonl")
                        .map(str::to_owned)
                        .ok_or_else(|| corrupt("staged usage backfill has an invalid filename"))?;
                    let existing = read_usage_day(&self.root, &date)?.ok_or_else(|| {
                        corrupt("existing usage backfill file vanished during validation")
                    })?;
                    if !existing.backfilled || existing.device_id != installation_id {
                        return Err(corrupt(
                            "usage backfill would replace a non-backfill or foreign-device day",
                        ));
                    }
                    fs::remove_file(&destination).map_err(|error| {
                        store_io_error("remove incomplete usage backfill day", error)
                    })?;
                }
                fs::rename(entry.path(), destination)
                    .map_err(|error| store_io_error("publish usage backfill day", error))?;
            }
            haider_platform::sync_directory(&destination_dir)
                .map_err(|error| store_io_error("sync usage history directory", error))?;
        }
        fs::remove_dir_all(&staging_root)
            .map_err(|error| store_io_error("remove usage backfill staging directory", error))?;
        Ok(())
    }

    /// Advances the management revision for an actor-owned state transition
    /// that has no durable command receipt (for example automatic rotation).
    ///
    /// Receipt-backed mutations must use their method-specific finalizer so
    /// final receipt state and the allocated revision share one transaction.
    pub fn advance_management_revision(&self) -> StoreResult<u64> {
        let mut connection = self.connection()?;
        next_profile_counter(
            &mut connection,
            "management_revision",
            "management revision",
        )
    }

    /// B1 — every registered Loom agent type, ordered by id.
    pub fn loom_agent_types(&self) -> StoreResult<Vec<LoomAgentType>> {
        let connection = self.connection()?;
        let mut statement = connection
            .prepare("SELECT record_json FROM loom_agent_types WHERE archived = 0 ORDER BY id")
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(map_sqlite_error)?;
        let mut records = Vec::new();
        for row in rows {
            let json = row.map_err(map_sqlite_error)?;
            records.push(
                serde_json::from_str(&json)
                    .map_err(|_| corrupt("loom agent type record is not decodable"))?,
            );
        }
        Ok(records)
    }

    /// Explicit archive-aware inventory. Callers must opt in; ordinary
    /// catalogs and selection continue to exclude archived rows.
    pub fn loom_agent_types_including_archived(&self) -> StoreResult<Vec<LoomAgentType>> {
        let connection = self.connection()?;
        loom_agent_types_tx(&connection, true)
    }

    /// B1 — every registered Loom workflow (compiled records), ordered by id.
    pub fn loom_workflows(&self) -> StoreResult<Vec<LoomWorkflow>> {
        let connection = self.connection()?;
        let mut statement = connection
            .prepare("SELECT record_json FROM loom_workflows WHERE archived = 0 ORDER BY id")
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(map_sqlite_error)?;
        let mut records = Vec::new();
        for row in rows {
            let json = row.map_err(map_sqlite_error)?;
            records.push(
                serde_json::from_str(&json)
                    .map_err(|_| corrupt("loom workflow record is not decodable"))?,
            );
        }
        Ok(records)
    }

    pub fn loom_workflows_including_archived(&self) -> StoreResult<Vec<LoomWorkflow>> {
        let connection = self.connection()?;
        loom_workflows_tx(&connection, true)
    }

    /// Exact archive facts for entries returned by an include-archived list.
    pub fn loom_archived_entries(&self) -> StoreResult<Vec<LoomRegistryEntryRef>> {
        let connection = self.connection()?;
        loom_archived_entries_tx(&connection)
    }

    /// C1 — one workflow by id (registry read used by the worker's Loom tail).
    pub fn loom_workflow(&self, id: &str) -> StoreResult<Option<LoomWorkflow>> {
        let connection = self.connection()?;
        let record = connection
            .query_row(
                "SELECT record_json FROM loom_workflows WHERE id = ?1 AND archived = 0",
                [id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(map_sqlite_error)?;
        record
            .map(|json| {
                serde_json::from_str(&json)
                    .map_err(|_| corrupt("loom workflow record is not decodable"))
            })
            .transpose()
    }

    /// Reads one immutable registered-workflow revision by the exact template
    /// digest frozen in `GraphPinned`. Current-by-name selection and pinned
    /// execution deliberately use different doors: editing a registry row
    /// must never change the bytes an already-pinned graph executes.
    pub fn loom_workflow_revision(
        &self,
        id: &str,
        template_digest: &str,
    ) -> StoreResult<Option<LoomWorkflow>> {
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT rev, digest, record_json FROM loom_workflow_revisions
                 WHERE id = ?1 ORDER BY rev DESC",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map([id], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(map_sqlite_error)?;
        for row in rows {
            let (revision, digest, json) = row.map_err(map_sqlite_error)?;
            let workflow: LoomWorkflow = serde_json::from_str(&json)
                .map_err(|_| corrupt("loom workflow revision is not decodable"))?;
            let revision = u32::try_from(revision)
                .map_err(|_| corrupt("loom workflow revision is out of range"))?;
            if workflow.rev != revision {
                return Err(corrupt(
                    "loom workflow revision row and record revisions differ",
                ));
            }
            if workflow.id != id || workflow.digest != digest {
                return Err(corrupt(
                    "loom workflow revision row and record identities differ",
                ));
            }
            if graph_template_digest(&workflow.template) == template_digest {
                return Ok(Some(workflow));
            }
        }
        Ok(None)
    }

    /// Reads the immutable workflow row named by a registration receipt.
    /// Confirmation uses this exact address instead of rereading the mutable
    /// current-by-name index after the registration transaction commits.
    pub fn loom_workflow_registered_revision(
        &self,
        id: &str,
        rev: u32,
        digest: &str,
    ) -> StoreResult<Option<LoomWorkflow>> {
        validate_loom_revision_address(id, rev, digest)?;
        let connection = self.connection()?;
        let json = connection
            .query_row(
                "SELECT record_json FROM loom_workflow_revisions
                 WHERE id = ?1 AND rev = ?2 AND digest = ?3",
                params![id, i64::from(rev), digest],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(map_sqlite_error)?;
        let Some(json) = json else {
            return Ok(None);
        };
        let workflow: LoomWorkflow = serde_json::from_str(&json)
            .map_err(|_| corrupt("Loom workflow revision is not decodable"))?;
        if workflow.id != id || workflow.rev != rev || workflow.digest != digest {
            return Err(corrupt(
                "Loom workflow revision does not match its registration address",
            ));
        }
        Ok(Some(workflow))
    }

    /// C2 — one agent type by id (registry read used by typed spawns).
    pub fn loom_agent_type(&self, id: &str) -> StoreResult<Option<LoomAgentType>> {
        let connection = self.connection()?;
        let record = connection
            .query_row(
                "SELECT record_json FROM loom_agent_types WHERE id = ?1 AND archived = 0",
                [id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(map_sqlite_error)?;
        record
            .map(|json| {
                serde_json::from_str(&json)
                    .map_err(|_| corrupt("loom agent type record is not decodable"))
            })
            .transpose()
    }

    /// Resolves one write-once agent-type revision by its registry revision
    /// and execution digest.
    /// The mutable current-by-id row remains the registry index; confirmed
    /// content lives forever in this retained namespace so advancing that
    /// index cannot destroy an older executable contract.
    pub fn loom_agent_type_revision(
        &self,
        id: &str,
        rev: u32,
        digest: &str,
    ) -> StoreResult<Option<LoomAgentType>> {
        validate_loom_revision_address(id, rev, digest)?;
        let path = self
            .root
            .join(LOOM_AGENT_REVISIONS_DIR)
            .join(id)
            .join(format!("{rev}-{digest}.json"));
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(loom_revision_io_error("read", &path, error)),
        };
        let record: LoomAgentType = serde_json::from_slice(&bytes)
            .map_err(|_| corrupt("retained Loom agent-type revision is not decodable"))?;
        if record.id != id || record.rev != rev || record.digest() != digest {
            return Err(corrupt(
                "retained Loom agent-type revision does not match its address",
            ));
        }
        Ok(Some(record))
    }

    fn retain_loom_agent_type_revision(&self, record: &LoomAgentType) -> StoreResult<()> {
        let digest = record.digest();
        validate_loom_revision_address(&record.id, record.rev, &digest)?;
        let revisions = self.root.join(LOOM_AGENT_REVISIONS_DIR);
        let type_revisions = revisions.join(&record.id);
        fs::create_dir_all(&revisions)
            .map_err(|error| loom_revision_io_error("create directory", &revisions, error))?;
        // Repeat parent synchronization even when an earlier attempt created
        // the directory but failed before making that entry durable.
        sync_loom_revision_directory(&self.root)?;
        fs::create_dir_all(&type_revisions)
            .map_err(|error| loom_revision_io_error("create directory", &type_revisions, error))?;
        sync_loom_revision_directory(&revisions)?;
        let path = type_revisions.join(format!("{}-{digest}.json", record.rev));
        if path.exists() {
            verify_retained_loom_agent_type(&path, record)?;
            return sync_loom_revision_directory(&type_revisions);
        }
        let bytes = serde_json::to_vec(record)
            .map_err(|_| corrupt("loom agent type revision is not encodable"))?;
        let mut temporary_path = None;
        let mut temporary = None;
        for _ in 0..32 {
            let counter = LOOM_AGENT_REVISION_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let candidate =
                type_revisions.join(format!(".tmp-loom-agent-{}-{counter}", std::process::id()));
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&candidate)
            {
                Ok(file) => {
                    temporary_path = Some(candidate);
                    temporary = Some(file);
                    break;
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(loom_revision_io_error(
                        "create temporary revision",
                        &candidate,
                        error,
                    ));
                }
            }
        }
        let temporary_path = temporary_path.ok_or_else(|| {
            store_error(
                ErrorCode::Internal,
                format!(
                    "cannot allocate a temporary Loom revision in {}",
                    type_revisions.display()
                ),
                true,
            )
        })?;
        let mut temporary =
            temporary.ok_or_else(|| corrupt("temporary Loom revision file was not opened"))?;
        let write_result = temporary
            .write_all(&bytes)
            .and_then(|()| temporary.sync_all());
        drop(temporary);
        if let Err(error) = write_result {
            let _ = fs::remove_file(&temporary_path);
            let _ = sync_loom_revision_directory(&type_revisions);
            return Err(loom_revision_io_error(
                "persist temporary revision",
                &temporary_path,
                error,
            ));
        }
        let publish_result = match fs::hard_link(&temporary_path, &path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                verify_retained_loom_agent_type(&path, record)
            }
            Err(error) => Err(loom_revision_io_error("publish revision", &path, error)),
        };
        let remove_result = fs::remove_file(&temporary_path).map_err(|error| {
            loom_revision_io_error("remove temporary revision", &temporary_path, error)
        });
        let sync_result = sync_loom_revision_directory(&type_revisions);
        publish_result?;
        remove_result?;
        sync_result
    }

    /// Compatibility create/idempotency door for one agent type. A NEW id
    /// lands at rev 1 and identical content is a no-op. Changed content is
    /// refused: revisions must use the explicit CAS door below, so this
    /// legacy helper can never silently overwrite a concurrent writer.
    pub fn loom_register_agent_type(
        &self,
        record: &LoomAgentType,
    ) -> StoreResult<LoomRegistration> {
        Ok(self
            .loom_register_agent_type_with_install(record)?
            .registration)
    }

    /// Create/idempotently register one typed specialist and atomically
    /// enqueue the required CLI installation for the exact stored rev/digest.
    /// Changed content is accepted only by the explicit CAS variant.
    pub fn loom_register_agent_type_with_install(
        &self,
        record: &LoomAgentType,
    ) -> StoreResult<LoomAgentTypeRegistration> {
        match self.loom_register_agent_type_with_install_inner(record, None)? {
            LoomRegistryMutation::Applied { value, .. } => Ok(value),
            LoomRegistryMutation::Conflict(_) => Err(corrupt(
                "unfenced Loom registration reported a CAS conflict",
            )),
        }
    }

    /// Client-visible CAS door. `rev = 0` means the id must be absent; a
    /// positive revision and optional digest must match the one current row.
    pub fn loom_register_agent_type_with_install_cas(
        &self,
        record: &LoomAgentType,
        expected: &LoomRevisionExpectation,
    ) -> StoreResult<LoomRegistryMutation<LoomAgentTypeRegistration>> {
        self.loom_register_agent_type_with_install_inner(record, Some(expected))
    }

    fn loom_register_agent_type_with_install_inner(
        &self,
        record: &LoomAgentType,
        expected: Option<&LoomRevisionExpectation>,
    ) -> StoreResult<LoomRegistryMutation<LoomAgentTypeRegistration>> {
        let record = &normalize_agent_type(record);
        validate_agent_type(record)?;
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        let existing = transaction
            .query_row(
                "SELECT rev, digest, record_json FROM loom_agent_types WHERE id = ?1",
                [record.id.as_str()],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(map_sqlite_error)?;
        let existing = existing
            .map(|(rev, digest, json)| {
                let stored: LoomAgentType = serde_json::from_str(&json)
                    .map_err(|_| corrupt("loom agent type record is not decodable"))?;
                let stored_rev = u32::try_from(rev)
                    .map_err(|_| corrupt("loom agent type rev is out of range"))?;
                if stored.rev != stored_rev || stored.id != record.id || stored.digest() != digest {
                    return Err(corrupt("loom agent type row and record identities differ"));
                }
                Ok((stored_rev, digest, stored))
            })
            .transpose()?;
        if let Some(expected) = expected
            && !loom_expectation_matches(
                expected,
                existing
                    .as_ref()
                    .map(|(rev, digest, _)| (*rev, digest.as_str())),
            )
        {
            let conflict = loom_revision_conflict(
                expected,
                existing
                    .as_ref()
                    .map(|(rev, digest, _)| (*rev, digest.as_str())),
            );
            transaction.commit().map_err(map_sqlite_error)?;
            return Ok(LoomRegistryMutation::Conflict(conflict));
        }
        let digest = record.digest();
        if expected.is_none()
            && existing
                .as_ref()
                .is_some_and(|(_, current, _)| current != &digest)
        {
            return Err(store_error(
                ErrorCode::RevisionConflict,
                format!(
                    "unfenced Loom agent-type mutation for `{}` refuses changed content; use the CAS door",
                    record.id
                ),
                false,
            ));
        }
        let now = now_ms()?;
        let is_new = existing.is_none();
        let (outcome, changed_record) = match &existing {
            Some((rev, current, _)) if *current == digest => (
                LoomRegistration {
                    id: record.id.clone(),
                    rev: *rev,
                    digest: digest.clone(),
                    updated: false,
                },
                None,
            ),
            Some((rev, _, _)) => {
                let next = rev
                    .checked_add(1)
                    .ok_or_else(|| corrupt("loom agent type rev is out of range"))?;
                let mut stored = record.clone();
                stored.rev = next;
                (
                    LoomRegistration {
                        id: record.id.clone(),
                        rev: next,
                        digest: digest.clone(),
                        updated: true,
                    },
                    Some(stored),
                )
            }
            None => {
                let mut stored = record.clone();
                stored.rev = 1;
                (
                    LoomRegistration {
                        id: record.id.clone(),
                        rev: 1,
                        digest: digest.clone(),
                        updated: true,
                    },
                    Some(stored),
                )
            }
        };
        let install_job = if let Some(stored) = changed_record {
            // Derive from the normalized, store-owned revision before either
            // registry or install rows are inserted. This is deliberately
            // stricter than the display/capability registry validation: a CLI
            // grant must also be a valid required-program install contract.
            let contract = TypedAgentContract::from_loom_agent_type(&stored).map_err(|error| {
                store_error(
                    ErrorCode::InvalidArgument,
                    format!("invalid typed-agent execution contract: {error}"),
                    false,
                )
            })?;
            let json = serde_json::to_string(&stored)
                .map_err(|_| corrupt("loom agent type record is not encodable"))?;
            if let Some((_, _, prior)) = &existing {
                self.retain_loom_agent_type_revision(prior)?;
            }
            self.retain_loom_agent_type_revision(&stored)?;
            if is_new {
                let inserted = transaction
                    .execute(
                        "INSERT INTO loom_agent_types(
                             id, rev, digest, record_json, created_at_ms, updated_at_ms)
                         VALUES (?1, 1, ?2, ?3, ?4, ?4)",
                        params![
                            record.id.as_str(),
                            digest.as_str(),
                            json.as_str(),
                            to_sqlite_integer(now)?
                        ],
                    )
                    .map_err(map_sqlite_error)?;
                if inserted != 1 {
                    return Err(corrupt("typed-agent registry insert affected no row"));
                }
            } else {
                let updated = transaction
                    .execute(
                        "UPDATE loom_agent_types
                         SET rev = ?2, digest = ?3, record_json = ?4, updated_at_ms = ?5
                         WHERE id = ?1",
                        params![
                            record.id.as_str(),
                            i64::from(stored.rev),
                            digest.as_str(),
                            json.as_str(),
                            to_sqlite_integer(now)?
                        ],
                    )
                    .map_err(map_sqlite_error)?;
                if updated != 1 {
                    return Err(corrupt("typed-agent registry update affected no row"));
                }
            }
            enqueue_typed_agent_install(&transaction, &contract, now)?
        } else {
            // Upgrade/backfill seam: a type created by an older daemon can be
            // content-identical yet have no install job. Re-registration at
            // startup creates the missing work exactly once without minting a
            // registry revision; ordinary no-ops with an existing job remain
            // no-ops on both tables.
            let mut stored = record.clone();
            stored.rev = outcome.rev;
            let retained = existing
                .as_ref()
                .map(|(_, _, stored)| stored)
                .ok_or_else(|| corrupt("unchanged Loom agent type has no current record"))?;
            self.retain_loom_agent_type_revision(retained)?;
            let contract = TypedAgentContract::from_loom_agent_type(&stored).map_err(|error| {
                store_error(
                    ErrorCode::InvalidArgument,
                    format!("invalid typed-agent execution contract: {error}"),
                    false,
                )
            })?;
            let already_enqueued = transaction
                .query_row(
                    "SELECT EXISTS(
                         SELECT 1 FROM loom_cli_install_jobs
                         WHERE agent_type_id = ?1 AND agent_type_rev = ?2
                     )",
                    params![stored.id.as_str(), i64::from(stored.rev)],
                    |row| row.get::<_, bool>(0),
                )
                .map_err(map_sqlite_error)?;
            if already_enqueued {
                None
            } else {
                enqueue_typed_agent_install(&transaction, &contract, now)?
            }
        };
        let install_job_id = transaction
            .query_row(
                "SELECT job_id FROM loom_cli_install_jobs
                 WHERE agent_type_id = ?1 AND agent_type_rev = ?2",
                params![outcome.id.as_str(), i64::from(outcome.rev)],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(map_sqlite_error)?;
        let publication_cursor = if outcome.updated {
            let archived = transaction
                .query_row(
                    "SELECT archived FROM loom_agent_types WHERE id = ?1",
                    [outcome.id.as_str()],
                    |row| row.get::<_, bool>(0),
                )
                .map_err(map_sqlite_error)?;
            insert_loom_registry_upsert_events(
                &transaction,
                LoomRegistryEntryKind::AgentType,
                &outcome,
                archived,
                now,
            )?
        } else {
            None
        };
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(LoomRegistryMutation::Applied {
            value: LoomAgentTypeRegistration {
                registration: outcome,
                install_job,
                install_job_id,
            },
            publication_cursor,
        })
    }

    /// Durable install jobs, optionally narrowed by exact job and/or agent
    /// type. Results are stable by type revision so reconnecting callers can
    /// resume polling without depending on row insertion order.
    pub fn typed_agent_install_jobs(
        &self,
        job_id: Option<&str>,
        agent_type_id: Option<&str>,
    ) -> StoreResult<Vec<TypedAgentInstallJob>> {
        let connection = self.connection()?;
        typed_agent_install_jobs_tx(&connection, job_id, agent_type_id)
    }

    /// Durable per-CLI items, with the same optional job/type filters as the
    /// job query. The type filter is resolved through the owning job.
    pub fn typed_agent_install_items(
        &self,
        job_id: Option<&str>,
        agent_type_id: Option<&str>,
    ) -> StoreResult<Vec<TypedAgentInstallItem>> {
        let connection = self.connection()?;
        typed_agent_install_items_tx(&connection, job_id, agent_type_id)
    }

    /// Read jobs and their item progress from one SQLite snapshot so an RPC
    /// never pairs a pre-transition job with post-transition item rows.
    pub fn typed_agent_install_status(
        &self,
        job_id: Option<&str>,
        agent_type_id: Option<&str>,
    ) -> StoreResult<TypedAgentInstallSnapshot> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction().map_err(map_sqlite_error)?;
        let jobs = typed_agent_install_status_jobs_tx(&transaction, job_id, agent_type_id)?;
        let items = typed_agent_install_status_items_tx(&transaction, job_id, agent_type_id)?;
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(TypedAgentInstallSnapshot { jobs, items })
    }

    /// Reset one failed, current-contract install aggregate to queued. This is
    /// deliberately separate from the monotonic installer CAS: only an
    /// explicit negotiated retry may reopen failure, and every CLI item is
    /// reset in the same transaction before a daemon runner is adopted.
    pub fn typed_agent_install_retry(
        &self,
        job_id: &str,
    ) -> StoreResult<TypedAgentInstallRetryResult> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        let Some(actual) = typed_agent_install_job_tx(&transaction, job_id)? else {
            return Ok(TypedAgentInstallRetryResult::JobNotFound);
        };
        if actual.state != TypedAgentInstallState::Failed {
            return Ok(TypedAgentInstallRetryResult::StateNotRetryable {
                state: actual.state,
            });
        }
        let current_contract = transaction
            .query_row(
                "SELECT rev, digest FROM loom_agent_types WHERE id = ?1",
                [actual.agent_type_id.as_str()],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(map_sqlite_error)?;
        let is_current = current_contract.is_some_and(|(rev, digest)| {
            u32::try_from(rev).ok() == Some(actual.agent_type_rev)
                && digest == actual.agent_type_digest
        });
        if !is_current {
            return Ok(TypedAgentInstallRetryResult::ContractNotCurrent);
        }

        let mut reset_items = typed_agent_install_items_tx(&transaction, Some(job_id), None)?;
        let retry_at_ms = reset_items
            .iter()
            .map(|item| item.updated_at_ms)
            .fold(now_ms()?.max(actual.updated_at_ms), u64::max);
        let mut next = actual.clone();
        next.state = TypedAgentInstallState::Queued;
        next.cancelled = false;
        next.progress.completed = 0;
        next.progress.current_cli = None;
        next.error = None;
        next.updated_at_ms = retry_at_ms;
        next.validate()
            .map_err(typed_agent_install_validation_error)?;
        for item in &mut reset_items {
            item.state = TypedAgentInstallState::Queued;
            item.error = None;
            item.updated_at_ms = retry_at_ms;
            item.validate()
                .map_err(typed_agent_install_validation_error)?;
        }
        validate_typed_agent_install_snapshot(&next, &reset_items).map_err(|message| {
            corrupt(format!(
                "typed-agent install retry for `{job_id}` is inconsistent: {message}"
            ))
        })?;

        let reset_job = transaction
            .execute(
                "UPDATE loom_cli_install_jobs
                 SET state = 'queued', completed = 0, current_cli = NULL,
                     error = NULL, cancelled = 0, updated_at_ms = ?2
                 WHERE job_id = ?1 AND state = 'failed' AND updated_at_ms = ?3",
                params![
                    job_id,
                    to_sqlite_integer(retry_at_ms)?,
                    to_sqlite_integer(actual.updated_at_ms)?,
                ],
            )
            .map_err(map_sqlite_error)?;
        if reset_job != 1 {
            return Err(typed_agent_install_conflict(format!(
                "typed-agent install job `{job_id}` lost its retry race"
            )));
        }
        let reset_items_count = transaction
            .execute(
                "UPDATE loom_cli_install_items
                 SET state = 'queued', error = NULL, updated_at_ms = ?2
                 WHERE job_id = ?1",
                params![job_id, to_sqlite_integer(retry_at_ms)?],
            )
            .map_err(map_sqlite_error)?;
        if reset_items_count != usize::from(next.progress.total) {
            return Err(corrupt(format!(
                "typed-agent install retry for `{job_id}` reset {reset_items_count} items; expected {}",
                next.progress.total
            )));
        }
        insert_typed_agent_install_event(&transaction, &next)?;
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(TypedAgentInstallRetryResult::Requeued(next))
    }

    /// Atomically terminalize one queued/running install job. The registry row
    /// and every install item remain intact so an explicit retry can requeue
    /// the same immutable contract later.
    pub fn typed_agent_install_cancel(
        &self,
        install_job_id: &str,
    ) -> StoreResult<TypedAgentInstallCancelResult> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        let Some(actual) = typed_agent_install_job_tx(&transaction, install_job_id)? else {
            transaction.commit().map_err(map_sqlite_error)?;
            return Ok(TypedAgentInstallCancelResult::Unknown);
        };
        if actual.state.is_terminal() {
            transaction.commit().map_err(map_sqlite_error)?;
            return Ok(TypedAgentInstallCancelResult::AlreadyTerminal {
                state: if actual.cancelled {
                    TypedAgentInstallTerminalStateV1::Cancelled
                } else if actual.state == TypedAgentInstallState::Succeeded {
                    TypedAgentInstallTerminalStateV1::Succeeded
                } else {
                    TypedAgentInstallTerminalStateV1::Failed
                },
            });
        }
        let mut cancelled = actual.clone();
        cancelled.state = TypedAgentInstallState::Failed;
        cancelled.cancelled = true;
        cancelled.progress.current_cli = None;
        cancelled.error = Some("typed-agent installation was cancelled".to_owned());
        cancelled.updated_at_ms = now_ms()?.max(actual.updated_at_ms);
        cancelled
            .validate()
            .map_err(typed_agent_install_validation_error)?;
        let changed = transaction
            .execute(
                "UPDATE loom_cli_install_jobs
                 SET state = 'failed', cancelled = 1, current_cli = NULL,
                     error = ?2, updated_at_ms = ?3
                 WHERE job_id = ?1 AND cancelled = 0 AND state = ?4 AND updated_at_ms = ?5",
                params![
                    install_job_id,
                    cancelled.error.as_deref(),
                    to_sqlite_integer(cancelled.updated_at_ms)?,
                    typed_agent_install_state_str(actual.state),
                    to_sqlite_integer(actual.updated_at_ms)?,
                ],
            )
            .map_err(map_sqlite_error)?;
        if changed != 1 {
            return Err(typed_agent_install_conflict(format!(
                "typed-agent install job `{install_job_id}` lost its cancellation race"
            )));
        }
        insert_typed_agent_install_event(&transaction, &cancelled)?;
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(TypedAgentInstallCancelResult::Cancelled)
    }

    /// Read one exact job's durable progress snapshots strictly after the
    /// caller's applied cursor. The response seals a replay-through cursor and
    /// returns at most 128 events; callers page until `next_cursor` reaches it.
    pub fn typed_agent_install_watch(
        &self,
        job_id: &str,
        after_cursor: u64,
    ) -> StoreResult<TypedAgentInstallWatchResult> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction().map_err(map_sqlite_error)?;
        if typed_agent_install_job_tx(&transaction, job_id)?.is_none() {
            return Ok(TypedAgentInstallWatchResult::JobNotFound);
        }
        let head = transaction
            .query_row(
                "SELECT COALESCE(MAX(cursor), 0)
                 FROM loom_cli_install_events WHERE job_id = ?1",
                [job_id],
                |row| row.get::<_, i64>(0),
            )
            .map_err(map_sqlite_error)
            .and_then(|cursor| {
                u64::try_from(cursor)
                    .map_err(|_| corrupt("typed-agent install event cursor is negative"))
            })?;
        if head == 0 {
            return Err(corrupt(format!(
                "typed-agent install job `{job_id}` has no progress history"
            )));
        }
        if after_cursor > head {
            return Ok(TypedAgentInstallWatchResult::CursorAhead {
                requested: after_cursor,
                head,
            });
        }
        let events = typed_agent_install_events_tx(&transaction, job_id, after_cursor)?;
        let next_cursor = events.last().map_or(after_cursor, |event| event.cursor);
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(TypedAgentInstallWatchResult::Watching(
            TypedAgentInstallWatchPage {
                requested_after_cursor: after_cursor,
                replay_through_cursor: head,
                next_cursor,
                events,
            },
        ))
    }

    /// Atomically compare and replace one durable install job plus an
    /// optional item. A stale expected snapshot is a revision conflict; an
    /// illegal or non-monotonic proposed transition is an invalid argument.
    pub fn typed_agent_install_compare_and_swap(
        &self,
        update: &TypedAgentInstallCas,
    ) -> StoreResult<TypedAgentInstallJob> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        let actual_job = typed_agent_install_job_tx(&transaction, &update.expected_job.job_id)?
            .ok_or_else(|| {
                typed_agent_install_conflict(format!(
                    "typed-agent install job `{}` no longer exists",
                    update.expected_job.job_id
                ))
            })?;
        if actual_job != update.expected_job {
            return Err(typed_agent_install_conflict(format!(
                "typed-agent install job `{}` changed before update",
                update.expected_job.job_id
            )));
        }
        actual_job
            .validate_update(&update.next_job)
            .map_err(typed_agent_install_validation_error)?;

        let actual_item = if let Some(item_update) = &update.item {
            if item_update.next.job_id != update.next_job.job_id {
                return Err(store_error(
                    ErrorCode::InvalidArgument,
                    "typed-agent install item must belong to the updated job",
                    false,
                ));
            }
            let actual = typed_agent_install_item_tx(
                &transaction,
                &item_update.expected.job_id,
                item_update.expected.ordinal,
            )?
            .ok_or_else(|| {
                typed_agent_install_conflict(format!(
                    "typed-agent install item `{}:{}` no longer exists",
                    item_update.expected.job_id, item_update.expected.ordinal
                ))
            })?;
            if actual != item_update.expected {
                return Err(typed_agent_install_conflict(format!(
                    "typed-agent install item `{}:{}` changed before update",
                    item_update.expected.job_id, item_update.expected.ordinal
                )));
            }
            actual
                .validate_update(&item_update.next)
                .map_err(typed_agent_install_validation_error)?;
            Some(actual)
        } else {
            None
        };
        validate_typed_agent_install_aggregate(
            &transaction,
            &actual_job,
            &update.next_job,
            update.item.as_ref(),
        )?;

        let updated_job = transaction
            .execute(
                "UPDATE loom_cli_install_jobs
                 SET state = ?2, completed = ?3, current_cli = ?4, error = ?5,
                     updated_at_ms = ?6, cancelled = ?7
                 WHERE job_id = ?1 AND state = ?8 AND updated_at_ms = ?9
                   AND cancelled = ?10",
                params![
                    update.next_job.job_id.as_str(),
                    typed_agent_install_state_str(update.next_job.state),
                    i64::from(update.next_job.progress.completed),
                    update.next_job.progress.current_cli.as_deref(),
                    update.next_job.error.as_deref(),
                    to_sqlite_integer(update.next_job.updated_at_ms)?,
                    update.next_job.cancelled,
                    typed_agent_install_state_str(actual_job.state),
                    to_sqlite_integer(actual_job.updated_at_ms)?,
                    actual_job.cancelled,
                ],
            )
            .map_err(map_sqlite_error)?;
        if updated_job != 1 {
            return Err(typed_agent_install_conflict(format!(
                "typed-agent install job `{}` lost its update race",
                update.next_job.job_id
            )));
        }

        if let (Some(item_update), Some(actual_item)) = (&update.item, actual_item) {
            let updated_item = transaction
                .execute(
                    "UPDATE loom_cli_install_items
                     SET state = ?3, error = ?4, updated_at_ms = ?5
                     WHERE job_id = ?1 AND ordinal = ?2
                       AND state = ?6 AND updated_at_ms = ?7",
                    params![
                        item_update.next.job_id.as_str(),
                        i64::from(item_update.next.ordinal),
                        typed_agent_install_state_str(item_update.next.state),
                        item_update.next.error.as_deref(),
                        to_sqlite_integer(item_update.next.updated_at_ms)?,
                        typed_agent_install_state_str(actual_item.state),
                        to_sqlite_integer(actual_item.updated_at_ms)?,
                    ],
                )
                .map_err(map_sqlite_error)?;
            if updated_item != 1 {
                return Err(typed_agent_install_conflict(format!(
                    "typed-agent install item `{}:{}` lost its update race",
                    item_update.next.job_id, item_update.next.ordinal
                )));
            }
        }
        insert_typed_agent_install_event(&transaction, &update.next_job)?;
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(update.next_job.clone())
    }

    /// Compatibility create/idempotency door for one workflow FROM PIPE
    /// SOURCE. The store remains the compiler authority, but changed content
    /// is refused unless the caller supplies an explicit CAS expectation.
    pub fn loom_register_workflow(&self, source: &str) -> StoreResult<LoomRegistration> {
        match self.loom_register_workflow_inner(source, None)? {
            LoomRegistryMutation::Applied { value, .. } => Ok(value),
            LoomRegistryMutation::Conflict(_) => {
                Err(corrupt("unfenced Loom workflow reported a CAS conflict"))
            }
        }
    }

    pub fn loom_register_workflow_cas(
        &self,
        source: &str,
        expected: &LoomRevisionExpectation,
    ) -> StoreResult<LoomRegistryMutation<LoomRegistration>> {
        self.loom_register_workflow_inner(source, Some(expected))
    }

    fn loom_register_workflow_inner(
        &self,
        source: &str,
        expected: Option<&LoomRevisionExpectation>,
    ) -> StoreResult<LoomRegistryMutation<LoomRegistration>> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        // Resolve @agent-type references against the registry AS OF this
        // transaction.
        let mut agent_types = std::collections::HashMap::new();
        {
            let mut statement = transaction
                .prepare("SELECT record_json FROM loom_agent_types WHERE archived = 0")
                .map_err(map_sqlite_error)?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(map_sqlite_error)?;
            for row in rows {
                let json = row.map_err(map_sqlite_error)?;
                let record: LoomAgentType = serde_json::from_str(&json)
                    .map_err(|_| corrupt("loom agent type record is not decodable"))?;
                agent_types.insert(record.id.clone(), record);
            }
        }
        let ast = parse_pipe(source);
        // Verify-fix C2: template resolution consults the built-in catalog
        // FIRST, so a workflow named after a catalog (or child) template would
        // register as a zombie — listable, never resolvable. Reject up front.
        if let Some(name) = ast.name.as_deref()
            && graph_template(name).is_some()
        {
            return Err(HaiderError::new(
                ErrorCode::InvalidArgument,
                format!("workflow name `{name}` collides with a built-in template"),
                false,
            ));
        }
        let mut workflow =
            compile_pipe(&ast, |id| agent_types.get(id).map(LoomAgentType::signature)).map_err(
                |errors| {
                    HaiderError::new(
                        ErrorCode::InvalidArgument,
                        format!("pipe rejected: {}", errors.join("; ")),
                        false,
                    )
                },
            )?;
        for meta in &mut workflow.meta {
            let Some(type_id) = meta.agent_type.as_deref() else {
                continue;
            };
            let record = agent_types.get(type_id).ok_or_else(|| {
                corrupt(format!(
                    "compiled Loom node references absent agent type `{type_id}`"
                ))
            })?;
            meta.agent_type_rev = Some(record.rev);
            meta.agent_type_digest = Some(record.digest());
        }
        workflow.refresh_digest();
        let existing = transaction
            .query_row(
                "SELECT rev, digest, record_json FROM loom_workflows WHERE id = ?1",
                [workflow.id.as_str()],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(map_sqlite_error)?;
        if let Some(expected) = expected {
            let current = existing
                .as_ref()
                .map(|(rev, digest, _)| {
                    u32::try_from(*rev)
                        .map(|rev| (rev, digest.as_str()))
                        .map_err(|_| corrupt("loom workflow rev is out of range"))
                })
                .transpose()?;
            if !loom_expectation_matches(expected, current) {
                let conflict = loom_revision_conflict(expected, current);
                transaction.commit().map_err(map_sqlite_error)?;
                return Ok(LoomRegistryMutation::Conflict(conflict));
            }
        }
        if expected.is_none()
            && existing
                .as_ref()
                .is_some_and(|(_, current, _)| current != &workflow.digest)
        {
            return Err(store_error(
                ErrorCode::RevisionConflict,
                format!(
                    "unfenced Loom workflow mutation for `{}` refuses changed content; use the CAS door",
                    workflow.id
                ),
                false,
            ));
        }
        let now = now_ms()?;
        let outcome = match existing {
            Some((rev, ref current, ref stored_json)) if *current == workflow.digest => {
                let rev =
                    u32::try_from(rev).map_err(|_| corrupt("loom workflow rev is out of range"))?;
                let stored_version = serde_json::from_str::<LoomWorkflow>(stored_json)
                    .map_err(|_| corrupt("loom workflow record is not decodable"))?
                    .template
                    .version;
                if stored_version != rev {
                    // A pre-stamp record is already an immutable instance: a
                    // run may have pinned its old template digest before this
                    // registration. Heal the current registry by appending a
                    // new revision, never by rewriting that archived row.
                    let next = rev
                        .checked_add(1)
                        .ok_or_else(|| corrupt("loom workflow rev is out of range"))?;
                    workflow.rev = next;
                    workflow.template.version = next;
                    let json = serde_json::to_string(&workflow)
                        .map_err(|_| corrupt("loom workflow record is not encodable"))?;
                    transaction
                        .execute(
                            "UPDATE loom_workflows
                             SET rev = ?2, record_json = ?3, updated_at_ms = ?4
                             WHERE id = ?1",
                            params![
                                workflow.id.as_str(),
                                i64::from(next),
                                json.as_str(),
                                to_sqlite_integer(now)?
                            ],
                        )
                        .map_err(map_sqlite_error)?;
                    transaction
                        .execute(
                            "INSERT INTO loom_workflow_revisions(
                                 id, rev, digest, record_json, created_at_ms)
                             VALUES (?1, ?2, ?3, ?4, ?5)",
                            params![
                                workflow.id.as_str(),
                                i64::from(next),
                                workflow.digest.as_str(),
                                json.as_str(),
                                to_sqlite_integer(now)?
                            ],
                        )
                        .map_err(map_sqlite_error)?;
                    LoomRegistration {
                        id: workflow.id.clone(),
                        rev: next,
                        digest: workflow.digest.clone(),
                        updated: true,
                    }
                } else {
                    workflow.rev = rev;
                    LoomRegistration {
                        id: workflow.id.clone(),
                        rev,
                        digest: workflow.digest.clone(),
                        updated: false,
                    }
                }
            }
            Some((rev, _, _)) => {
                let next = u32::try_from(rev)
                    .ok()
                    .and_then(|rev| rev.checked_add(1))
                    .ok_or_else(|| corrupt("loom workflow rev is out of range"))?;
                workflow.rev = next;
                // Review round 2: the pinned-instance digest is
                // `graph_template_digest(template)`, which binds `version` but
                // never LoomNodeMeta. Stamping `version = rev` makes the
                // template digest a faithful proxy for the WHOLE workflow
                // identity — any content change bumps rev, so tasks/types can
                // never drift behind an unchanged template digest.
                workflow.template.version = next;
                let json = serde_json::to_string(&workflow)
                    .map_err(|_| corrupt("loom workflow record is not encodable"))?;
                transaction
                    .execute(
                        "UPDATE loom_workflows
                         SET rev = ?2, digest = ?3, record_json = ?4, updated_at_ms = ?5
                         WHERE id = ?1",
                        params![
                            workflow.id.as_str(),
                            i64::from(next),
                            workflow.digest.as_str(),
                            json.as_str(),
                            to_sqlite_integer(now)?
                        ],
                    )
                    .map_err(map_sqlite_error)?;
                transaction
                    .execute(
                        "INSERT INTO loom_workflow_revisions(
                             id, rev, digest, record_json, created_at_ms)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![
                            workflow.id.as_str(),
                            i64::from(next),
                            workflow.digest.as_str(),
                            json.as_str(),
                            to_sqlite_integer(now)?
                        ],
                    )
                    .map_err(map_sqlite_error)?;
                LoomRegistration {
                    id: workflow.id.clone(),
                    rev: next,
                    digest: workflow.digest.clone(),
                    updated: true,
                }
            }
            None => {
                workflow.rev = 1;
                workflow.template.version = 1;
                let json = serde_json::to_string(&workflow)
                    .map_err(|_| corrupt("loom workflow record is not encodable"))?;
                transaction
                    .execute(
                        "INSERT INTO loom_workflows(
                             id, rev, digest, record_json, created_at_ms, updated_at_ms)
                         VALUES (?1, 1, ?2, ?3, ?4, ?4)",
                        params![
                            workflow.id.as_str(),
                            workflow.digest.as_str(),
                            json.as_str(),
                            to_sqlite_integer(now)?
                        ],
                    )
                    .map_err(map_sqlite_error)?;
                transaction
                    .execute(
                        "INSERT INTO loom_workflow_revisions(
                             id, rev, digest, record_json, created_at_ms)
                         VALUES (?1, 1, ?2, ?3, ?4)",
                        params![
                            workflow.id.as_str(),
                            workflow.digest.as_str(),
                            json.as_str(),
                            to_sqlite_integer(now)?
                        ],
                    )
                    .map_err(map_sqlite_error)?;
                LoomRegistration {
                    id: workflow.id.clone(),
                    rev: 1,
                    digest: workflow.digest.clone(),
                    updated: true,
                }
            }
        };
        let publication_cursor = if outcome.updated {
            let archived = transaction
                .query_row(
                    "SELECT archived FROM loom_workflows WHERE id = ?1",
                    [outcome.id.as_str()],
                    |row| row.get::<_, bool>(0),
                )
                .map_err(map_sqlite_error)?;
            insert_loom_registry_upsert_events(
                &transaction,
                LoomRegistryEntryKind::Workflow,
                &outcome,
                archived,
                now,
            )?
        } else {
            None
        };
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(LoomRegistryMutation::Applied {
            value: outcome,
            publication_cursor,
        })
    }

    /// Atomically change only selection state. Content revision and digest do
    /// not move; the archive transition receives its own durable cursor.
    pub fn loom_set_archived(
        &self,
        kind: LoomRegistryEntryKind,
        id: &str,
        archived: bool,
        expected: &LoomRevisionExpectation,
    ) -> StoreResult<LoomArchiveResult> {
        validate_loom_registry_id(id)?;
        let table = match kind {
            LoomRegistryEntryKind::AgentType => "loom_agent_types",
            LoomRegistryEntryKind::Workflow => "loom_workflows",
            _ => {
                return Err(store_error(
                    ErrorCode::InvalidArgument,
                    "unknown Loom registry entry kind",
                    false,
                ));
            }
        };
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        let sql = format!("SELECT rev, digest, archived, record_json FROM {table} WHERE id = ?1");
        let current = transaction
            .query_row(&sql, [id], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, bool>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .optional()
            .map_err(map_sqlite_error)?;
        let Some((rev, digest, was_archived, record_json)) = current else {
            let outcome = if loom_expectation_matches(expected, None) {
                LoomArchiveResult::NotFound
            } else {
                LoomArchiveResult::Conflict(loom_revision_conflict(expected, None))
            };
            transaction.commit().map_err(map_sqlite_error)?;
            return Ok(outcome);
        };
        let rev =
            u32::try_from(rev).map_err(|_| corrupt("Loom registry revision is out of range"))?;
        let current = Some((rev, digest.as_str()));
        if !loom_expectation_matches(expected, current) {
            let conflict = loom_revision_conflict(expected, current);
            transaction.commit().map_err(map_sqlite_error)?;
            return Ok(LoomArchiveResult::Conflict(conflict));
        }
        let entry = LoomRegistryEntryRef {
            kind,
            id: id.to_owned(),
            rev,
            digest: digest.clone(),
            archived,
        };
        if was_archived == archived {
            transaction.commit().map_err(map_sqlite_error)?;
            return Ok(LoomArchiveResult::Already(entry));
        }
        if kind == LoomRegistryEntryKind::AgentType {
            let record: LoomAgentType = serde_json::from_str(&record_json)
                .map_err(|_| corrupt("loom agent type record is not decodable"))?;
            self.retain_loom_agent_type_revision(&record)?;
        }
        let update = format!("UPDATE {table} SET archived = ?2, updated_at_ms = ?3 WHERE id = ?1");
        let now = now_ms()?;
        let changed = transaction
            .execute(&update, params![id, archived, to_sqlite_integer(now)?])
            .map_err(map_sqlite_error)?;
        if changed != 1 {
            return Err(corrupt("Loom archive transition affected no row"));
        }
        let change = if archived {
            LoomRegistryDeltaKind::Archived
        } else {
            LoomRegistryDeltaKind::Unarchived
        };
        let cursor = insert_loom_registry_event(&transaction, change, &entry, now)?;
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(LoomArchiveResult::Changed {
            entry,
            publication_cursor: cursor,
        })
    }

    /// Full archive-aware registry baseline from one SQLite snapshot.
    pub fn loom_registry_snapshot(&self) -> StoreResult<LoomRegistrySnapshot> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction().map_err(map_sqlite_error)?;
        let through_cursor = loom_registry_head_tx(&transaction)?;
        let agent_types = loom_agent_type_rows_tx(&transaction, true)?;
        let workflows = loom_workflow_rows_tx(&transaction, true)?;
        let mut entries = Vec::with_capacity(agent_types.len().saturating_add(workflows.len()));
        entries.extend(agent_types.into_iter().map(|(record, archived)| {
            let entry = LoomRegistryEntryRef {
                kind: LoomRegistryEntryKind::AgentType,
                id: record.id.clone(),
                rev: record.rev,
                digest: record.digest(),
                archived,
            };
            LoomRegistrySnapshotEntry {
                entry,
                record: LoomRegistryRecord::AgentType(record),
            }
        }));
        entries.extend(workflows.into_iter().map(|(record, archived)| {
            let entry = LoomRegistryEntryRef {
                kind: LoomRegistryEntryKind::Workflow,
                id: record.id.clone(),
                rev: record.rev,
                digest: record.digest.clone(),
                archived,
            };
            LoomRegistrySnapshotEntry {
                entry,
                record: LoomRegistryRecord::Workflow(record),
            }
        }));
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(LoomRegistrySnapshot {
            through_cursor,
            entries,
        })
    }

    pub fn loom_registry_head(&self) -> StoreResult<u64> {
        let connection = self.connection()?;
        loom_registry_head_tx(&connection)
    }

    /// Replays exact immutable records for one sealed registry cursor range.
    pub fn loom_registry_watch_page(
        &self,
        after_cursor: u64,
        through_cursor: u64,
    ) -> StoreResult<LoomRegistryWatchPage> {
        if after_cursor > through_cursor {
            return Err(store_error(
                ErrorCode::InvalidArgument,
                "Loom registry watch cursor is beyond its replay seal",
                false,
            ));
        }
        let mut connection = self.connection()?;
        let transaction = connection.transaction().map_err(map_sqlite_error)?;
        let head = loom_registry_head_tx(&transaction)?;
        if through_cursor > head {
            return Err(corrupt("Loom registry replay seal is beyond durable head"));
        }
        let mut statement = transaction
            .prepare_cached(
                "SELECT cursor, entry_kind, entry_id, change_kind, rev, digest, archived
                 FROM loom_registry_events
                 WHERE cursor > ?1 AND cursor <= ?2
                 ORDER BY cursor
                 LIMIT 128",
            )
            .map_err(map_sqlite_error)?;
        let mut rows = statement
            .query(params![
                to_sqlite_integer(after_cursor)?,
                to_sqlite_integer(through_cursor)?
            ])
            .map_err(map_sqlite_error)?;
        let mut raw = Vec::new();
        while let Some(row) = rows.next().map_err(map_sqlite_error)? {
            raw.push((
                u64::try_from(row.get::<_, i64>(0).map_err(map_sqlite_error)?)
                    .map_err(|_| corrupt("Loom registry event cursor is negative"))?,
                row.get::<_, String>(1).map_err(map_sqlite_error)?,
                row.get::<_, String>(2).map_err(map_sqlite_error)?,
                row.get::<_, String>(3).map_err(map_sqlite_error)?,
                u32::try_from(row.get::<_, i64>(4).map_err(map_sqlite_error)?)
                    .map_err(|_| corrupt("Loom registry event revision is out of range"))?,
                row.get::<_, String>(5).map_err(map_sqlite_error)?,
                row.get::<_, bool>(6).map_err(map_sqlite_error)?,
            ));
        }
        drop(rows);
        drop(statement);
        let mut deltas = Vec::with_capacity(raw.len());
        for (cursor, kind, id, change, rev, digest, archived) in raw {
            let kind = loom_registry_entry_kind(&kind)?;
            let record =
                loom_registry_record_tx(&self.root, &transaction, kind, &id, rev, &digest)?;
            deltas.push(LoomRegistryDelta {
                cursor,
                change: loom_registry_delta_kind(&change)?,
                entry: LoomRegistryEntryRef {
                    kind,
                    id,
                    rev,
                    digest,
                    archived,
                },
                record,
            });
        }
        let next_cursor = deltas.last().map_or(after_cursor, |delta| delta.cursor);
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(LoomRegistryWatchPage {
            replay_through_cursor: through_cursor,
            next_cursor,
            deltas,
        })
    }

    /// Reads a provider's last-known model catalog.
    pub fn provider_models(&self, provider: &str) -> StoreResult<Option<CachedModels>> {
        let connection = self.connection()?;
        let cached = connection
            .query_row(
                "SELECT models_json, etag, fetched_at_ms
                 FROM provider_models
                 WHERE provider = ?1",
                [provider],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(map_sqlite_error)?;
        cached
            .map(|(models_json, etag, fetched_at_ms)| {
                let fetched_at_ms = u64::try_from(fetched_at_ms)
                    .map_err(|_| corrupt("provider model cache has a negative fetch timestamp"))?;
                Ok(CachedModels {
                    models_json,
                    etag,
                    fetched_at_ms,
                })
            })
            .transpose()
    }

    /// Replaces one provider's last-known model catalog.
    pub fn put_provider_models(
        &self,
        provider: &str,
        models_json: &str,
        etag: Option<&str>,
        fetched_at_ms: u64,
    ) -> StoreResult<()> {
        let connection = self.connection()?;
        connection
            .execute(
                "INSERT INTO provider_models(provider, models_json, etag, fetched_at_ms)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(provider) DO UPDATE SET
                     models_json = excluded.models_json,
                     etag = excluded.etag,
                     fetched_at_ms = excluded.fetched_at_ms",
                params![
                    provider,
                    models_json,
                    etag,
                    to_sqlite_integer(fetched_at_ms)?
                ],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    /// Replaces one provider catalog and advances the management revision in
    /// the same immediate transaction.
    pub fn put_provider_models_and_advance_management_revision(
        &self,
        provider: &str,
        models_json: &str,
        etag: Option<&str>,
        fetched_at_ms: u64,
    ) -> StoreResult<u64> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        transaction
            .execute(
                "INSERT INTO provider_models(provider, models_json, etag, fetched_at_ms)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(provider) DO UPDATE SET
                     models_json = excluded.models_json,
                     etag = excluded.etag,
                     fetched_at_ms = excluded.fetched_at_ms",
                params![
                    provider,
                    models_json,
                    etag,
                    to_sqlite_integer(fetched_at_ms)?
                ],
            )
            .map_err(map_sqlite_error)?;
        let revision = next_management_revision_in_transaction(&transaction)?;
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(revision)
    }

    /// Lists every durable session in stable byte order.
    ///
    /// W3b1 seam (additive): startup recovery must visit every session; the
    /// stable order keeps interrupted recovery passes deterministic.
    pub fn session_ids(&self) -> StoreResult<Vec<SessionId>> {
        let connection = self.connection()?;
        let mut statement = connection
            .prepare_cached("SELECT id FROM sessions ORDER BY id ASC")
            .map_err(map_sqlite_error)?;
        let ids = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(map_sqlite_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_sqlite_error)?;
        Ok(ids.into_iter().map(SessionId::new).collect())
    }

    /// Reads an opaque projection checkpoint. Missing rows are cache misses;
    /// malformed rows are reported as corruption so callers can replay the
    /// authoritative journal without silently losing checkpoint capability.
    pub fn session_projection_checkpoint(
        &self,
        session_id: &SessionId,
        projection: &str,
        timeline_key: &str,
    ) -> StoreResult<Option<SessionProjectionCheckpoint>> {
        let connection = self.connection()?;
        if projection == RUN_HEADS_PROJECTION && timeline_key == RUN_HEADS_TIMELINE {
            return run_heads_projection_checkpoint(&connection, session_id).map(Some);
        }
        let row = connection
            .query_row(
                "SELECT checkpoint.through_seq, checkpoint.boundary_event_id,
                        checkpoint.payload, checkpoint.payload_digest, event.event_id
                 FROM session_projection_checkpoints AS checkpoint
                 LEFT JOIN events AS event
                   ON event.session_id = checkpoint.session_id
                  AND event.seq = checkpoint.through_seq
                 WHERE checkpoint.session_id = ?1
                   AND checkpoint.projection = ?2
                   AND checkpoint.timeline_key = ?3",
                params![session_id.as_str(), projection, timeline_key],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                        row.get::<_, Vec<u8>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                    ))
                },
            )
            .optional()
            .map_err(|error| {
                map_projection_checkpoint_row_error(error, session_id, projection, timeline_key)
            })?;
        let Some((through_seq, boundary_event_id, payload, digest, event_id)) = row else {
            return Ok(None);
        };
        let checkpoint_corrupt = |reason: &str| {
            corrupt(format!(
                "projection checkpoint {projection}/{timeline_key} for session {session_id} is corrupt: {reason}"
            ))
        };
        let Ok(through_seq) = u64::try_from(through_seq) else {
            return Err(checkpoint_corrupt("through_seq is negative"));
        };
        if through_seq == 0 {
            return Err(checkpoint_corrupt("through_seq is zero"));
        }
        if payload.is_empty() {
            return Err(checkpoint_corrupt("payload is empty"));
        }
        if event_id.as_deref() != Some(boundary_event_id.as_str()) {
            return Err(checkpoint_corrupt(
                "boundary event does not match the immutable journal",
            ));
        }
        if digest.as_slice()
            != projection_checkpoint_digest(
                session_id,
                projection,
                timeline_key,
                through_seq,
                &boundary_event_id,
                &payload,
            )
            .as_bytes()
        {
            return Err(checkpoint_corrupt("payload digest does not match"));
        }
        Ok(Some(SessionProjectionCheckpoint {
            session_id: session_id.clone(),
            projection: projection.to_owned(),
            timeline_key: timeline_key.to_owned(),
            through_seq,
            boundary_event_id: EventId::new(boundary_event_id),
            payload,
        }))
    }

    /// Installs a newer checkpoint for one exact projection timeline. The
    /// referenced boundary event must already exist; checkpoint writes never
    /// update or delete journal rows.
    pub fn put_session_projection_checkpoint(
        &self,
        checkpoint: &SessionProjectionCheckpoint,
    ) -> StoreResult<()> {
        if checkpoint.projection.is_empty()
            || checkpoint.timeline_key.is_empty()
            || checkpoint.through_seq == 0
            || checkpoint.payload.is_empty()
        {
            return Err(store_error(
                ErrorCode::InvalidArgument,
                "projection checkpoint coordinates must be non-empty",
                false,
            ));
        }
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        require_session(&transaction, &checkpoint.session_id)?;
        let event_id = transaction
            .query_row(
                "SELECT event_id FROM events WHERE session_id = ?1 AND seq = ?2",
                params![
                    checkpoint.session_id.as_str(),
                    to_sqlite_integer(checkpoint.through_seq)?
                ],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(map_sqlite_error)?;
        if event_id.as_deref() != Some(checkpoint.boundary_event_id.as_str()) {
            return Err(store_error(
                ErrorCode::InvalidArgument,
                "projection checkpoint does not name its immutable boundary event",
                false,
            ));
        }
        let digest = projection_checkpoint_digest(
            &checkpoint.session_id,
            &checkpoint.projection,
            &checkpoint.timeline_key,
            checkpoint.through_seq,
            checkpoint.boundary_event_id.as_str(),
            &checkpoint.payload,
        );
        transaction
            .execute(
                "INSERT INTO session_projection_checkpoints(
                     session_id, projection, timeline_key, through_seq,
                     boundary_event_id, payload, payload_digest
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(session_id, projection, timeline_key) DO UPDATE SET
                     through_seq = excluded.through_seq,
                     boundary_event_id = excluded.boundary_event_id,
                     payload = excluded.payload,
                     payload_digest = excluded.payload_digest
                 WHERE excluded.through_seq >= session_projection_checkpoints.through_seq",
                params![
                    checkpoint.session_id.as_str(),
                    checkpoint.projection,
                    checkpoint.timeline_key,
                    to_sqlite_integer(checkpoint.through_seq)?,
                    checkpoint.boundary_event_id.as_str(),
                    checkpoint.payload,
                    digest.as_bytes().as_slice(),
                ],
            )
            .map_err(map_sqlite_error)?;
        transaction.commit().map_err(map_sqlite_error)
    }

    /// Deletes one already-quiesced session and every row it owns in one
    /// transaction. Runtime quiescence and admission fencing belong to the
    /// daemon; this store operation owns only referentially complete removal.
    pub fn delete_session(&self, session_id: &SessionId) -> StoreResult<()> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        require_session(&transaction, session_id)?;
        for statement in [
            "DELETE FROM run_heads WHERE session_id = ?1",
            "DELETE FROM run_head_sessions WHERE session_id = ?1",
            "DELETE FROM session_projection_checkpoints WHERE session_id = ?1",
            "DELETE FROM graph_telemetry_dirty WHERE session_id = ?1",
            "DELETE FROM graph_telemetry_projection WHERE session_id = ?1",
            "DELETE FROM workflow_node_states WHERE session_id = ?1",
            "DELETE FROM workflow_graph_instances WHERE session_id = ?1",
            "DELETE FROM hook_dispatch_outbox WHERE session_id = ?1",
            "DELETE FROM menu_resolutions WHERE session_id = ?1",
            "DELETE FROM branches WHERE session_id = ?1",
            "DELETE FROM delegations WHERE parent_session_id = ?1 OR child_session_id = ?1",
            "DELETE FROM events WHERE session_id = ?1",
            "DELETE FROM sessions WHERE id = ?1",
        ] {
            transaction
                .execute(statement, [session_id.as_str()])
                .map_err(map_sqlite_error)?;
        }
        transaction.commit().map_err(map_sqlite_error)?;
        self.invalidate_graph_reduction(session_id);
        Ok(())
    }

    /// Loads typed session configuration. Legacy `{}` rows return `None`.
    pub fn session_metadata(
        &self,
        session_id: &SessionId,
    ) -> StoreResult<Option<SessionMetadataV1>> {
        let connection = self.connection()?;
        let metadata = connection
            .query_row(
                "SELECT meta_json FROM sessions WHERE id = ?1",
                [session_id.as_str()],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(map_sqlite_error)?;
        match metadata {
            Some(json) => decode_session_metadata(session_id, &json),
            None => Ok(None),
        }
    }

    /// Pure read of the latest graph projection. Graph truth remains the
    /// append-only event stream; there is intentionally no mutable graph row.
    pub fn graph_status(&self, session_id: &SessionId) -> StoreResult<Option<GraphStatus>> {
        let connection = self.connection()?;
        require_session(&connection, session_id)?;
        Ok(self
            .graph_reductions(&connection, session_id)?
            .active()
            .and_then(|reduction| reduction.status.clone()))
    }

    /// Reads the same-transaction indexed activation projection. When no id
    /// is supplied, the most recently changed workflow graph is returned.
    pub fn workflow_graph_state(
        &self,
        session_id: &SessionId,
        graph_id: Option<&GraphId>,
    ) -> StoreResult<Option<WorkflowGraphState>> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Deferred)
            .map_err(map_sqlite_error)?;
        require_session(&transaction, session_id)?;
        let state = load_workflow_graph_state(&transaction, session_id, graph_id)?;
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(state)
    }

    /// Keyset replay of activation/completion/rejection facts. Cursor zero
    /// starts from the beginning; an ahead cursor is rejected so clients can
    /// repair explicitly instead of silently skipping a rewritten stream.
    pub fn workflow_graph_watch(
        &self,
        session_id: &SessionId,
        after_cursor: u64,
        limit: u32,
    ) -> StoreResult<WorkflowGraphWatchPage> {
        if limit == 0 || limit > WORKFLOW_GRAPH_WATCH_MAX_EVENTS {
            return Err(store_error(
                ErrorCode::InvalidArgument,
                format!("workflow.graph.watch limit must be 1..={WORKFLOW_GRAPH_WATCH_MAX_EVENTS}"),
                false,
            ));
        }
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Deferred)
            .map_err(map_sqlite_error)?;
        require_session(&transaction, session_id)?;
        let replay_through_cursor = latest_seq_in_connection(&transaction, session_id)?;
        if after_cursor > replay_through_cursor {
            let mut error = store_error(
                ErrorCode::InvalidArgument,
                format!("workflow graph cursor {after_cursor} is ahead of {replay_through_cursor}"),
                true,
            );
            error.details = Some(serde_json::json!({
                "kind": "cursor_ahead",
                "requested": after_cursor,
                "head": replay_through_cursor,
            }));
            return Err(error);
        }
        let events = {
            let mut statement = transaction
                .prepare_cached(
                    "SELECT seq, envelope_json
                     FROM events INDEXED BY events_payload_kind_session_seq
                     WHERE session_id = ?1 AND seq > ?2 AND seq <= ?3
                       AND payload_kind IN (?4, ?5, ?6, ?7)
                     ORDER BY seq ASC LIMIT ?8",
                )
                .map_err(map_sqlite_error)?;
            let mut rows = statement
                .query(params![
                    session_id.as_str(),
                    to_sqlite_integer(after_cursor)?,
                    to_sqlite_integer(replay_through_cursor)?,
                    "workflow_graph_started",
                    "workflow_node_activated",
                    "workflow_node_completed",
                    "workflow_node_rejected",
                    to_sqlite_integer(u64::from(limit))?,
                ])
                .map_err(map_sqlite_error)?;
            let mut events = Vec::new();
            while let Some(row) = rows.next().map_err(map_sqlite_error)? {
                let stored_seq: i64 = row.get(0).map_err(map_sqlite_error)?;
                let cursor = u64::try_from(stored_seq)
                    .map_err(|_| corrupt("workflow graph event has a negative cursor"))?;
                let envelope = decode_envelope_column(row, 1).map_err(|error| {
                    corrupt(format!(
                        "invalid workflow graph envelope at {session_id}:{cursor}: {error}"
                    ))
                })?;
                let event = workflow_graph_journal_event(&envelope.payload)?.ok_or_else(|| {
                    corrupt(format!(
                        "workflow graph event index selected a non-runtime fact at {session_id}:{cursor}"
                    ))
                })?;
                events.push(WorkflowGraphWatchEvent { cursor, event });
            }
            events
        };
        transaction.commit().map_err(map_sqlite_error)?;
        let next_cursor = events
            .last()
            .map_or(replay_through_cursor, |event| event.cursor);
        Ok(WorkflowGraphWatchPage {
            requested_after_cursor: after_cursor,
            replay_through_cursor,
            next_cursor,
            events,
        })
    }

    /// Queries any retained graph instance, including superseded roots.
    #[allow(clippy::result_large_err)]
    pub fn graph_status_by_id(
        &self,
        session_id: &SessionId,
        graph_id: &GraphId,
    ) -> StoreResult<Option<GraphStatus>> {
        let connection = self.connection()?;
        require_session(&connection, session_id)?;
        Ok(self
            .graph_reductions(&connection, session_id)?
            .graph(graph_id)
            .and_then(|reduction| reduction.status.clone()))
    }

    /// Full retained reduction for inspection, including immutable specs and
    /// evidence history from superseded graph instances.
    #[allow(clippy::result_large_err)]
    pub fn graph_reduction_by_id(
        &self,
        session_id: &SessionId,
        graph_id: &GraphId,
    ) -> StoreResult<Option<GraphReduction>> {
        let connection = self.connection()?;
        require_session(&connection, session_id)?;
        Ok(self
            .graph_reductions(&connection, session_id)?
            .graph(graph_id)
            .cloned())
    }

    /// Rebuildable per-instance telemetry for one session.
    pub fn graph_runs(&self, session_id: &SessionId) -> StoreResult<Vec<GraphRunRow>> {
        let connection = self.connection()?;
        require_session(&connection, session_id)?;
        let mut runs = self
            .graph_telemetry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .by_session
            .get(session_id)
            .map(|cached| cached.projection.graph_runs.clone())
            .unwrap_or_default();
        trim_to_latest(&mut runs, GRAPH_TELEMETRY_MAX_RUN_ROWS);
        drop(connection);
        Ok(runs)
    }

    /// Rebuildable wall-clock node-attempt intervals for one session.
    pub fn graph_node_attempts(
        &self,
        session_id: &SessionId,
    ) -> StoreResult<Vec<GraphNodeAttemptRow>> {
        let connection = self.connection()?;
        require_session(&connection, session_id)?;
        let mut attempts = self
            .graph_telemetry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .by_session
            .get(session_id)
            .map(|cached| cached.projection.graph_node_attempts.clone())
            .unwrap_or_default();
        trim_to_latest(&mut attempts, GRAPH_TELEMETRY_MAX_ATTEMPT_ROWS);
        drop(connection);
        Ok(attempts)
    }

    /// Profile-wide template adoption aggregate rebuilt from journal facts.
    pub fn graph_template_rollups(&self) -> StoreResult<Vec<GraphTemplateRollup>> {
        let connection = self.connection()?;
        let telemetry = self
            .graph_telemetry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut runs = Vec::new();
        let mut attempts = Vec::new();
        for cached in telemetry.by_session.values() {
            runs.extend(cached.projection.graph_runs.iter().cloned());
            attempts.extend(cached.projection.graph_node_attempts.iter().cloned());
        }
        let mut rollups = graph_template_rollups(&runs, &attempts);
        rollups.truncate(GRAPH_TELEMETRY_MAX_TEMPLATE_ROWS);
        drop(connection);
        Ok(rollups)
    }

    /// Bounded graph inspection snapshot with keyset-paged evidence
    /// provenance. The cursor is bound to both graph identity and journal
    /// head, so a mid-page mutation is rejected rather than mixing snapshots.
    pub fn graph_inspect(
        &self,
        session_id: &SessionId,
        cursor: Option<&str>,
        limit: u32,
    ) -> StoreResult<GraphInspectResult> {
        if limit == 0 {
            return Err(store_error(
                ErrorCode::InvalidArgument,
                "graph.inspect limit must be greater than zero",
                false,
            ));
        }
        let limit = usize::try_from(limit.min(haider_protocol::graph::GRAPH_INSPECT_MAX_PAGE))
            .unwrap_or(usize::MAX);
        let connection = self.connection()?;
        require_session(&connection, session_id)?;
        let through_seq = latest_seq_in_connection(&connection, session_id)?;
        let reductions = self.graph_reductions(&connection, session_id)?;
        let reduction = reductions.active().cloned();
        let status = reduction
            .as_ref()
            .and_then(|reduction| reduction.status.clone());
        let graph_id = status.as_ref().map(|status| status.graph_id.clone());
        let after_seq = match cursor {
            None => 0,
            Some(cursor) => {
                let cursor = serde_json::from_str::<GraphInspectCursor>(cursor).map_err(|_| {
                    store_error(
                        ErrorCode::InvalidArgument,
                        "graph.inspect cursor is malformed",
                        false,
                    )
                })?;
                if graph_id.as_ref() != Some(&cursor.graph_id) || cursor.through_seq != through_seq
                {
                    return Err(store_error(
                        ErrorCode::RevisionConflict,
                        "graph.inspect cursor is stale for the current graph snapshot",
                        false,
                    ));
                }
                cursor.after_seq
            }
        };
        let mut evidence = if let (Some(graph_id), Some(reduction)) = (&graph_id, &reduction) {
            let targets = status
                .as_ref()
                .and_then(|status| status.run_set.as_ref())
                .map(|run_set| {
                    run_set
                        .children
                        .iter()
                        .filter_map(|child| {
                            reductions
                                .graph(&child.graph_id)
                                .map(|reduction| (&child.graph_id, reduction))
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_else(|| vec![(graph_id, reduction)]);
            let mut rows = Vec::new();
            for (target_graph_id, target_reduction) in targets {
                rows.extend(graph_evidence_provenance(
                    &connection,
                    session_id,
                    target_graph_id,
                    target_reduction,
                    after_seq,
                    through_seq,
                    limit.saturating_add(1),
                )?);
            }
            rows.sort_by_key(|row| row.seq);
            rows.truncate(limit.saturating_add(1));
            rows
        } else {
            Vec::new()
        };
        let has_more = evidence.len() > limit;
        if has_more {
            evidence.truncate(limit);
        }
        // Non-empty evidence always belongs to a graph; if that invariant
        // ever broke, pagination simply ends (no cursor) rather than panic.
        let next_cursor = if has_more && let Some(graph_id) = graph_id.clone() {
            let after_seq = evidence.last().map_or(after_seq, |row| row.seq);
            Some(
                serde_json::to_string(&GraphInspectCursor {
                    graph_id,
                    through_seq,
                    after_seq,
                })
                .map_err(|error| {
                    store_error(
                        ErrorCode::Internal,
                        format!("cannot encode graph.inspect cursor: {error}"),
                        false,
                    )
                })?,
            )
        } else {
            None
        };
        let telemetry = self
            .graph_telemetry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut runs = telemetry
            .by_session
            .get(session_id)
            .map(|cached| cached.projection.graph_runs.clone())
            .unwrap_or_default();
        runs.reverse();
        if let Some(active_run_set_id) = status
            .as_ref()
            .and_then(|status| status.run_set.as_ref())
            .map(|run_set| &run_set.run_set_id)
        {
            runs.sort_by_key(|row| match &row.scope {
                Some(GraphRunScope::RunSetAggregate { run_set_id, .. })
                    if run_set_id == active_run_set_id =>
                {
                    0
                }
                Some(GraphRunScope::TodoChild { run_set_id, .. })
                    if run_set_id == active_run_set_id =>
                {
                    1
                }
                _ => 2,
            });
        }
        runs.truncate(GRAPH_INSPECT_MAX_RUNS);
        let mut all_runs = Vec::new();
        let mut all_attempts = Vec::new();
        for cached in telemetry.by_session.values() {
            all_runs.extend(cached.projection.graph_runs.iter().cloned());
            all_attempts.extend(cached.projection.graph_node_attempts.iter().cloned());
        }
        let mut template_rollups = graph_template_rollups(&all_runs, &all_attempts);
        if let Some(active_template) = status.as_ref().map(|status| status.template.as_str())
            && let Some(position) = template_rollups
                .iter()
                .position(|rollup| rollup.template == active_template)
        {
            template_rollups.swap(0, position);
        }
        template_rollups.truncate(GRAPH_INSPECT_MAX_RUNS);
        let mut tool_selection = telemetry
            .by_session
            .get(session_id)
            .map(|cached| cached.projection.tool_selection.clone())
            .unwrap_or_default();
        tool_selection.truncate(GRAPH_INSPECT_MAX_TOOL_SELECTION_ROWS);
        let result = GraphInspectResult {
            snapshot: GraphInspectSnapshot {
                through_seq,
                status,
                runs,
                template_rollups,
                evidence,
                tool_selection,
            },
            next_cursor,
        };
        drop(telemetry);
        drop(connection);
        Ok(result)
    }

    /// Atomically applies the provider finalization guardrail against the
    /// current active-root reduction. Only an Active graph is guarded;
    /// Blocked and every terminal phase pass through unchanged.
    pub fn guard_graph_finalization(
        &self,
        command: &GraphFinalizationCommand,
    ) -> StoreResult<GraphFinalizationOutcome> {
        if command.worker_generation != self.worker_generation {
            return Err(stale_generation(
                command.worker_generation,
                self.worker_generation,
            ));
        }
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        require_typed_session(&transaction, &command.session_id)?;
        let reductions = self.graph_reductions(&transaction, &command.session_id)?;
        let Some(reduction) = reductions.active().cloned() else {
            transaction.commit().map_err(map_sqlite_error)?;
            return Ok(GraphFinalizationOutcome::AllowDone);
        };
        let Some(status) = reduction.status.clone() else {
            transaction.commit().map_err(map_sqlite_error)?;
            return Ok(GraphFinalizationOutcome::AllowDone);
        };
        let aggregate_unfinished = status
            .run_set
            .as_ref()
            .is_some_and(|run_set| !run_set.is_complete());
        if status.phase != GraphPhase::Active
            || (!aggregate_unfinished && !status.nodes.iter().any(|node| !node.satisfied))
        {
            transaction.commit().map_err(map_sqlite_error)?;
            return Ok(GraphFinalizationOutcome::AllowDone);
        }
        let state_digest = graph_finalization_state_digest(&status)?;
        let same_state_deferred = reduction.finalization_deferrals.iter().any(|deferred| {
            deferred.run_id == command.run_id && deferred.state_digest == state_digest
        });
        let run_already_deferred = reduction
            .finalization_deferrals
            .iter()
            .any(|deferred| deferred.run_id == command.run_id);
        let metadata_json = transaction
            .query_row(
                "SELECT meta_json FROM sessions WHERE id = ?1",
                [command.session_id.as_str()],
                |row| row.get::<_, String>(0),
            )
            .map_err(map_sqlite_error)?;
        let metadata =
            decode_session_metadata(&command.session_id, &metadata_json)?.ok_or_else(|| {
                store_error(
                    ErrorCode::InvalidArgument,
                    "legacy session has no interaction policy",
                    false,
                )
            })?;
        let policy = haider_protocol::interaction::InteractionResolutionPolicy::new(
            metadata.interaction_mode,
        );
        if run_already_deferred
            && matches!(
                (
                    policy.resolve(
                        haider_protocol::interaction::InteractionGate::WorkflowUnfinishedRecurrence,
                    ),
                    policy
                        .resolve(haider_protocol::interaction::InteractionGate::GraphHumanConfirm,),
                ),
                (
                    haider_protocol::interaction::InteractionResolution::ReturnWorkflowUnfinished,
                    haider_protocol::interaction::InteractionResolution::FailClosed,
                )
            )
        {
            transaction.commit().map_err(map_sqlite_error)?;
            return Ok(GraphFinalizationOutcome::WorkflowUnfinished {
                graph_id: status.graph_id,
                state_digest,
            });
        }
        if !same_state_deferred {
            if policy
                .resolve(haider_protocol::interaction::InteractionGate::WorkflowUnfinishedFirst)
                != haider_protocol::interaction::InteractionResolution::ContinueWorkflow
            {
                return Err(store_error(
                    ErrorCode::Internal,
                    "unfinished-workflow policy refused its one safe continuation",
                    false,
                ));
            }
            let emit_reminder = !reduction
                .finalization_deferrals
                .iter()
                .any(|deferred| deferred.run_id == command.run_id);
            let deferred = GraphFinalizationDeferred {
                graph_id: status.graph_id.clone(),
                run_id: command.run_id.clone(),
                state_digest: state_digest.clone(),
                unmet_nodes: status
                    .nodes
                    .iter()
                    .filter(|node| !node.satisfied)
                    .map(|node| node.node.clone())
                    .collect(),
            };
            let mut envelopes = vec![graph_finalization_envelope(
                command,
                &state_digest,
                "deferred",
                EventPayload::GraphFinalizationDeferred(deferred),
            )?];
            append_transaction_envelopes(
                &transaction,
                &command.session_id,
                now_ms()?,
                &mut envelopes,
            )?;
            transaction.commit().map_err(map_sqlite_error)?;
            self.extend_graph_reduction(&connection, &command.session_id, &envelopes);
            return Ok(GraphFinalizationOutcome::Deferred {
                graph_id: status.graph_id,
                emit_reminder,
                envelopes,
            });
        }

        let pending = graph_pending_menus(&status);
        if let Some(existing) = reduction.finalization_menus.iter().rev().find(|opened| {
            opened.run_id == command.run_id
                && opened.state_digest == state_digest
                && pending.iter().any(|menu| menu == &opened.menu_id)
        }) {
            transaction.commit().map_err(map_sqlite_error)?;
            return Ok(GraphFinalizationOutcome::ConfirmRequired {
                menu: graph_abandon_confirm_menu(
                    existing.menu_id.clone(),
                    status.graph_id,
                    command.run_id.clone(),
                    state_digest,
                ),
                envelopes: Vec::new(),
            });
        }

        let ordinal = reduction
            .finalization_menus
            .iter()
            .filter(|opened| opened.run_id == command.run_id && opened.state_digest == state_digest)
            .count()
            .saturating_add(1);
        let menu_id = graph_abandon_confirm_menu_id(
            &command.session_id,
            &status.graph_id,
            &command.run_id,
            &state_digest,
            ordinal,
        );
        let menu = graph_abandon_confirm_menu(
            menu_id,
            status.graph_id,
            command.run_id.clone(),
            state_digest.clone(),
        );
        let mut envelopes = vec![graph_finalization_envelope(
            command,
            &state_digest,
            &format!("confirm-{ordinal}"),
            EventPayload::MenuOpened(menu.clone()),
        )?];
        append_transaction_envelopes(&transaction, &command.session_id, now_ms()?, &mut envelopes)?;
        transaction.commit().map_err(map_sqlite_error)?;
        self.extend_graph_reduction(&connection, &command.session_id, &envelopes);
        Ok(GraphFinalizationOutcome::ConfirmRequired { menu, envelopes })
    }

    /// Appends one daemon-observed process signal, or returns the exact
    /// already-committed signal when a lost response is replayed.
    #[allow(clippy::result_large_err)]
    pub fn record_process_signal(
        &self,
        command: &ProcessSignalCommand,
    ) -> StoreResult<ProcessSignalOutcome> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        let event_id = process_signal_event_id(&command.session_id, &command.signal.effect_id);
        if let Some(envelope) = load_envelope_by_event_id(&transaction, &event_id)? {
            let existing = serde_json::from_value::<EventPayload>(envelope.payload.clone())
                .map_err(|error| {
                    corrupt(format!(
                        "process signal event {} does not decode: {error}",
                        envelope.event_id
                    ))
                })?;
            let EventPayload::ProcessSignalRecorded(existing_signal) = existing else {
                return Err(graph_evidence_error(
                    ErrorCode::InvalidArgument,
                    "mismatched_signal_provenance",
                    "process signal event id belongs to another payload",
                ));
            };
            let signal_matches = if command.stamp_workspace_revision {
                process_signal_base_matches(&existing_signal, &command.signal)
            } else {
                existing_signal == command.signal
            };
            if envelope.session_id != command.session_id
                || envelope.branch_id != command.branch_id
                || envelope.run_id.as_ref() != Some(&command.signal.run_id)
                || !signal_matches
            {
                return Err(graph_evidence_error(
                    ErrorCode::InvalidArgument,
                    "mismatched_signal_provenance",
                    "process signal replay does not match the committed signal provenance",
                ));
            }
            let recorded = RecordedProcessSignal {
                effect_id: command.signal.effect_id.clone(),
                signal_seq: envelope.seq,
                worker_generation: envelope.worker_generation,
            };
            transaction.commit().map_err(map_sqlite_error)?;
            return Ok(ProcessSignalOutcome::IdempotentReplay {
                recorded,
                signal: existing_signal,
            });
        }
        if command.worker_generation != self.worker_generation {
            return Err(stale_generation(
                command.worker_generation,
                self.worker_generation,
            ));
        }
        require_typed_session(&transaction, &command.session_id)?;
        let mut signal = command.signal.clone();
        if command.stamp_workspace_revision {
            let outcome_seq = process_effect_outcome_seq(
                &transaction,
                &command.session_id,
                &signal.run_id,
                &signal.effect_id,
            )?;
            let revision =
                workspace_revision_at_or_before(&transaction, &command.session_id, outcome_seq)?;
            signal.workspace_revision = Some(revision);
            signal.subject_digest = process_signal_subject_digest(
                &signal.command_arg_digest,
                &signal.transcript_digest,
                signal.workspace_revision.as_ref(),
            );
        }
        validate_process_signal_provenance(&transaction, &command.session_id, &signal)?;
        let now = now_ms()?;
        let mut envelopes = vec![unstamped_command_envelope(
            event_id,
            &command.session_id,
            command.branch_id.clone(),
            Some(signal.run_id.clone()),
            command.device_id.clone(),
            command.worker_generation,
            EventPayload::ProcessSignalRecorded(signal.clone()),
            PromptRender::Omit,
        )?];
        append_transaction_envelopes(&transaction, &command.session_id, now, &mut envelopes)?;
        let recorded = RecordedProcessSignal {
            effect_id: signal.effect_id.clone(),
            signal_seq: envelopes[0].seq,
            worker_generation: self.worker_generation,
        };
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(ProcessSignalOutcome::Committed {
            recorded,
            signal,
            envelopes,
        })
    }

    /// Receipt lookup before current-state or generation validation.
    pub fn graph_pin_receipt(
        &self,
        command_id: &str,
        request_digest: &str,
        request_json: &str,
    ) -> StoreResult<Option<PinnedGraph>> {
        validate_command_identity(command_id, request_digest, request_json)?;
        let connection = self.connection()?;
        lookup_command_response(
            &connection,
            command_id,
            "graph.pin",
            request_digest,
            request_json,
            "graph-pin",
        )
    }

    /// Receipt lookup for `graph.run_set.open`. Replay precedes live state and
    /// generation checks so response loss remains recoverable after restart.
    #[allow(clippy::result_large_err)]
    pub fn graph_run_set_open_receipt(
        &self,
        command_id: &str,
        request_digest: &str,
        request_json: &str,
    ) -> StoreResult<Option<OpenedGraphRunSet>> {
        validate_command_identity(command_id, request_digest, request_json)?;
        let connection = self.connection()?;
        lookup_command_response(
            &connection,
            command_id,
            "graph.run_set.open",
            request_digest,
            request_json,
            "graph-run-set-open",
        )
    }

    /// Opens one child graph per todo from an exact durable Plan fact. The
    /// event, attachments, immutable pins, dependency-root attempts, and
    /// receipt commit in one SQLite transaction.
    #[allow(clippy::result_large_err)]
    pub fn open_graph_run_set(
        &self,
        command: &GraphRunSetOpenCommand,
    ) -> StoreResult<GraphRunSetOpenOutcome> {
        validate_command_identity(
            &command.command_id,
            &command.request_digest,
            &command.request_json,
        )?;
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        if let Some(opened) = lookup_command_response(
            &transaction,
            &command.command_id,
            "graph.run_set.open",
            &command.request_digest,
            &command.request_json,
            "graph-run-set-open",
        )? {
            transaction.commit().map_err(map_sqlite_error)?;
            return Ok(GraphRunSetOpenOutcome::IdempotentReplay { opened });
        }
        if command.worker_generation != self.worker_generation {
            return Err(stale_generation(
                command.worker_generation,
                self.worker_generation,
            ));
        }
        require_typed_session(&transaction, &command.session_id)?;
        let plan_envelope =
            load_envelope(&transaction, &command.session_id, command.plan_event_seq)?.ok_or_else(
                || {
                    store_error(
                        ErrorCode::InvalidArgument,
                        format!(
                            "Plan event {} does not exist in session {}",
                            command.plan_event_seq, command.session_id
                        ),
                        false,
                    )
                },
            )?;
        let plan_event_id = plan_envelope.event_id.clone();
        let items = plan_items_from_event(&plan_envelope, &command.plan_item_id)?;
        validate_todo_plan_items(&items)?;

        let reductions = self.graph_reductions(&transaction, &command.session_id)?;
        if reductions.run_sets.values().any(|run_set| {
            run_set.plan_item_id == command.plan_item_id && run_set.plan_event_id == plan_event_id
        }) {
            return Err(store_error(
                ErrorCode::RevisionConflict,
                "this exact Plan fact already owns a graph run-set",
                false,
            ));
        }
        let root_graph_id = reductions.active_root.clone().ok_or_else(|| {
            store_error(
                ErrorCode::GraphNotActive,
                "session has no selected Convergence Graph template",
                false,
            )
        })?;
        let root = reductions
            .graph(&root_graph_id)
            .cloned()
            .ok_or_else(|| corrupt("active graph root is absent from the graph forest"))?;
        let root_status = root.status.clone().ok_or_else(|| {
            store_error(
                ErrorCode::GraphNotActive,
                "selected Convergence Graph has no status",
                false,
            )
        })?;
        if matches!(
            root_status.phase,
            GraphPhase::Abandoned | GraphPhase::Superseded
        ) {
            return Err(store_error(
                ErrorCode::GraphNotActive,
                "selected Convergence Graph cannot own a new todo run-set",
                false,
            ));
        }
        let template = haider_protocol::graph::GraphTemplateSpec {
            name: root_status.template.clone(),
            version: root_status.template_version,
            start_node: root_status.start_node.clone(),
            nodes: root.template_nodes.clone(),
        };
        validate_pinned_graph_template(&template)?;
        let digest = graph_template_digest(&template);
        if digest != root_status.digest {
            return Err(corrupt(
                "selected graph digest disagrees with its immutable template",
            ));
        }
        let start_node = template
            .start_node
            .clone()
            .ok_or_else(|| corrupt("validated todo graph template has no start node"))?;
        let run_set_id =
            todo_run_set_id(&command.session_id, &command.plan_item_id, &plan_event_id);
        let child_graph_ids = items
            .iter()
            .map(|todo| {
                todo_child_graph_id(
                    &command.session_id,
                    &run_set_id,
                    &command.plan_item_id,
                    todo.id,
                )
            })
            .collect::<Vec<_>>();
        if child_graph_ids
            .iter()
            .any(|graph_id| reductions.by_graph.contains_key(graph_id))
        {
            return Err(store_error(
                ErrorCode::RevisionConflict,
                "a deterministic todo child graph id already exists",
                false,
            ));
        }

        let now = now_ms()?;
        claim_pending_receipt(
            &transaction,
            &command.command_id,
            "graph.run_set.open",
            &command.request_digest,
            &command.request_json,
            now,
        )?;
        let mut payloads = Vec::new();
        // The root becomes an aggregate and any previous run-set children are
        // retired in this transaction. Their graph menus must be retired too:
        // leaving one journal-open would keep ObserveProjection in
        // `needs_input` even though the graph reducer correctly hides it.
        let mut closed_menus = HashSet::new();
        for menu in graph_pending_menus(&root_status) {
            if closed_menus.insert(menu.clone()) {
                payloads.push(EventPayload::MenuClosed {
                    menu,
                    reason: MenuCloseReason::Dismissed,
                });
            }
        }
        if let Some(previous_id) = reductions.active_run_set.as_ref()
            && let Some(previous) = reductions.run_sets.get(previous_id)
        {
            for child in &previous.children {
                if !matches!(
                    child.phase,
                    GraphPhase::Completed | GraphPhase::Abandoned | GraphPhase::Superseded
                ) {
                    if let Some(status) = reductions
                        .graph(&child.graph_id)
                        .and_then(|graph| graph.status.as_ref())
                    {
                        for menu in graph_pending_menus(status) {
                            if closed_menus.insert(menu.clone()) {
                                payloads.push(EventPayload::MenuClosed {
                                    menu,
                                    reason: MenuCloseReason::Dismissed,
                                });
                            }
                        }
                    }
                    let replacement = items
                        .iter()
                        .position(|todo| todo.id == child.todo_id)
                        .and_then(|index| child_graph_ids.get(index))
                        .cloned()
                        .unwrap_or_else(|| {
                            GraphId::new(format!("retired-by-{}", run_set_id.as_str()))
                        });
                    payloads.push(EventPayload::GraphSuperseded(GraphSuperseded {
                        old: child.graph_id.clone(),
                        new: replacement,
                    }));
                }
            }
        }
        let run_set_index = payloads.len();
        payloads.push(EventPayload::GraphRunSetOpened(GraphRunSetOpened {
            run_set_id: run_set_id.clone(),
            root_graph_id: root_graph_id.clone(),
            plan_item_id: command.plan_item_id.clone(),
            plan_event_id,
            required_children: u32::try_from(items.len()).unwrap_or(u32::MAX),
        }));
        let mut child_indexes = Vec::with_capacity(items.len());
        for (ordinal, (todo, child_graph_id)) in
            items.iter().zip(child_graph_ids.iter()).enumerate()
        {
            let attached_index = payloads.len();
            payloads.push(EventPayload::TodoGraphAttached(TodoGraphAttached {
                run_set_id: run_set_id.clone(),
                plan_item_id: command.plan_item_id.clone(),
                todo_id: todo.id,
                depends_on_todo_id: todo.dep,
                child_graph_id: child_graph_id.clone(),
                ordinal: u32::try_from(ordinal).unwrap_or(u32::MAX),
            }));
            let pinned_index = payloads.len();
            payloads.push(EventPayload::GraphPinned(GraphPinned {
                graph_id: child_graph_id.clone(),
                template: template.name.clone(),
                digest: digest.clone(),
                template_version: template.version,
                start_node: Some(start_node.clone()),
                nodes: template.nodes.clone(),
            }));
            let opened_index = if todo.dep.is_none() {
                let index = payloads.len();
                payloads.push(EventPayload::GraphAttemptOpened(GraphAttemptOpened {
                    graph_id: child_graph_id.clone(),
                    node: start_node.clone(),
                    attempt: 1,
                }));
                if template.nodes.iter().any(|spec| {
                    spec.name == start_node && matches!(spec.gate, GraphGateKind::HumanConfirm)
                }) {
                    payloads.push(EventPayload::MenuOpened(graph_confirm_menu_for(
                        child_graph_id,
                        &template.name,
                        &start_node,
                        1,
                    )));
                }
                Some(index)
            } else {
                None
            };
            child_indexes.push((attached_index, pinned_index, opened_index));
        }
        let mut envelopes = graph_command_envelopes(command, payloads)?;
        augment_workflow_graph_envelopes(
            &transaction,
            &self.cas,
            &command.session_id,
            &mut envelopes,
        )?;
        append_transaction_envelopes(&transaction, &command.session_id, now, &mut envelopes)?;
        let children = items
            .iter()
            .zip(child_graph_ids)
            .zip(child_indexes)
            .map(
                |((todo, child_graph_id), (attached_index, pinned_index, opened_index))| {
                    OpenedTodoGraph {
                        todo_id: todo.id,
                        depends_on_todo_id: todo.dep,
                        child_graph_id,
                        attached_seq: envelopes[attached_index].seq,
                        pinned_seq: envelopes[pinned_index].seq,
                        opened_seq: opened_index.map(|index| envelopes[index].seq),
                    }
                },
            )
            .collect::<Vec<_>>();
        let opened = OpenedGraphRunSet {
            session_id: command.session_id.clone(),
            run_set_id,
            root_graph_id,
            plan_item_id: command.plan_item_id.clone(),
            plan_event_seq: command.plan_event_seq,
            template: template.name,
            digest,
            run_set_opened_seq: envelopes[run_set_index].seq,
            through_seq: envelopes.last().map_or(0, |envelope| envelope.seq),
            children,
            worker_generation: self.worker_generation,
        };
        finalize_command_receipt(
            &transaction,
            &command.command_id,
            command.session_id.as_str(),
            plan_envelope.run_id.as_ref().map(RunId::as_str),
            Some(opened.run_set_opened_seq),
            &opened,
            now,
            "graph-run-set-open",
        )?;
        transaction.commit().map_err(map_sqlite_error)?;
        self.extend_graph_reduction(&connection, &command.session_id, &envelopes);
        Ok(GraphRunSetOpenOutcome::Committed { opened, envelopes })
    }

    /// Atomically pins ship-loop and opens BUILD attempt/epoch 1. A blocked
    /// graph may be exited by re-pin; that transaction first abandons it.
    pub fn pin_graph(&self, command: &GraphPinCommand) -> StoreResult<GraphPinOutcome> {
        self.pin_graph_with_expected_digest(command, None)
    }

    /// Pins a graph only when the registry bytes resolved inside the same
    /// transaction still have the caller-authorized digest.
    pub fn pin_graph_matching_digest(
        &self,
        command: &GraphPinCommand,
        expected_digest: &str,
    ) -> StoreResult<GraphPinOutcome> {
        self.pin_graph_with_expected_digest(command, Some(expected_digest))
    }

    fn pin_graph_with_expected_digest(
        &self,
        command: &GraphPinCommand,
        expected_digest: Option<&str>,
    ) -> StoreResult<GraphPinOutcome> {
        validate_command_identity(
            &command.command_id,
            &command.request_digest,
            &command.request_json,
        )?;
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        if let Some(pinned) = lookup_command_response(
            &transaction,
            &command.command_id,
            "graph.pin",
            &command.request_digest,
            &command.request_json,
            "graph-pin",
        )? {
            transaction.commit().map_err(map_sqlite_error)?;
            return Ok(GraphPinOutcome::IdempotentReplay { pinned });
        }
        if command.worker_generation != self.worker_generation {
            return Err(stale_generation(
                command.worker_generation,
                self.worker_generation,
            ));
        }
        require_typed_session(&transaction, &command.session_id)?;
        let resolved =
            resolve_graph_template_tx(&transaction, &command.template)?.ok_or_else(|| {
                store_error(
                    ErrorCode::InvalidArgument,
                    format!("unknown graph template `{}`", command.template),
                    false,
                )
            })?;
        let template = resolved.template;
        validate_pinned_graph_template(&template)?;
        let digest = graph_template_digest(&template);
        if let Some(expected_digest) = expected_digest
            && expected_digest != digest
        {
            return Err(workflow_revision_conflict(
                expected_digest,
                &digest,
                resolved.revision,
            ));
        }
        let current = self
            .graph_reductions(&transaction, &command.session_id)?
            .active()
            .and_then(|reduction| reduction.status.clone());
        if current
            .as_ref()
            .is_some_and(|status| status.phase == GraphPhase::Active)
        {
            return Err(store_error(
                ErrorCode::GraphAlreadyActive,
                "session already has an active Convergence Graph",
                false,
            ));
        }

        let now = now_ms()?;
        claim_pending_receipt(
            &transaction,
            &command.command_id,
            "graph.pin",
            &command.request_digest,
            &command.request_json,
            now,
        )?;
        let mut payloads = Vec::new();
        if let Some(status) = current.filter(|status| status.phase == GraphPhase::Blocked) {
            payloads.push(EventPayload::GraphAbandoned(GraphAbandoned {
                graph_id: status.graph_id,
                why: "re-pinned".into(),
            }));
        }
        let pinned_index = payloads.len();
        let start_node = template.start_node.clone().ok_or_else(|| {
            store_error(
                ErrorCode::StoreCorrupt,
                "validated graph template lost its start node",
                false,
            )
        })?;
        let human_start = template.nodes.iter().any(|spec| {
            spec.name == start_node && matches!(spec.gate, GraphGateKind::HumanConfirm)
        });
        let template_name = template.name.clone();
        payloads.push(EventPayload::GraphPinned(GraphPinned {
            graph_id: command.graph_id.clone(),
            template: template.name,
            digest: digest.clone(),
            template_version: template.version,
            start_node: Some(start_node.clone()),
            nodes: template.nodes,
        }));
        payloads.push(EventPayload::GraphAttemptOpened(GraphAttemptOpened {
            graph_id: command.graph_id.clone(),
            node: start_node.clone(),
            attempt: 1,
        }));
        if human_start {
            payloads.push(EventPayload::MenuOpened(graph_confirm_menu_for(
                &command.graph_id,
                &template_name,
                &start_node,
                1,
            )));
        }
        let mut envelopes = graph_command_envelopes(command, payloads)?;
        augment_workflow_graph_envelopes(
            &transaction,
            &self.cas,
            &command.session_id,
            &mut envelopes,
        )?;
        append_transaction_envelopes(&transaction, &command.session_id, now, &mut envelopes)?;
        let pinned = PinnedGraph {
            session_id: command.session_id.clone(),
            graph_id: command.graph_id.clone(),
            template: command.template.clone(),
            digest,
            pinned_seq: envelopes[pinned_index].seq,
            opened_seq: envelopes[pinned_index + 1].seq,
            worker_generation: self.worker_generation,
        };
        finalize_command_receipt(
            &transaction,
            &command.command_id,
            command.session_id.as_str(),
            None,
            Some(pinned.pinned_seq),
            &pinned,
            now,
            "graph-pin",
        )?;
        transaction.commit().map_err(map_sqlite_error)?;
        self.extend_graph_reduction(&connection, &command.session_id, &envelopes);
        Ok(GraphPinOutcome::Committed { pinned, envelopes })
    }

    /// Attaches one already-pinned child graph to the exact parent graph
    /// attempt which admitted its spawn. This reference never enters either
    /// graph's dependency set.
    #[allow(clippy::result_large_err)]
    pub fn attach_child_graph(
        &self,
        command: &ChildGraphAttachCommand,
    ) -> StoreResult<ChildGraphAttachOutcome> {
        validate_command_identity(
            &command.command_id,
            &command.request_digest,
            &command.request_json,
        )?;
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        if let Some(attached) = lookup_command_response(
            &transaction,
            &command.command_id,
            "child.graph.attach",
            &command.request_digest,
            &command.request_json,
            "child-graph-attach",
        )? {
            transaction.commit().map_err(map_sqlite_error)?;
            return Ok(ChildGraphAttachOutcome::IdempotentReplay { attached });
        }
        if command.worker_generation != self.worker_generation {
            return Err(stale_generation(
                command.worker_generation,
                self.worker_generation,
            ));
        }
        require_typed_session(&transaction, &command.session_id)?;
        require_typed_session(&transaction, &command.attachment.child_session_id)?;
        let attachment = &command.attachment;
        let parent_reductions = self.graph_reductions(&transaction, &command.session_id)?;
        let parent_reduction = parent_reductions
            .graph(&attachment.parent_attempt.graph_id)
            .ok_or_else(|| {
                graph_evidence_error(
                    ErrorCode::RevisionConflict,
                    "stale_child_attachment",
                    "parent graph attempt no longer exists",
                )
            })?;
        let parent = parent_reduction.status.as_ref().ok_or_else(|| {
            graph_evidence_error(
                ErrorCode::RevisionConflict,
                "stale_child_attachment",
                "parent graph attempt has no reducible status",
            )
        })?;
        if parent.graph_id != attachment.parent_attempt.graph_id
            || !parent.node_is_ready(&attachment.parent_attempt.node)
            || parent
                .nodes
                .iter()
                .find(|node| node.node == attachment.parent_attempt.node)
                .is_none_or(|node| node.current_attempt != Some(attachment.parent_attempt.attempt))
        {
            return Err(graph_evidence_error(
                ErrorCode::RevisionConflict,
                "stale_child_attachment",
                "child graph does not attach to the current exact parent attempt",
            ));
        }
        let parent_node = parent_reduction
            .template_nodes
            .iter()
            .find(|node| node.name == attachment.parent_attempt.node)
            .ok_or_else(|| {
                graph_evidence_error(
                    ErrorCode::InvalidArgument,
                    "unknown_child_parent_slot",
                    "child workflow named no declared parent graph node",
                )
            })?;
        let slot = parent_node
            .verify_slots
            .iter()
            .find(|slot| slot.id == attachment.parent_slot)
            .ok_or_else(|| {
                graph_evidence_error(
                    ErrorCode::InvalidArgument,
                    "unknown_child_parent_slot",
                    "child workflow named no declared parent evidence slot",
                )
            })?;
        if slot.authority != attachment.parent_authority {
            return Err(graph_evidence_error(
                ErrorCode::InvalidArgument,
                "child_authority_growth",
                "child workflow attachment authority differs from its parent slot grant",
            ));
        }
        let attached_slot_is_required = match parent_node.gate {
            GraphGateKind::AllOfN { n } => usize::try_from(n).ok().is_some_and(|required| {
                required > parent_node.verify_slots.len().saturating_sub(1)
            }),
            GraphGateKind::CommandGreen | GraphGateKind::HumanConfirm => false,
        };
        if !attached_slot_is_required {
            return Err(graph_evidence_error(
                ErrorCode::InvalidArgument,
                "non_reservable_parent_slot",
                "workflow attachment slot is not required to settle its parent attempt",
            ));
        }
        let logical_attachments = load_child_graph_attachments(&transaction, &command.session_id)?
            .into_iter()
            .filter(|existing| {
                existing.parent_attempt == attachment.parent_attempt
                    && existing.parent_slot == attachment.parent_slot
            })
            .collect::<Vec<_>>();
        if !logical_attachments.is_empty() {
            let kind = if logical_attachments.len() == 1 && logical_attachments[0] == *attachment {
                "duplicate_child_attachment"
            } else {
                "colliding_child_attachment"
            };
            return Err(graph_evidence_error(
                ErrorCode::RevisionConflict,
                kind,
                "parent attempt slot already owns a child graph attachment",
            ));
        }
        let delegation = lookup_delegation_by_parent_call(
            &transaction,
            &command.session_id,
            &attachment.parent_run_id,
            &attachment.parent_call_id,
        )?
        .ok_or_else(|| {
            graph_evidence_error(
                ErrorCode::InvalidArgument,
                "mismatched_child_provenance",
                "child workflow has no durable delegation coordinates",
            )
        })?;
        if delegation.tool_item_id != attachment.parent_tool_item_id
            || delegation.child_session_id != attachment.child_session_id
            || delegation.child_run_id != attachment.child_run_id
        {
            return Err(graph_evidence_error(
                ErrorCode::InvalidArgument,
                "mismatched_child_provenance",
                "child workflow attachment differs from its delegation record",
            ));
        }
        let child_reductions = self.graph_reductions(&transaction, &attachment.child_session_id)?;
        let child = child_reductions
            .graph(&attachment.child_graph_id)
            .and_then(|reduction| reduction.status.as_ref())
            .ok_or_else(|| {
                graph_evidence_error(
                    ErrorCode::InvalidArgument,
                    "mismatched_child_provenance",
                    "attached child graph is not pinned in the child session",
                )
            })?;
        if child.template != attachment.template || child.digest != attachment.digest {
            return Err(graph_evidence_error(
                ErrorCode::RevisionConflict,
                "mismatched_child_provenance",
                "attached child graph does not match its pinned template",
            ));
        }
        let now = now_ms()?;
        claim_pending_receipt(
            &transaction,
            &command.command_id,
            "child.graph.attach",
            &command.request_digest,
            &command.request_json,
            now,
        )?;
        let mut envelopes = graph_command_envelopes(
            command,
            vec![EventPayload::ChildGraphAttached(attachment.clone())],
        )?;
        envelopes[0].branch_id = command.parent_branch_id.clone();
        envelopes[0].run_id = Some(attachment.parent_run_id.clone());
        append_transaction_envelopes(&transaction, &command.session_id, now, &mut envelopes)?;
        let attached = AttachedChildGraph {
            parent_session_id: command.session_id.clone(),
            child_session_id: attachment.child_session_id.clone(),
            child_graph_id: attachment.child_graph_id.clone(),
            attached_seq: envelopes[0].seq,
            worker_generation: self.worker_generation,
        };
        finalize_command_receipt(
            &transaction,
            &command.command_id,
            command.session_id.as_str(),
            Some(attachment.parent_run_id.as_str()),
            Some(attached.attached_seq),
            &attached,
            now,
            "child-graph-attach",
        )?;
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(ChildGraphAttachOutcome::Committed {
            attached,
            envelopes,
        })
    }

    /// Records one successful equivalent child workflow observation. Journal
    /// facts are the cache authority; no mutable cache table can outrun them.
    #[allow(clippy::result_large_err)]
    pub fn observe_child_template_success(
        &self,
        command: &ChildTemplateObservationCommand,
    ) -> StoreResult<ChildTemplateObservation> {
        validate_child_cache_key(&command.key)?;
        validate_pinned_graph_template(&command.template)?;
        if command.worker_generation != self.worker_generation {
            return Err(stale_generation(
                command.worker_generation,
                self.worker_generation,
            ));
        }
        if child_gate_structure(&command.template) != command.key.gate_structure {
            return Err(child_cache_error(
                "colliding_child_template_cache",
                "template gate structure differs from its simple cache key",
            ));
        }
        let digest = graph_template_digest(&command.template);
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        require_typed_session(&transaction, &command.parent_session_id)?;
        graph_attempt_opened_seq(
            &transaction,
            &command.parent_session_id,
            &command.parent_attempt.graph_id,
            command.parent_attempt.node.clone(),
            command.parent_attempt.attempt,
        )?;
        let collapse = load_envelope(
            &transaction,
            &command.parent_session_id,
            command.collapse_evidence_seq,
        )?
        .ok_or_else(|| {
            child_cache_error(
                "unsuccessful_child_template_observation",
                "cache observation names no durable collapsed child evidence",
            )
        })?;
        let evidence = match serde_json::from_value::<EventPayload>(collapse.payload) {
            Ok(EventPayload::EvidenceRecorded(evidence)) => evidence,
            _ => {
                return Err(child_cache_error(
                    "unsuccessful_child_template_observation",
                    "cache observation sequence is not graph evidence",
                ));
            }
        };
        let source_matches = matches!(
            &evidence.source,
            GraphEvidenceSource::ChildContract {
                child_session_id,
                child_run_id,
                child_graph_id,
                report_digest,
                workspace_revision,
            } if child_session_id == &command.child_contract.child_session_id
                && child_run_id == &command.child_contract.child_run_id
                && child_graph_id == &command.child_contract.child_graph_id
                && report_digest == &command.child_contract.report_digest
                && workspace_revision == &command.child_contract.workspace_revision
        );
        if evidence.graph_id != command.parent_attempt.graph_id
            || evidence.node != command.parent_attempt.node
            || evidence.attempt != command.parent_attempt.attempt
            || evidence.verdict != EvidenceVerdict::Green
            || !source_matches
        {
            return Err(child_cache_error(
                "unsuccessful_child_template_observation",
                "cache promotion requires exact green collapsed child evidence",
            ));
        }
        let attachments = load_child_graph_attachments(&transaction, &command.parent_session_id)?
            .into_iter()
            .filter(|attached| {
                attached.parent_attempt == command.parent_attempt
                    && attached.child_session_id == command.child_contract.child_session_id
                    && attached.child_run_id == command.child_contract.child_run_id
                    && attached.child_graph_id == command.child_contract.child_graph_id
            })
            .collect::<Vec<_>>();
        let [attached] = attachments.as_slice() else {
            return Err(child_cache_error(
                "unsuccessful_child_template_observation",
                "cache observation has no single exact unchanged child attachment",
            ));
        };
        if evidence.slot.as_deref() != Some(attached.parent_slot.as_str())
            || evidence.subject_digest.as_deref()
                != Some(child_contract_subject_digest(&command.child_contract).as_str())
        {
            return Err(child_cache_error(
                "unsuccessful_child_template_observation",
                "collapsed evidence slot or subject differs from its child attachment",
            ));
        }
        if attached.cache_key != command.key
            || attached.template != command.template.name
            || attached.digest != digest
        {
            return Err(child_cache_error(
                "colliding_child_template_cache",
                "successful child attachment differs from the cache observation",
            ));
        }
        let mut observations = load_child_template_observations(&transaction)?;
        validate_child_cache_bucket(&transaction, &command.key, &observations)?;
        let exact = observations.iter().any(|(session_id, observed)| {
            session_id == &command.parent_session_id
                && observed.cache_key == command.key
                && observed.parent_attempt == command.parent_attempt
        });
        if exact {
            let distinct_attempts = child_cache_distinct_attempts(&command.key, &observations);
            transaction.commit().map_err(map_sqlite_error)?;
            return Ok(ChildTemplateObservation {
                distinct_attempts,
                promoted: distinct_attempts >= 3,
                envelopes: Vec::new(),
            });
        }
        let observed = ChildTemplateObserved {
            cache_key: command.key.clone(),
            parent_attempt: command.parent_attempt.clone(),
            collapse_evidence_seq: command.collapse_evidence_seq,
            child_contract: command.child_contract.clone(),
            template: command.template.clone(),
            digest: digest.clone(),
        };
        observations.push((command.parent_session_id.clone(), observed.clone()));
        validate_child_cache_bucket(&transaction, &command.key, &observations)?;
        let distinct_attempts = child_cache_distinct_attempts(&command.key, &observations);
        let bucket_digest = command.key.digest();
        let attempt_string = command.parent_attempt.attempt.to_string();
        let event_id = EventId::new(format!(
            "child-template-observed-{}",
            stable_digest(&[
                &bucket_digest,
                command.parent_session_id.as_str(),
                command.parent_attempt.graph_id.as_str(),
                command.parent_attempt.node.as_str(),
                &attempt_string,
            ])
        ));
        let mut envelopes = vec![unstamped_command_envelope(
            event_id,
            &command.parent_session_id,
            None,
            None,
            command.device_id.clone(),
            command.worker_generation,
            EventPayload::ChildTemplateObserved(observed),
            PromptRender::Omit,
        )?];
        if distinct_attempts == 3 {
            envelopes.push(unstamped_command_envelope(
                EventId::new(format!("child-template-promoted-{}", command.key.digest())),
                &command.parent_session_id,
                None,
                None,
                command.device_id.clone(),
                command.worker_generation,
                EventPayload::ChildTemplatePromoted(ChildTemplatePromoted {
                    cache_key: command.key.clone(),
                    template: command.template.name.clone(),
                    digest,
                    distinct_parent_attempts: distinct_attempts,
                }),
                PromptRender::Omit,
            )?);
        }
        append_transaction_envelopes(
            &transaction,
            &command.parent_session_id,
            now_ms()?,
            &mut envelopes,
        )?;
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(ChildTemplateObservation {
            distinct_attempts,
            promoted: distinct_attempts >= 3,
            envelopes,
        })
    }

    /// Loads a promoted child template and revalidates current bounds,
    /// authority policy, key payload, digest, and gate structure every time.
    #[allow(clippy::result_large_err)]
    pub fn child_template_cache_lookup(
        &self,
        key: &ChildTemplateCacheKey,
    ) -> StoreResult<Option<ChildTemplateCacheEntry>> {
        validate_child_cache_key(key)?;
        let connection = self.connection()?;
        let observations = load_child_template_observations(&connection)?;
        validate_child_cache_bucket(&connection, key, &observations)?;
        let distinct_attempts = child_cache_distinct_attempts(key, &observations);
        if distinct_attempts < 3 {
            return Ok(None);
        }
        let observed = observations
            .iter()
            .find_map(|(_, observed)| (observed.cache_key == *key).then_some(observed))
            .ok_or_else(|| corrupt("promoted child cache bucket has no observation"))?;
        validate_pinned_graph_template(&observed.template).map_err(|_| {
            child_cache_error(
                "poisoned_child_template_cache",
                "cached child template failed bounds, authority, or policy revalidation",
            )
        })?;
        if graph_template_digest(&observed.template) != observed.digest
            || child_gate_structure(&observed.template) != key.gate_structure
        {
            return Err(child_cache_error(
                "poisoned_child_template_cache",
                "cached child template failed digest or gate-structure revalidation",
            ));
        }
        Ok(Some(ChildTemplateCacheEntry {
            key: key.clone(),
            template: observed.template.clone(),
            digest: observed.digest.clone(),
            distinct_attempts,
        }))
    }

    #[allow(clippy::result_large_err)]
    pub fn graph_switch_receipt(
        &self,
        command_id: &str,
        request_digest: &str,
        request_json: &str,
    ) -> StoreResult<Option<SwitchedGraph>> {
        validate_command_identity(command_id, request_digest, request_json)?;
        let connection = self.connection()?;
        lookup_command_response(
            &connection,
            command_id,
            "graph.switch",
            request_digest,
            request_json,
            "graph-switch",
        )
    }

    /// Atomically supersedes one active root, retires all of its human menus,
    /// pins an immutable replacement, and opens the replacement START.
    #[allow(clippy::result_large_err)]
    pub fn switch_graph(&self, command: &GraphSwitchCommand) -> StoreResult<GraphSwitchOutcome> {
        self.switch_graph_with_expected_digest(command, None)
    }

    /// Switches only when the registry bytes resolved inside the transaction
    /// still match the client-observed immutable template digest.
    #[allow(clippy::result_large_err)]
    pub fn switch_graph_matching_digest(
        &self,
        command: &GraphSwitchCommand,
        expected_digest: &str,
    ) -> StoreResult<GraphSwitchOutcome> {
        self.switch_graph_with_expected_digest(command, Some(expected_digest))
    }

    #[allow(clippy::result_large_err)]
    fn switch_graph_with_expected_digest(
        &self,
        command: &GraphSwitchCommand,
        expected_digest: Option<&str>,
    ) -> StoreResult<GraphSwitchOutcome> {
        validate_command_identity(
            &command.command_id,
            &command.request_digest,
            &command.request_json,
        )?;
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        if let Some(switched) = lookup_command_response(
            &transaction,
            &command.command_id,
            "graph.switch",
            &command.request_digest,
            &command.request_json,
            "graph-switch",
        )? {
            transaction.commit().map_err(map_sqlite_error)?;
            return Ok(GraphSwitchOutcome::IdempotentReplay { switched });
        }
        if command.worker_generation != self.worker_generation {
            return Err(stale_generation(
                command.worker_generation,
                self.worker_generation,
            ));
        }
        require_typed_session(&transaction, &command.session_id)?;
        let resolved = match command.template_spec.clone() {
            Some(template) if template.name == command.template => ResolvedGraphTemplate {
                revision: template.version,
                template,
            },
            Some(_) => {
                return Err(store_error(
                    ErrorCode::InvalidArgument,
                    "authored workflow name differs from its switch selector",
                    false,
                ));
            }
            None => {
                resolve_graph_template_tx(&transaction, &command.template)?.ok_or_else(|| {
                    store_error(
                        ErrorCode::InvalidArgument,
                        format!("unknown graph template `{}`", command.template),
                        false,
                    )
                })?
            }
        };
        let template = resolved.template;
        validate_pinned_graph_template(&template)?;
        let reductions = self.graph_reductions(&transaction, &command.session_id)?;
        if reductions.active_root.as_ref() != Some(&command.old_graph_id) {
            return Err(store_error(
                ErrorCode::RevisionConflict,
                format!(
                    "graph switch expected active root {}, found {}",
                    command.old_graph_id,
                    reductions.active_root.as_ref().map_or("-", GraphId::as_str)
                ),
                false,
            ));
        }
        if reductions.by_graph.contains_key(&command.new_graph_id) {
            return Err(store_error(
                ErrorCode::InvalidArgument,
                format!("replacement graph {} already exists", command.new_graph_id),
                false,
            ));
        }
        let old_status = reductions
            .graph(&command.old_graph_id)
            .and_then(|reduction| reduction.status.clone())
            .filter(GraphStatus::is_unfinished)
            .ok_or_else(|| {
                store_error(
                    ErrorCode::GraphNotActive,
                    "expected graph is not an unfinished active root",
                    false,
                )
            })?;
        let unfinished_children = active_unfinished_run_set_children(&reductions);
        let start_node = template.start_node.clone().ok_or_else(|| {
            store_error(
                ErrorCode::StoreCorrupt,
                "validated graph template lost its start node",
                false,
            )
        })?;
        let human_start = template.nodes.iter().any(|spec| {
            spec.name == start_node && matches!(spec.gate, GraphGateKind::HumanConfirm)
        });
        let template_name = template.name.clone();
        let digest = graph_template_digest(&template);
        if let Some(expected_digest) = expected_digest
            && expected_digest != digest
        {
            return Err(workflow_revision_conflict(
                expected_digest,
                &digest,
                resolved.revision,
            ));
        }
        let now = now_ms()?;
        claim_pending_receipt(
            &transaction,
            &command.command_id,
            "graph.switch",
            &command.request_digest,
            &command.request_json,
            now,
        )?;
        let mut payloads = vec![EventPayload::GraphSuperseded(GraphSuperseded {
            old: command.old_graph_id.clone(),
            new: command.new_graph_id.clone(),
        })];
        let mut closed_menus = HashSet::new();
        for menu in graph_pending_menus(&old_status) {
            if closed_menus.insert(menu.clone()) {
                payloads.push(EventPayload::MenuClosed {
                    menu,
                    reason: MenuCloseReason::Dismissed,
                });
            }
        }
        // A run-set root is only an aggregate projection: every unfinished
        // child is an independently pinned graph with its own menus and
        // effect coordinates. Switching the session must terminalize that
        // complete owned forest in the same journal transaction.
        for (child_graph_id, child_menus) in unfinished_children {
            for menu in child_menus {
                if closed_menus.insert(menu.clone()) {
                    payloads.push(EventPayload::MenuClosed {
                        menu,
                        reason: MenuCloseReason::Dismissed,
                    });
                }
            }
            payloads.push(EventPayload::GraphSuperseded(GraphSuperseded {
                old: child_graph_id,
                new: command.new_graph_id.clone(),
            }));
        }
        let pinned_index = payloads.len();
        payloads.push(EventPayload::GraphPinned(GraphPinned {
            graph_id: command.new_graph_id.clone(),
            template: template.name,
            digest: digest.clone(),
            template_version: template.version,
            start_node: Some(start_node.clone()),
            nodes: template.nodes,
        }));
        payloads.push(EventPayload::GraphAttemptOpened(GraphAttemptOpened {
            graph_id: command.new_graph_id.clone(),
            node: start_node.clone(),
            attempt: 1,
        }));
        if human_start {
            payloads.push(EventPayload::MenuOpened(graph_confirm_menu_for(
                &command.new_graph_id,
                &template_name,
                &start_node,
                1,
            )));
        }
        let mut envelopes = graph_command_envelopes(command, payloads)?;
        augment_workflow_graph_envelopes(
            &transaction,
            &self.cas,
            &command.session_id,
            &mut envelopes,
        )?;
        append_transaction_envelopes(&transaction, &command.session_id, now, &mut envelopes)?;
        let switched = SwitchedGraph {
            session_id: command.session_id.clone(),
            old_graph_id: command.old_graph_id.clone(),
            new_graph_id: command.new_graph_id.clone(),
            template: command.template.clone(),
            digest,
            superseded_seq: envelopes[0].seq,
            pinned_seq: envelopes[pinned_index].seq,
            opened_seq: envelopes[pinned_index + 1].seq,
            worker_generation: self.worker_generation,
        };
        finalize_command_receipt(
            &transaction,
            &command.command_id,
            command.session_id.as_str(),
            None,
            Some(switched.superseded_seq),
            &switched,
            now,
            "graph-switch",
        )?;
        transaction.commit().map_err(map_sqlite_error)?;
        self.extend_graph_reduction(&connection, &command.session_id, &envelopes);
        Ok(GraphSwitchOutcome::Committed {
            switched,
            envelopes,
        })
    }

    pub fn graph_abandon_receipt(
        &self,
        command_id: &str,
        request_digest: &str,
        request_json: &str,
    ) -> StoreResult<Option<AbandonedGraph>> {
        validate_command_identity(command_id, request_digest, request_json)?;
        let connection = self.connection()?;
        lookup_command_response(
            &connection,
            command_id,
            "graph.abandon",
            request_digest,
            request_json,
            "graph-abandon",
        )
    }

    pub fn abandon_graph(&self, command: &GraphAbandonCommand) -> StoreResult<GraphAbandonOutcome> {
        validate_command_identity(
            &command.command_id,
            &command.request_digest,
            &command.request_json,
        )?;
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        if let Some(abandoned) = lookup_command_response(
            &transaction,
            &command.command_id,
            "graph.abandon",
            &command.request_digest,
            &command.request_json,
            "graph-abandon",
        )? {
            transaction.commit().map_err(map_sqlite_error)?;
            return Ok(GraphAbandonOutcome::IdempotentReplay { abandoned });
        }
        if command.worker_generation != self.worker_generation {
            return Err(stale_generation(
                command.worker_generation,
                self.worker_generation,
            ));
        }
        require_typed_session(&transaction, &command.session_id)?;
        let reductions = self.graph_reductions(&transaction, &command.session_id)?;
        let status = reductions
            .active()
            .and_then(|reduction| reduction.status.clone())
            .filter(GraphStatus::is_unfinished)
            .ok_or_else(|| {
                store_error(
                    ErrorCode::GraphNotActive,
                    "session has no unfinished Convergence Graph",
                    false,
                )
            })?;
        let unfinished_children = active_unfinished_run_set_children(&reductions);
        let why = normalize_graph_why(&command.why)?;
        let now = now_ms()?;
        claim_pending_receipt(
            &transaction,
            &command.command_id,
            "graph.abandon",
            &command.request_digest,
            &command.request_json,
            now,
        )?;
        let mut payloads = vec![EventPayload::GraphAbandoned(GraphAbandoned {
            graph_id: status.graph_id.clone(),
            why: why.clone(),
        })];
        let mut closed_menus = HashSet::new();
        for menu in graph_pending_menus(&status) {
            // Leaving an active SHIP obligation retires its durable menu too;
            // otherwise the answered-menu fallback scan would keep exposing
            // a permanently stale, unanswerable card after abandonment.
            if closed_menus.insert(menu.clone()) {
                payloads.push(EventPayload::MenuClosed {
                    menu,
                    reason: MenuCloseReason::Dismissed,
                });
            }
        }
        for (child_graph_id, child_menus) in unfinished_children {
            for menu in child_menus {
                if closed_menus.insert(menu.clone()) {
                    payloads.push(EventPayload::MenuClosed {
                        menu,
                        reason: MenuCloseReason::Dismissed,
                    });
                }
            }
            payloads.push(EventPayload::GraphAbandoned(GraphAbandoned {
                graph_id: child_graph_id,
                why: why.clone(),
            }));
        }
        let mut envelopes = graph_command_envelopes(command, payloads)?;
        augment_workflow_graph_envelopes(
            &transaction,
            &self.cas,
            &command.session_id,
            &mut envelopes,
        )?;
        append_transaction_envelopes(&transaction, &command.session_id, now, &mut envelopes)?;
        let abandoned = AbandonedGraph {
            session_id: command.session_id.clone(),
            graph_id: status.graph_id,
            abandoned_seq: envelopes[0].seq,
            worker_generation: self.worker_generation,
        };
        finalize_command_receipt(
            &transaction,
            &command.command_id,
            command.session_id.as_str(),
            None,
            Some(abandoned.abandoned_seq),
            &abandoned,
            now,
            "graph-abandon",
        )?;
        transaction.commit().map_err(map_sqlite_error)?;
        self.extend_graph_reduction(&connection, &command.session_id, &envelopes);
        Ok(GraphAbandonOutcome::Committed {
            abandoned,
            envelopes,
        })
    }

    /// Records one daemon-produced screen observation on the node that is
    /// snapshotted before backend execution. The observation is deliberately
    /// supplemental: it uses the ordinary `EvidenceRecorded` event channel
    /// and graph provenance read model, but cannot satisfy or exhaust a gate.
    ///
    /// Models cannot call this path. The store validates all supplied
    /// coordinates against the durable effect lifecycle and graph attempt,
    /// and it alone selects authority, subject, verdict, and revision.
    pub fn record_computer_evidence(
        &self,
        command: &ComputerEvidenceCommand,
    ) -> StoreResult<ComputerEvidenceOutcome> {
        validate_command_identity(
            &command.command_id,
            &command.request_digest,
            &command.request_json,
        )?;
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        if let Some(recorded) = lookup_command_response(
            &transaction,
            &command.command_id,
            "graph.computer-evidence",
            &command.request_digest,
            &command.request_json,
            "computer-evidence",
        )? {
            transaction.commit().map_err(map_sqlite_error)?;
            return Ok(ComputerEvidenceOutcome::IdempotentReplay { recorded });
        }
        if command.worker_generation != self.worker_generation {
            return Err(stale_generation(
                command.worker_generation,
                self.worker_generation,
            ));
        }
        require_typed_session(&transaction, &command.session_id)?;
        if command.call_id.trim().is_empty()
            || command.effect_args_digest.trim().is_empty()
            || command.image.media_type != "image/png"
            || command.image.width == 0
            || command.image.height == 0
            || command.image.byte_len == 0
        {
            return Err(store_error(
                ErrorCode::InvalidArgument,
                "computer graph evidence requires an exact admitted PNG artifact",
                false,
            ));
        }
        let image_bytes = self.cas.get(&command.image.artifact)?;
        validate_image_block(&image_bytes, &command.image)?;
        let effect_outcome_seq =
            validate_computer_observation_effect(&transaction, &command.session_id, command)?;

        let reductions = self.graph_reductions(&transaction, &command.session_id)?;
        let Some(reduction) = reductions.active() else {
            transaction.commit().map_err(map_sqlite_error)?;
            return Ok(ComputerEvidenceOutcome::StaleGraph);
        };
        let Some(status) = reduction
            .status
            .as_ref()
            .filter(|status| status.phase == GraphPhase::Active)
        else {
            transaction.commit().map_err(map_sqlite_error)?;
            return Ok(ComputerEvidenceOutcome::StaleGraph);
        };
        if status.graph_id != command.graph_id
            || status.current_node.as_ref() != Some(&command.node)
            || !status.node_is_ready(&command.node)
            || status
                .nodes
                .iter()
                .find(|node| node.node == command.node)
                .is_none_or(|node| node.current_attempt != Some(command.attempt))
        {
            transaction.commit().map_err(map_sqlite_error)?;
            return Ok(ComputerEvidenceOutcome::StaleGraph);
        }
        let epoch_seq = graph_attempt_opened_seq(
            &transaction,
            &command.session_id,
            &command.graph_id,
            command.node.clone(),
            command.attempt,
        )?;
        if effect_outcome_seq < epoch_seq {
            transaction.commit().map_err(map_sqlite_error)?;
            return Ok(ComputerEvidenceOutcome::StaleGraph);
        }

        let graph_id = command.graph_id.clone();
        let node = command.node.clone();
        let attempt = command.attempt;
        let workspace_revision =
            workspace_revision_at_or_before(&transaction, &command.session_id, effect_outcome_seq)?;
        let subject_digest = computer_observation_subject_digest(
            &command.run_id,
            &command.call_id,
            &command.effect_id,
            &command.effect_args_digest,
            command.observation,
            &command.image,
            &workspace_revision,
        );
        let detail = normalize_evidence_detail(&command.detail);
        if detail.is_empty() {
            return Err(store_error(
                ErrorCode::InvalidArgument,
                "computer evidence detail is empty after normalization",
                false,
            ));
        }
        let fingerprint = evidence_fingerprint(&detail);
        let evidence = EvidenceRecorded {
            graph_id: graph_id.clone(),
            node: node.clone(),
            attempt,
            verdict: EvidenceVerdict::Green,
            detail,
            fingerprint: fingerprint.clone(),
            slot: None,
            subject_digest: Some(subject_digest),
            source: GraphEvidenceSource::ComputerObservation {
                run_id: command.run_id.clone(),
                call_id: command.call_id.clone(),
                effect_id: command.effect_id.clone(),
                effect_args_digest: command.effect_args_digest.clone(),
                observation: command.observation,
                image: command.image.clone(),
                workspace_revision,
            },
        };

        let now = now_ms()?;
        claim_pending_receipt(
            &transaction,
            &command.command_id,
            "graph.computer-evidence",
            &command.request_digest,
            &command.request_json,
            now,
        )?;
        let mut envelopes =
            graph_command_envelopes(command, vec![EventPayload::EvidenceRecorded(evidence)])?;
        append_transaction_envelopes(&transaction, &command.session_id, now, &mut envelopes)?;
        let recorded = RecordedGraphEvidence {
            graph_id,
            node,
            attempt,
            fingerprint,
            evidence_seq: envelopes[0].seq,
            through_seq: envelopes[0].seq,
            worker_generation: self.worker_generation,
        };
        finalize_command_receipt(
            &transaction,
            &command.command_id,
            command.session_id.as_str(),
            Some(command.run_id.as_str()),
            Some(recorded.evidence_seq),
            &recorded,
            now,
            "computer-evidence",
        )?;
        transaction.commit().map_err(map_sqlite_error)?;
        self.extend_graph_reduction(&connection, &command.session_id, &envelopes);
        Ok(ComputerEvidenceOutcome::Committed {
            recorded,
            envelopes,
        })
    }

    /// Stamps one evidence fact and performs the deterministic gate reduction
    /// in the same receipt transaction. No model-authored value can directly
    /// select a successor node or attempt number.
    pub fn record_graph_evidence(
        &self,
        command: &GraphEvidenceCommand,
    ) -> StoreResult<GraphEvidenceOutcome> {
        validate_command_identity(
            &command.command_id,
            &command.request_digest,
            &command.request_json,
        )?;
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        if let Some(recorded) = lookup_command_response(
            &transaction,
            &command.command_id,
            "graph.evidence",
            &command.request_digest,
            &command.request_json,
            "graph-evidence",
        )? {
            transaction.commit().map_err(map_sqlite_error)?;
            return Ok(GraphEvidenceOutcome::IdempotentReplay { recorded });
        }
        if command.worker_generation != self.worker_generation {
            return Err(stale_generation(
                command.worker_generation,
                self.worker_generation,
            ));
        }
        require_typed_session(&transaction, &command.session_id)?;
        let reductions = self.graph_reductions(&transaction, &command.session_id)?;
        let active_run_set = reductions
            .active_run_set
            .as_ref()
            .and_then(|run_set_id| reductions.run_sets.get(run_set_id));
        let active_child = active_run_set.is_some_and(|run_set| {
            run_set
                .children
                .iter()
                .any(|child| child.graph_id == command.graph_id)
        });
        if reductions.active_root.as_ref() != Some(&command.graph_id) && !active_child {
            let superseded = reductions
                .graph(&command.graph_id)
                .and_then(|reduction| reduction.status.as_ref())
                .is_some_and(|status| status.phase == GraphPhase::Superseded);
            if superseded {
                return Err(graph_evidence_error(
                    ErrorCode::GraphNotActive,
                    "superseded",
                    format!("graph {} has been superseded", command.graph_id),
                ));
            }
            return Err(store_error(
                ErrorCode::GraphNotActive,
                format!("graph {} is not the active root", command.graph_id),
                false,
            ));
        }
        if reductions.active_root.as_ref() == Some(&command.graph_id)
            && active_run_set.is_some_and(|run_set| run_set.root_graph_id == command.graph_id)
        {
            return Err(store_error(
                ErrorCode::GraphWrongNode,
                "the active root is a todo aggregate; evidence must target one attached child graph",
                false,
            ));
        }
        let reduction = reductions
            .graph(&command.graph_id)
            .cloned()
            .ok_or_else(|| {
                store_error(
                    ErrorCode::GraphNotActive,
                    "session has no Convergence Graph",
                    false,
                )
            })?;
        let status = reduction.status.clone().ok_or_else(|| {
            store_error(
                ErrorCode::GraphNotActive,
                "session has no Convergence Graph",
                false,
            )
        })?;
        if status.phase != GraphPhase::Active {
            return Err(store_error(
                ErrorCode::GraphNotActive,
                "Convergence Graph is not accepting evidence",
                false,
            ));
        }
        let current_node = command.node.clone();
        let node_spec = reduction
            .template_nodes
            .iter()
            .find(|spec| spec.name == current_node)
            .ok_or_else(|| {
                store_error(
                    ErrorCode::GraphWrongNode,
                    format!(
                        "graph node {} is absent from the pinned template",
                        current_node
                    ),
                    false,
                )
            })?;
        let node_status = status
            .nodes
            .iter()
            .find(|node| node.node == current_node)
            .ok_or_else(|| {
                store_error(
                    ErrorCode::StoreCorrupt,
                    "open graph node is absent from its pinned template",
                    false,
                )
            })?;
        if !status.node_is_ready(&current_node)
            || matches!(node_spec.gate, GraphGateKind::HumanConfirm)
            || node_status.current_attempt.is_none()
        {
            return Err(store_error(
                ErrorCode::GraphWrongNode,
                format!(
                    "graph_evidence named {}, but the first open obligation is {}",
                    command.node.label(),
                    status
                        .current_node
                        .as_ref()
                        .map_or("-", GraphNodeName::label)
                ),
                false,
            ));
        }
        let detail = normalize_evidence_detail(&command.detail);
        if detail.is_empty() {
            return Err(store_error(
                ErrorCode::InvalidArgument,
                "graph evidence detail is empty after normalization",
                false,
            ));
        }
        let fingerprint = evidence_fingerprint(&detail);
        let attempt = node_status.current_attempt.ok_or_else(|| {
            store_error(
                ErrorCode::GraphWrongNode,
                "graph evidence target has no open node-local attempt",
                false,
            )
        })?;
        let graph_id = status.graph_id.clone();
        let slot_spec = if node_spec.verify_slots.is_empty() {
            if command.slot.is_some() {
                return Err(graph_evidence_error(
                    ErrorCode::InvalidArgument,
                    "unknown_evidence_slot",
                    "the pinned graph node does not declare evidence slots",
                ));
            }
            None
        } else {
            let slot_id = command.slot.as_deref().ok_or_else(|| {
                graph_evidence_error(
                    ErrorCode::InvalidArgument,
                    "unknown_evidence_slot",
                    "graph evidence must name one declared slot",
                )
            })?;
            Some(
                node_spec
                    .verify_slots
                    .iter()
                    .find(|slot| slot.id == slot_id)
                    .ok_or_else(|| {
                        graph_evidence_error(
                            ErrorCode::InvalidArgument,
                            "unknown_evidence_slot",
                            format!("evidence slot `{slot_id}` is not declared by the pinned node"),
                        )
                    })?,
            )
        };
        if command.child_contract.is_none()
            && let Some(slot_id) = command.slot.as_deref()
        {
            let reserved = load_child_graph_attachments(&transaction, &command.session_id)?
                .into_iter()
                .any(|attached| {
                    attached.parent_attempt.graph_id == graph_id
                        && attached.parent_attempt.node == current_node
                        && attached.parent_attempt.attempt == attempt
                        && attached.parent_slot == slot_id
                });
            if reserved {
                return Err(graph_evidence_error(
                    ErrorCode::RevisionConflict,
                    "child_slot_reserved",
                    "parent evidence slot is reserved for its attached child contract",
                ));
            }
        }
        if let (Some(signal_ref), Some(slot_id)) =
            (command.signal.as_ref(), command.slot.as_deref())
        {
            let other_process_sources = status
                .nodes
                .iter()
                .find(|node| node.node == current_node)
                .into_iter()
                .flat_map(|node| node.evidence_slots.iter())
                .filter(|slot| slot.id != slot_id)
                .filter_map(|slot| match slot.source.as_ref() {
                    Some(GraphEvidenceSource::ProcessSignal {
                        run_id,
                        call_id,
                        effect_id,
                    }) => Some(ProcessSignalRef {
                        run_id: run_id.clone(),
                        call_id: call_id.clone(),
                        effect_id: effect_id.clone(),
                    }),
                    _ => None,
                })
                .collect::<Vec<_>>();
            if !other_process_sources.is_empty() {
                let incoming = load_process_signal(&transaction, &command.session_id, signal_ref)?;
                for prior_ref in other_process_sources {
                    let prior = load_process_signal(&transaction, &command.session_id, &prior_ref)?;
                    if prior_ref == *signal_ref
                        || prior.signal.command_arg_digest == incoming.signal.command_arg_digest
                    {
                        return Err(graph_evidence_error(
                            ErrorCode::InvalidArgument,
                            "mismatched_signal_provenance",
                            "one process command subject cannot prove multiple evidence slots in the same graph epoch",
                        ));
                    }
                }
            }
        }
        let source = validate_graph_evidence_authority(
            &transaction,
            &command.session_id,
            command,
            slot_spec,
            &graph_id,
            attempt,
        )?;
        let evidence = EvidenceRecorded {
            graph_id: graph_id.clone(),
            node: current_node.clone(),
            attempt,
            verdict: command.verdict,
            detail,
            fingerprint: fingerprint.clone(),
            slot: command.slot.clone(),
            subject_digest: command.subject_digest.clone(),
            source,
        };
        let mut previous_node_attempt = None;
        let mut previous_red_fingerprint = None;
        for prior in &reduction.evidence {
            if prior.graph_id != graph_id || prior.node != current_node || prior.attempt >= attempt
            {
                continue;
            }
            if previous_node_attempt.is_none_or(|previous| prior.attempt > previous) {
                previous_node_attempt = Some(prior.attempt);
                previous_red_fingerprint = None;
            }
            if previous_node_attempt == Some(prior.attempt) && prior.verdict == EvidenceVerdict::Red
            {
                previous_red_fingerprint = Some(prior.fingerprint.as_str());
            }
        }
        let no_progress = command.verdict == EvidenceVerdict::Red
            && previous_red_fingerprint.is_some_and(|previous| previous == fingerprint);
        let mut payloads = vec![EventPayload::EvidenceRecorded(evidence)];
        let pending = graph_pending_menus(&status);
        let pending_finalization_menus = reduction
            .finalization_menus
            .iter()
            .filter(|opened| pending.iter().any(|menu| menu == &opened.menu_id))
            .map(|opened| opened.menu_id.clone())
            .collect::<Vec<_>>();
        payloads.extend(pending_finalization_menus.iter().cloned().map(|menu| {
            EventPayload::MenuClosed {
                menu,
                reason: MenuCloseReason::Dismissed,
            }
        }));
        if no_progress {
            payloads.push(EventPayload::GraphBlocked(GraphBlocked {
                graph_id: graph_id.clone(),
                node: current_node.clone(),
                reason: GraphBlockReason::NoProgress,
            }));
        } else {
            let evidence_count = node_status
                .evidence
                .green
                .saturating_add(node_status.evidence.red)
                .saturating_add(1);
            let effective_green = match command.verdict {
                EvidenceVerdict::Green => node_status.evidence.effective_green.saturating_add(1),
                EvidenceVerdict::Red => 0,
            };
            let satisfied = match &node_spec.gate {
                GraphGateKind::CommandGreen => command.verdict == EvidenceVerdict::Green,
                GraphGateKind::AllOfN { n } if node_spec.verify_slots.is_empty() => {
                    command.verdict == EvidenceVerdict::Green && effective_green >= *n
                }
                GraphGateKind::AllOfN { .. } => node_status.evidence_slots.iter().all(|slot| {
                    if command.slot.as_deref() == Some(slot.id.as_str()) {
                        command.verdict == EvidenceVerdict::Green
                    } else {
                        slot.verdict == Some(EvidenceVerdict::Green)
                    }
                }),
                GraphGateKind::HumanConfirm => false,
            };
            if satisfied {
                payloads.push(EventPayload::GraphGateSatisfied(GraphGateSatisfied {
                    graph_id: graph_id.clone(),
                    node: current_node.clone(),
                    attempt,
                }));
                let graph_followups =
                    dependency_followups(&reduction, &status, &current_node, attempt)?;
                let child_completed = graph_followups.iter().any(|payload| {
                    matches!(
                        payload,
                        EventPayload::GraphCompleted(completed) if completed.graph_id == graph_id
                    )
                });
                payloads.extend(graph_followups);
                if child_completed {
                    payloads.extend(todo_child_completed_followups(&reductions, &graph_id)?);
                }
            } else if evidence_count >= graph_evidence_limit(node_spec)? {
                let declared_target = node_spec.red_target.as_ref();
                let target = declared_target
                    .cloned()
                    .unwrap_or_else(|| status.start_node.clone().unwrap_or_else(build_node));
                let target_spec = reduction
                    .template_nodes
                    .iter()
                    .find(|spec| spec.name == target)
                    .ok_or_else(|| {
                        store_error(
                            ErrorCode::StoreCorrupt,
                            format!("graph red target {target} is absent from its pinned template"),
                            false,
                        )
                    })?;
                let target_status = status
                    .nodes
                    .iter()
                    .find(|node| node.node == target)
                    .ok_or_else(|| {
                        store_error(
                            ErrorCode::StoreCorrupt,
                            format!("graph red target {target} has no reduced state"),
                            false,
                        )
                    })?;
                let source_attempts_exhausted =
                    node_status.attempts_opened >= node_spec.max_attempts;
                let target_attempts_exhausted = declared_target.is_some()
                    && target_status.attempts_opened >= target_spec.max_attempts;
                let conditional_hops_exhausted = declared_target.is_some()
                    && status.attempt.saturating_sub(1) >= GRAPH_MAX_CONDITIONAL_HOPS;
                if source_attempts_exhausted
                    || target_attempts_exhausted
                    || conditional_hops_exhausted
                {
                    payloads.push(EventPayload::GraphBlocked(GraphBlocked {
                        graph_id: graph_id.clone(),
                        node: current_node.clone(),
                        reason: GraphBlockReason::RoundsExhausted,
                    }));
                } else {
                    let menus = if declared_target.is_some() {
                        graph_retry_menus(&status, &reduction.template_nodes, &target)
                    } else {
                        graph_pending_menus(&status)
                    };
                    for menu in menus
                        .into_iter()
                        .filter(|menu| !pending_finalization_menus.contains(menu))
                    {
                        payloads.push(EventPayload::MenuClosed {
                            menu,
                            reason: MenuCloseReason::Dismissed,
                        });
                    }
                    let next_epoch = status.attempt.checked_add(1).ok_or_else(|| {
                        store_error(
                            ErrorCode::StoreCorrupt,
                            "graph traversal epoch space is exhausted",
                            false,
                        )
                    })?;
                    payloads.push(EventPayload::GraphAttemptOpened(GraphAttemptOpened {
                        graph_id: graph_id.clone(),
                        node: target.clone(),
                        attempt: next_epoch,
                    }));
                    if matches!(target_spec.gate, GraphGateKind::HumanConfirm) {
                        payloads.push(EventPayload::MenuOpened(graph_confirm_menu(
                            &status, &target, next_epoch,
                        )));
                    }
                }
            }
        }

        let now = now_ms()?;
        claim_pending_receipt(
            &transaction,
            &command.command_id,
            "graph.evidence",
            &command.request_digest,
            &command.request_json,
            now,
        )?;
        let mut envelopes = graph_command_envelopes(command, payloads)?;
        augment_workflow_graph_envelopes(
            &transaction,
            &self.cas,
            &command.session_id,
            &mut envelopes,
        )?;
        append_transaction_envelopes(&transaction, &command.session_id, now, &mut envelopes)?;
        let recorded = RecordedGraphEvidence {
            graph_id,
            node: current_node,
            attempt,
            fingerprint,
            evidence_seq: envelopes[0].seq,
            through_seq: envelopes.last().map_or(0, |envelope| envelope.seq),
            worker_generation: self.worker_generation,
        };
        finalize_command_receipt(
            &transaction,
            &command.command_id,
            command.session_id.as_str(),
            Some(command.run_id.as_str()),
            Some(recorded.evidence_seq),
            &recorded,
            now,
            "graph-evidence",
        )?;
        transaction.commit().map_err(map_sqlite_error)?;
        self.extend_graph_reduction(&connection, &command.session_id, &envelopes);
        Ok(GraphEvidenceOutcome::Committed {
            recorded,
            envelopes,
        })
    }

    /// Inserts the durable delegation link exactly once. Replays with the
    /// same opaque parent/run/call coordinates return the original row;
    /// altered semantics are rejected rather than creating a sibling.
    pub fn create_delegation(
        &self,
        record: &DelegationRecord,
    ) -> StoreResult<DelegationCreateOutcome> {
        validate_delegation(record)?;
        let manifest_json = serde_json::to_string(&record.manifest).map_err(|error| {
            store_error(
                ErrorCode::InvalidArgument,
                format!("cannot serialize delegation manifest: {error}"),
                false,
            )
        })?;
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        if let Some(existing) = lookup_delegation_by_agent(&transaction, &record.agent_id)? {
            require_same_delegation_identity(&existing, record)?;
            transaction.commit().map_err(map_sqlite_error)?;
            return Ok(DelegationCreateOutcome::IdempotentReplay(existing));
        }
        if let Some(existing) = lookup_delegation_by_parent_call(
            &transaction,
            &record.parent_session_id,
            &record.parent_run_id,
            &record.call_id,
        )? {
            require_same_delegation_identity(&existing, record)?;
            transaction.commit().map_err(map_sqlite_error)?;
            return Ok(DelegationCreateOutcome::IdempotentReplay(existing));
        }
        let live_count = live_delegation_count(&transaction)?;
        if live_count >= SUBAGENT_LIVE_LIMIT {
            let detail = format!(
                "Haider allows at most {SUBAGENT_LIVE_LIMIT} live subagents on this device; the current live count is {live_count}."
            );
            let mut error = store_error(ErrorCode::Busy, detail.clone(), true).with_presentation(
                ErrorPresentation::new(
                    "subagent-limit-reached",
                    "Subagent limit reached",
                    detail,
                    ErrorScope::Tool,
                    [ErrorAction::Retry],
                ),
            );
            error.details = Some(serde_json::json!({
                "limit": SUBAGENT_LIVE_LIMIT,
                "live_count": live_count,
            }));
            return Err(error);
        }
        let now = now_ms()?;
        transaction
            .execute(
                "INSERT INTO delegations(
                    agent_id, child_session_id, child_run_id,
                    parent_session_id, parent_run_id, parent_branch_id,
                    call_id, tool_item_id,
                    parent_agent_id, root_session_id, depth, task, prompt,
                    manifest_json, state, report_json, created_at_ms, updated_at_ms
                 ) VALUES (
                    ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                    ?13, ?14, 'spawned', NULL, ?15, ?15
                 )",
                params![
                    record.agent_id.as_str(),
                    record.child_session_id.as_str(),
                    record.child_run_id.as_str(),
                    record.parent_session_id.as_str(),
                    record.parent_run_id.as_str(),
                    record.parent_branch_id.as_ref().map(BranchId::as_str),
                    &record.call_id,
                    record.tool_item_id.as_str(),
                    record.parent_agent_id.as_ref().map(AgentId::as_str),
                    record.root_session_id.as_str(),
                    i64::from(record.depth),
                    &record.task,
                    &record.prompt,
                    manifest_json,
                    to_sqlite_integer(now)?,
                ],
            )
            .map_err(map_sqlite_error)?;
        transaction.commit().map_err(map_sqlite_error)?;
        let mut committed = record.clone();
        committed.state = DelegationState::Spawned;
        committed.report = None;
        Ok(DelegationCreateOutcome::Committed(committed))
    }

    pub fn delegation(&self, agent: &AgentId) -> StoreResult<Option<DelegationRecord>> {
        let connection = self.connection()?;
        lookup_delegation_by_agent(&connection, agent)
    }

    /// Rebuilds the current global live count from durable delegation and
    /// exact child-run truth. Exposed for diagnostics and restart laws; spawn
    /// admission invokes the same reducer inside its write transaction.
    pub fn live_delegation_count(&self) -> StoreResult<u64> {
        let connection = self.connection()?;
        live_delegation_count(&connection)
    }

    pub fn delegation_for_child_session(
        &self,
        session_id: &SessionId,
    ) -> StoreResult<Option<DelegationRecord>> {
        let connection = self.connection()?;
        lookup_delegation_by_child_session(&connection, session_id)
    }

    pub fn delegations_for_parent_run(
        &self,
        session_id: &SessionId,
        run_id: &RunId,
    ) -> StoreResult<Vec<DelegationRecord>> {
        let connection = self.connection()?;
        let mut statement = connection
            .prepare_cached(&format!(
                "{} WHERE parent_session_id = ?1 AND parent_run_id = ?2 ORDER BY created_at_ms, call_id",
                delegation_select()
            ))
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(
                params![session_id.as_str(), run_id.as_str()],
                stored_delegation,
            )
            .map_err(map_sqlite_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_sqlite_error)?;
        rows.into_iter().map(decode_delegation).collect()
    }

    /// Returns a deterministic, bounded breadth-first subtree rooted at a
    /// session. Every edge comes from the durable delegation relation, so a
    /// daemon restart observes the same terminal and live history.
    ///
    /// Direct-child queries fetch at most the remaining node budget plus one
    /// witness. At the depth boundary they fetch one edge solely to set the
    /// honest truncation marker. No response-sized read can materialize an
    /// unbounded historical fleet.
    pub fn delegation_descendants(
        &self,
        session_id: &SessionId,
        max_nodes: usize,
        max_depth: u32,
    ) -> StoreResult<DelegationDescendants> {
        let connection = self.connection()?;
        let mut pending = std::collections::VecDeque::from([(session_id.clone(), 0_u32)]);
        let mut seen_sessions = HashSet::from([session_id.clone()]);
        let mut descendants = Vec::with_capacity(max_nodes.min(512));
        let mut truncated = false;

        while let Some((parent_session_id, parent_depth)) = pending.pop_front() {
            let remaining = max_nodes.saturating_sub(descendants.len());
            let at_depth_limit = parent_depth >= max_depth;
            let query_limit = if at_depth_limit || remaining == 0 {
                1
            } else {
                remaining.saturating_add(1)
            };
            let mut children = delegations_for_parent_session_limited(
                &connection,
                &parent_session_id,
                query_limit,
            )?;
            if children.is_empty() {
                continue;
            }
            if at_depth_limit || remaining == 0 {
                truncated = true;
                break;
            }
            if children.len() > remaining {
                children.truncate(remaining);
                truncated = true;
            }
            let relative_depth = parent_depth.saturating_add(1);
            for child in children {
                if !seen_sessions.insert(child.child_session_id.clone()) {
                    return Err(corrupt(format!(
                        "delegation graph revisits child session {}",
                        child.child_session_id
                    )));
                }
                let direct_child_count =
                    delegation_count_for_parent_session(&connection, &child.child_session_id)?;
                pending.push_back((child.child_session_id.clone(), relative_depth));
                descendants.push(DelegationDescendant {
                    record: child,
                    relative_depth,
                    direct_child_count,
                });
            }
            if truncated {
                break;
            }
        }

        Ok(DelegationDescendants {
            descendants,
            truncated,
        })
    }

    pub fn mark_delegation_running(&self, agent: &AgentId) -> StoreResult<DelegationRecord> {
        self.update_delegation(agent, DelegationState::Running, None)
    }

    pub fn record_delegation_report(
        &self,
        agent: &AgentId,
        report: &ChildReport,
    ) -> StoreResult<DelegationRecord> {
        if report.agent != *agent {
            return Err(store_error(
                ErrorCode::InvalidArgument,
                "child report agent does not match delegation",
                false,
            ));
        }
        self.update_delegation(agent, DelegationState::Reported, Some(report))
    }

    pub fn mark_delegation_collected(&self, agent: &AgentId) -> StoreResult<DelegationRecord> {
        self.update_delegation(agent, DelegationState::Collected, None)
    }

    fn update_delegation(
        &self,
        agent: &AgentId,
        target: DelegationState,
        report: Option<&ChildReport>,
    ) -> StoreResult<DelegationRecord> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        let existing = lookup_delegation_by_agent(&transaction, agent)?.ok_or_else(|| {
            store_error(
                ErrorCode::SessionNotFound,
                "delegation was not found",
                false,
            )
        })?;
        if existing.state == DelegationState::Collected
            && matches!(
                target,
                DelegationState::Reported | DelegationState::Collected
            )
        {
            if let (Some(requested), Some(committed)) = (report, existing.report.as_ref())
                && requested != committed
            {
                return Err(store_error(
                    ErrorCode::StoreCorrupt,
                    "delegation already carries a different terminal report",
                    false,
                ));
            }
            transaction.commit().map_err(map_sqlite_error)?;
            return Ok(existing);
        }
        if target == DelegationState::Running
            && matches!(
                existing.state,
                DelegationState::Reported | DelegationState::Collected
            )
        {
            transaction.commit().map_err(map_sqlite_error)?;
            return Ok(existing);
        }
        let report_json = match (report, existing.report.as_ref()) {
            (Some(report), Some(committed)) if report != committed => {
                return Err(store_error(
                    ErrorCode::StoreCorrupt,
                    "delegation already carries a different terminal report",
                    false,
                ));
            }
            (Some(report), _) => Some(serde_json::to_string(report).map_err(|error| {
                store_error(
                    ErrorCode::InvalidArgument,
                    format!("cannot serialize child report: {error}"),
                    false,
                )
            })?),
            (None, Some(committed)) => Some(serde_json::to_string(committed).map_err(|error| {
                store_error(
                    ErrorCode::Internal,
                    format!("cannot preserve child report: {error}"),
                    false,
                )
            })?),
            (None, None) => None,
        };
        if target == DelegationState::Collected && report_json.is_none() {
            return Err(store_error(
                ErrorCode::InvalidArgument,
                "delegation cannot be collected before it reports",
                false,
            ));
        }
        transaction
            .execute(
                "UPDATE delegations SET state = ?2, report_json = ?3, updated_at_ms = ?4
                 WHERE agent_id = ?1",
                params![
                    agent.as_str(),
                    delegation_state_name(target),
                    report_json,
                    to_sqlite_integer(now_ms()?)?,
                ],
            )
            .map_err(map_sqlite_error)?;
        let updated = lookup_delegation_by_agent(&transaction, agent)?.ok_or_else(|| {
            store_error(
                ErrorCode::StoreCorrupt,
                "updated delegation vanished",
                false,
            )
        })?;
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(updated)
    }

    /// Atomically appends a live worker batch after validating it against the
    /// transaction's durable run heads.
    ///
    /// This is the ONE authoritative live-worker transition site. Once a run
    /// is terminal no later run-scoped worker event may commit, and a durable
    /// `Cancelling` may transition only to `Cancelled`, except that a durable
    /// headless budget cause may close through the matching typed budget-error
    /// batch. The budget cause wins because it was journaled before the stop.
    pub fn append_worker(&self, envelopes: &mut [RawEnvelope]) -> StoreResult<CommittedSeqRange> {
        append_envelopes(self, envelopes, true)
    }

    /// Commits already-queued actor appends under one outer SQLite transaction.
    ///
    /// A savepoint isolates each logical request, preserving the pre-batching
    /// behavior where a semantically invalid append does not reject valid
    /// requests queued before or after it. Results become observable only after
    /// the outer transaction commits. Fatal transaction failures remain the
    /// outer `Err`; request-local failures occupy their matching nested result,
    /// including when the group contains one request.
    ///
    /// This method does not wait for more work. A singleton uses the ordinary
    /// immediate append path. Receipt-backed mutations remain on their
    /// method-specific transactions so their receipt and accepted envelopes
    /// keep one indivisible durability boundary.
    pub fn append_group(
        &self,
        batches: &mut [JournalAppendBatch],
    ) -> StoreResult<Vec<StoreResult<CommittedSeqRange>>> {
        if batches.is_empty() {
            return Err(store_error(
                ErrorCode::InvalidArgument,
                "cannot commit an empty append group",
                false,
            ));
        }
        if let [batch] = batches {
            return match append_envelopes(
                self,
                &mut batch.envelopes,
                batch.validate_worker_transitions,
            ) {
                Ok(range) => Ok(vec![Ok(range)]),
                Err(error) if isolatable_append_error(&error) => Ok(vec![Err(error)]),
                Err(error) => Err(error),
            };
        }

        let mut connection = self.connection()?;
        let mut transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        let mut outcomes = Vec::with_capacity(batches.len());
        for batch in batches.iter() {
            let savepoint = transaction.savepoint().map_err(map_sqlite_error)?;
            match append_envelopes_in_transaction(
                &savepoint,
                &batch.envelopes,
                batch.validate_worker_transitions,
            ) {
                Ok(outcome) => {
                    savepoint.commit().map_err(map_sqlite_error)?;
                    outcomes.push(Ok(outcome));
                }
                Err(error) if isolatable_append_error(&error) => {
                    savepoint.finish().map_err(map_sqlite_error)?;
                    outcomes.push(Err(error));
                }
                Err(error) => return Err(error),
            }
        }
        transaction.commit().map_err(map_sqlite_error)?;

        let mut results = Vec::with_capacity(batches.len());
        for (batch, outcome) in batches.iter_mut().zip(outcomes) {
            match outcome {
                Ok(outcome) => {
                    batch.envelopes = outcome.stamped;
                    update_append_caches(
                        self,
                        &connection,
                        &outcome.range.session_id,
                        &batch.envelopes,
                        outcome.changes_graph_reduction,
                        outcome.changes_graph_telemetry,
                    );
                    results.push(Ok(outcome.range));
                }
                Err(error) => results.push(Err(error)),
            }
        }
        Ok(results)
    }

    /// Claims the global command-id namespace for `session.compact` before
    /// its durable intent is appended. Committed replay is intentionally
    /// unfenced so a lost response survives daemon restart.
    pub fn claim_context_compaction_receipt(
        &self,
        command_id: &str,
        request_digest: &str,
        request_json: &str,
    ) -> StoreResult<ContextCompactionClaim> {
        validate_command_identity(command_id, request_digest, request_json)?;
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        if let Some(response) = lookup_command_response::<ContextCompactionReceiptResponse>(
            &transaction,
            command_id,
            "session.compact",
            request_digest,
            request_json,
            "session-compact",
        )? {
            transaction.commit().map_err(map_sqlite_error)?;
            return Ok(ContextCompactionClaim::Committed(Box::new(response)));
        }
        let pending = transaction
            .query_row(
                "SELECT 1 FROM command_receipts WHERE command_id = ?1",
                [command_id],
                |_| Ok(()),
            )
            .optional()
            .map_err(map_sqlite_error)?
            .is_some();
        claim_pending_receipt(
            &transaction,
            command_id,
            "session.compact",
            request_digest,
            request_json,
            now_ms()?,
        )?;
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(if pending {
            ContextCompactionClaim::ResumePending
        } else {
            ContextCompactionClaim::Fresh
        })
    }

    /// Finalizes the already-claimed compaction receipt. The compaction node
    /// is independently sufficient to reconcile a crash between node commit
    /// and this receipt update.
    pub fn finalize_context_compaction_receipt(
        &self,
        command_id: &str,
        response: &ContextCompactionReceiptResponse,
    ) -> StoreResult<()> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        finalize_command_receipt(
            &transaction,
            command_id,
            response.session_id.as_str(),
            Some(response.run_id.as_str()),
            Some(response.accepted_seq),
            response,
            now_ms()?,
            "session-compact",
        )?;
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(())
    }

    /// Looks up a committed typed client-monitor mutation in the same global
    /// command-id namespace used by every other durable client mutation.
    pub fn monitor_control_receipt(
        &self,
        command_id: &str,
        method: &str,
        request_digest: &str,
        request_json: &str,
    ) -> StoreResult<Option<serde_json::Value>> {
        validate_monitor_control_method(method)?;
        validate_command_identity(command_id, request_digest, request_json)?;
        let connection = self.connection()?;
        lookup_command_response(
            &connection,
            command_id,
            method,
            request_digest,
            request_json,
            "monitor-control",
        )
    }

    /// Claims (or resumes) one typed client-monitor mutation. A pending row
    /// is recoverable from the atomically appended session-local monitor fact
    /// and receipt before this global response is finalized.
    pub fn claim_monitor_control_receipt(
        &self,
        command_id: &str,
        method: &str,
        request_digest: &str,
        request_json: &str,
    ) -> StoreResult<MonitorControlClaim> {
        validate_monitor_control_method(method)?;
        validate_command_identity(command_id, request_digest, request_json)?;
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        if let Some(response) = lookup_command_response::<serde_json::Value>(
            &transaction,
            command_id,
            method,
            request_digest,
            request_json,
            "monitor-control",
        )? {
            transaction.commit().map_err(map_sqlite_error)?;
            return Ok(MonitorControlClaim::Committed(response));
        }
        let pending = transaction
            .query_row(
                "SELECT 1 FROM command_receipts WHERE command_id = ?1",
                [command_id],
                |_| Ok(()),
            )
            .optional()
            .map_err(map_sqlite_error)?
            .is_some();
        claim_pending_receipt(
            &transaction,
            command_id,
            method,
            request_digest,
            request_json,
            now_ms()?,
        )?;
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(if pending {
            MonitorControlClaim::ResumePending
        } else {
            MonitorControlClaim::Fresh
        })
    }

    /// Finalizes a monitor mutation after its session-local fact and recovery
    /// receipt have committed atomically through the session actor.
    pub fn finalize_monitor_control_receipt(
        &self,
        command_id: &str,
        session_id: &SessionId,
        accepted_seq: u64,
        response: &serde_json::Value,
    ) -> StoreResult<()> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        finalize_command_receipt(
            &transaction,
            command_id,
            session_id.as_str(),
            None,
            Some(accepted_seq),
            response,
            now_ms()?,
            "monitor-control",
        )?;
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(())
    }

    /// Looks up a committed `session.create` response before filesystem
    /// validation. This ordering is intentional: after a successful create,
    /// a lost-response retry remains recoverable even if the workspace path
    /// was subsequently removed.
    ///
    /// RECEIPT IDEMPOTENCY (R2, authoritative statement for all three
    /// receipt lookups): the durable key is the client's semantic
    /// `command_id` — never a transport request id. Same `command_id` +
    /// same method/digest returns the original committed response, however
    /// many times it is retried and across daemon restarts (this lookup is
    /// deliberately NOT generation-fenced). Same `command_id` with a
    /// different method or semantic body is `invalid_argument`. The wire
    /// layer MUST consult the unfenced lookup BEFORE the fenced command
    /// transaction — see `accept_turn` for why.
    pub fn session_create_receipt(
        &self,
        command_id: &str,
        request_digest: &str,
        request_json: &str,
    ) -> StoreResult<Option<CreatedSession>> {
        validate_command_identity(command_id, request_digest, request_json)?;
        let connection = self.connection()?;
        lookup_session_create_receipt(&connection, command_id, request_digest, request_json)
    }

    /// Atomically claims/finalizes a command receipt, inserts typed metadata,
    /// and commits `SessionState::Created` at sequence one.
    pub fn create_session(
        &self,
        command: &SessionCreateCommand,
    ) -> StoreResult<SessionCreateOutcome> {
        self.create_session_with_interaction_mode(
            command,
            haider_protocol::session::SessionInteractionModeV1::Interactive,
        )
    }

    /// Same atomic session creation transaction with an explicit durable
    /// human-availability contract. Legacy callers remain interactive through
    /// [`Self::create_session`].
    pub fn create_session_with_interaction_mode(
        &self,
        command: &SessionCreateCommand,
        interaction_mode: haider_protocol::session::SessionInteractionModeV1,
    ) -> StoreResult<SessionCreateOutcome> {
        validate_command_identity(
            &command.command_id,
            &command.request_digest,
            &command.request_json,
        )?;
        if command.cwd.is_empty()
            || command.provider.is_empty()
            || command.model.is_empty()
            || command.max_tokens == 0
        {
            return Err(store_error(
                ErrorCode::InvalidArgument,
                "session metadata fields must be non-empty and max_tokens must be positive",
                false,
            ));
        }
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        if let Some(created) = lookup_session_create_receipt(
            &transaction,
            &command.command_id,
            &command.request_digest,
            &command.request_json,
        )? {
            transaction.commit().map_err(map_sqlite_error)?;
            return Ok(SessionCreateOutcome::IdempotentReplay { created });
        }

        let created_at_ms = now_ms()?;
        let created_at_sql = to_sqlite_integer(created_at_ms)?;
        claim_pending_receipt(
            &transaction,
            &command.command_id,
            "session.create",
            &command.request_digest,
            &command.request_json,
            created_at_ms,
        )?;

        let metadata = SessionMetadataV1 {
            cwd: command.cwd.clone(),
            provider: command.provider.clone(),
            model: command.model.clone(),
            max_tokens: command.max_tokens,
            system_prompt_version: Some(command.system_prompt_version.clone()),
            permission_overrides: command.permission_overrides,
            interaction_mode,
            // G2: sessions are born untitled; the daemon-side auto-title
            // (first accept) or an explicit `session.rename` fills this.
            title: None,
            // G3 tuning: the wire `session.create` passes the defaults;
            // delegation passes the parent's CURRENT tuning so children
            // inherit effort/fast through the metadata clone (LE6).
            effort: command.effort.clone(),
            fast: command.fast,
            cache_policy: command.cache_policy,
            // W-flow: sessions are born plain; `session.select_agent_type`
            // binds a Loom identity later.
            agent_type: None,
            created_at_ms,
        };
        let metadata_json = serde_json::to_string(&metadata).map_err(|error| {
            store_error(
                ErrorCode::InvalidArgument,
                format!("cannot serialize session metadata: {error}"),
                false,
            )
        })?;
        transaction
            .execute(
                "INSERT INTO sessions(id, created_at_ms, meta_json) VALUES (?1, ?2, ?3)",
                params![command.session_id.as_str(), created_at_sql, metadata_json,],
            )
            .map_err(map_sqlite_error)?;

        let payload = serde_json::to_value(EventPayload::SessionState(SessionState::Created))
            .map_err(|error| {
                store_error(
                    ErrorCode::InvalidArgument,
                    format!("cannot serialize session-created payload: {error}"),
                    false,
                )
            })?;
        let envelope = EventEnvelope {
            schema_version: SCHEMA_VERSION,
            event_id: command.event_id.clone(),
            seq: 1,
            session_id: command.session_id.clone(),
            branch_id: None,
            run_id: None,
            agent_id: None,
            device_id: command.device_id.clone(),
            authority_epoch: 0,
            worker_generation: self.worker_generation,
            causation_id: None,
            correlation_id: None,
            committed_at_ms: created_at_ms,
            render: RenderTargets {
                ui: true,
                durable: true,
                prompt: PromptRender::Omit,
            },
            payload,
        };
        let envelope_bytes = encode_envelope(&envelope).map_err(|error| {
            store_error(
                ErrorCode::InvalidArgument,
                format!("cannot serialize session-created envelope: {error}"),
                false,
            )
        })?;
        transaction
            .execute(
                "INSERT INTO events(
                    session_id, seq, envelope_json, event_id, committed_at_ms, payload_kind
                 ) VALUES (?1, 1, ?2, ?3, ?4, ?5)",
                params![
                    command.session_id.as_str(),
                    envelope_bytes,
                    command.event_id.as_str(),
                    created_at_sql,
                    payload_kind(&envelope),
                ],
            )
            .map_err(map_sqlite_error)?;
        enqueue_hook_dispatch(&transaction, &envelope)?;
        transaction
            .execute(
                "INSERT INTO run_head_sessions(
                     session_id, through_seq, run_count, nonterminal_count
                 ) VALUES (?1, 1, 0, 0)",
                [command.session_id.as_str()],
            )
            .map_err(map_sqlite_error)?;

        let created = CreatedSession {
            session_id: command.session_id.clone(),
            created_seq: 1,
            worker_generation: self.worker_generation,
            metadata,
        };
        let response_json = serde_json::to_string(&created).map_err(|error| {
            store_error(
                ErrorCode::InvalidArgument,
                format!("cannot serialize session-create response: {error}"),
                false,
            )
        })?;
        let updated = transaction
            .execute(
                "UPDATE command_receipts
                 SET state = 'committed', session_id = ?2, accepted_seq = 1,
                     response_json = ?3, updated_at_ms = ?4
                 WHERE command_id = ?1 AND state = 'pending'",
                params![
                    &command.command_id,
                    command.session_id.as_str(),
                    response_json,
                    created_at_sql,
                ],
            )
            .map_err(map_sqlite_error)?;
        if updated != 1 {
            return Err(corrupt(
                "session-create command receipt was not pending at finalization",
            ));
        }
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(SessionCreateOutcome::Committed {
            created,
            envelope: Box::new(envelope),
        })
    }

    /// Looks up a committed `session.fork` response before source/generation
    /// validation so response-loss replay remains recoverable after restart.
    pub fn session_fork_receipt(
        &self,
        command_id: &str,
        request_digest: &str,
        request_json: &str,
    ) -> StoreResult<Option<CreatedSessionFork>> {
        validate_command_identity(command_id, request_digest, request_json)?;
        let connection = self.connection()?;
        lookup_command_response(
            &connection,
            command_id,
            "session.fork",
            request_digest,
            request_json,
            "session-fork",
        )
    }

    /// Looks up a committed `session.metafork` response before mutable source
    /// validation. Proposal-only review never creates a receipt.
    pub fn session_metafork_receipt(
        &self,
        command_id: &str,
        request_digest: &str,
        request_json: &str,
    ) -> StoreResult<Option<CreatedSessionFork>> {
        validate_command_identity(command_id, request_digest, request_json)?;
        let connection = self.connection()?;
        lookup_command_response(
            &connection,
            command_id,
            "session.metafork",
            request_digest,
            request_json,
            "session-metafork",
        )
    }

    /// Read-only validation for the exact source coordinate displayed by a
    /// metafork review. Delegated child sessions validate their owning agent
    /// lane; ordinary sessions validate the root lane.
    pub fn validate_session_fork_source(
        &self,
        worker_generation: u64,
        source_session_id: &SessionId,
        source_branch_id: Option<&BranchId>,
        fork_node_id: &NodeId,
        fork_seq: u64,
    ) -> StoreResult<()> {
        if worker_generation != self.worker_generation {
            return Err(stale_generation(worker_generation, self.worker_generation));
        }
        let connection = self.connection()?;
        require_typed_session(&connection, source_session_id)?;
        let owner_agent = lookup_delegation_by_child_session(&connection, source_session_id)?
            .map(|delegation| delegation.agent_id);
        validate_branch_fork(
            &connection,
            source_session_id,
            source_branch_id,
            fork_node_id,
            fork_seq,
            owner_agent.as_ref(),
        )
    }

    /// Atomically creates an independent child session, copies exactly the
    /// admitted source lineage through the requested node, appends an audit
    /// fact, and finalizes the command receipt. Source rows are read-only.
    pub fn fork_session(&self, command: &SessionForkCommand) -> StoreResult<SessionForkOutcome> {
        self.fork_session_with_cache_candidate(command, None)
    }

    /// Same atomic fork with a provider-rendered child view eligible for
    /// fail-closed exact-prefix cache inheritance. Missing, malformed, stale,
    /// or byte-divergent candidates retain the ordinary fresh epoch.
    pub fn fork_session_with_cache_candidate(
        &self,
        command: &SessionForkCommand,
        cache_candidate: Option<&ForkCacheInheritanceCandidate>,
    ) -> StoreResult<SessionForkOutcome> {
        validate_command_identity(
            &command.command_id,
            &command.request_digest,
            &command.request_json,
        )?;
        if command.worker_generation != self.worker_generation {
            return Err(stale_generation(
                command.worker_generation,
                self.worker_generation,
            ));
        }
        if command.source_session_id == command.session_id
            || command.session_id.as_str().is_empty()
            || command.fork_node_id.as_str().is_empty()
            || command.fork_seq == 0
            || command.name.as_ref().is_some_and(|title| {
                title.trim().is_empty()
                    || title.chars().count() > 80
                    || title.chars().any(char::is_control)
            })
        {
            return Err(store_error(
                ErrorCode::InvalidArgument,
                "session fork ids, fork coordinate, and optional name must be valid",
                false,
            ));
        }
        let (method, description, model_proposal, proposal_digest, mode) =
            if let Some(metafork) = &command.metafork {
                validate_metafork_commit(command, metafork)?;
                (
                    "session.metafork",
                    Some(metafork.description.clone()),
                    Some(metafork.model_proposal.clone()),
                    Some(metafork.accepted_proposal_digest.clone()),
                    SessionForkMode::Metafork,
                )
            } else {
                ("session.fork", None, None, None, SessionForkMode::Fork)
            };

        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        if let Some(created) = lookup_command_response(
            &transaction,
            &command.command_id,
            method,
            &command.request_digest,
            &command.request_json,
            if command.metafork.is_some() {
                "session-metafork"
            } else {
                "session-fork"
            },
        )? {
            transaction.commit().map_err(map_sqlite_error)?;
            return Ok(SessionForkOutcome::IdempotentReplay { created });
        }

        require_typed_session(&transaction, &command.source_session_id)?;
        if transaction
            .query_row(
                "SELECT 1 FROM sessions WHERE id = ?1",
                [command.session_id.as_str()],
                |_| Ok(()),
            )
            .optional()
            .map_err(map_sqlite_error)?
            .is_some()
        {
            return Err(store_error(
                ErrorCode::InvalidArgument,
                "daemon-minted child session id already exists",
                false,
            ));
        }
        let source_owner_agent =
            lookup_delegation_by_child_session(&transaction, &command.source_session_id)?
                .map(|delegation| delegation.agent_id);
        validate_branch_fork(
            &transaction,
            &command.source_session_id,
            command.source_branch_id.as_ref(),
            &command.fork_node_id,
            command.fork_seq,
            source_owner_agent.as_ref(),
        )?;

        let now = now_ms()?;
        claim_pending_receipt(
            &transaction,
            &command.command_id,
            method,
            &command.request_digest,
            &command.request_json,
            now,
        )?;
        let source_metadata_json: String = transaction
            .query_row(
                "SELECT meta_json FROM sessions WHERE id = ?1",
                [command.source_session_id.as_str()],
                |row| row.get(0),
            )
            .map_err(map_sqlite_error)?;
        let mut metadata =
            decode_session_metadata(&command.source_session_id, &source_metadata_json)?
                .ok_or_else(|| corrupt("typed source session lost its metadata"))?;
        metadata.created_at_ms = now;
        if let Some(name) = &command.name {
            metadata.title = Some(name.clone());
        }
        let metadata_json = serde_json::to_string(&metadata).map_err(|error| {
            store_error(
                ErrorCode::InvalidArgument,
                format!("cannot serialize forked session metadata: {error}"),
                false,
            )
        })?;
        transaction
            .execute(
                "INSERT INTO sessions(id, created_at_ms, meta_json) VALUES (?1, ?2, ?3)",
                params![
                    command.session_id.as_str(),
                    to_sqlite_integer(now)?,
                    metadata_json
                ],
            )
            .map_err(map_sqlite_error)?;

        let scopes = branch_lineage_scopes(
            &transaction,
            &command.source_session_id,
            command.source_branch_id.as_ref(),
        )?;
        let source_envelopes = load_fork_source_envelopes(
            &transaction,
            &command.source_session_id,
            command.fork_seq,
            &scopes,
        )?;
        if source_envelopes.is_empty() || source_envelopes[0].seq != 1 {
            return Err(corrupt(
                "fork source lineage does not contain its created envelope",
            ));
        }
        let inherited_cache_segment = if model_proposal.is_none() {
            if let Some(candidate) = cache_candidate {
                let source_cache_boundary =
                    source_fork_cache_boundary(&transaction, &command.source_session_id)?;
                inherited_fork_cache_segment(
                    &source_envelopes,
                    source_owner_agent.as_ref(),
                    &metadata,
                    &command.source_session_id,
                    source_cache_boundary.as_ref(),
                    candidate,
                )
            } else {
                None
            }
        } else {
            // A valid metafork changes at least one prompt-visible source
            // envelope, so its copied history cannot equal the parent view.
            None
        };

        let event_ids = source_envelopes
            .iter()
            .map(|envelope| {
                (
                    envelope.event_id.clone(),
                    remapped_fork_event_id(&command.session_id, &envelope.event_id),
                )
            })
            .collect::<HashMap<_, _>>();
        let mut matched_removals = model_proposal
            .as_ref()
            .map(|proposal| vec![false; proposal.removals.len()])
            .unwrap_or_default();
        let mut omissions = Vec::new();
        let mut child_envelopes = Vec::with_capacity(source_envelopes.len() + 4);
        for (index, source) in source_envelopes.iter().enumerate() {
            let child_seq = u64::try_from(index)
                .map_err(|_| corrupt("forked journal is too large"))?
                .checked_add(1)
                .ok_or_else(|| corrupt("forked journal sequence space is exhausted"))?;
            let child_event_id = event_ids
                .get(&source.event_id)
                .cloned()
                .ok_or_else(|| corrupt("fork event-id remap is incomplete"))?;
            let mut child = source.clone();
            child.event_id = child_event_id.clone();
            child.seq = child_seq;
            child.session_id = command.session_id.clone();
            // A session fork materializes the selected lineage as the child's
            // ordinary main history; source named refs remain parent-owned.
            child.branch_id = None;
            if child.agent_id.as_ref() == source_owner_agent.as_ref() {
                // A delegated source's owning lane becomes the independent
                // child's ordinary root lane. Other lanes remain attributed.
                child.agent_id = None;
            }
            child.device_id = command.device_id.clone();
            child.authority_epoch = 0;
            child.worker_generation = self.worker_generation;
            child.causation_id = source
                .causation_id
                .as_ref()
                .and_then(|event_id| event_ids.get(event_id).cloned());
            child.correlation_id = source
                .correlation_id
                .as_ref()
                .and_then(|event_id| event_ids.get(event_id).cloned());

            if let Some(proposal) = &model_proposal {
                for (removal_index, removal) in proposal.removals.iter().enumerate() {
                    if source.seq >= removal.from_seq
                        && source.seq <= removal.through_seq
                        && source.render.prompt != PromptRender::Omit
                        && child.agent_id.is_none()
                    {
                        matched_removals[removal_index] = true;
                        child.render.prompt = PromptRender::Omit;
                        omissions.push(SessionHistoryOmission {
                            source_seq: source.seq,
                            child_seq,
                            source_event_id: source.event_id.clone(),
                            child_event_id: child_event_id.clone(),
                            payload_kind: payload_kind(source).to_owned(),
                            reason: removal.reason.clone(),
                        });
                        break;
                    }
                }
            }
            insert_forked_envelope(&transaction, &child)?;
            child_envelopes.push(child);
        }
        if matched_removals.iter().any(|matched| !matched) {
            return Err(store_error(
                ErrorCode::InvalidArgument,
                "metafork proposal contains a range outside the copied source lineage",
                false,
            ));
        }

        // A copied source run is historical authority, never live child work.
        // Close any run whose terminal fact lies after the fork coordinate so
        // startup recovery cannot resume it in the independent child.
        append_fork_boundary_closures(&transaction, command, now, &mut child_envelopes)?;

        let audit_seq = u64::try_from(child_envelopes.len())
            .map_err(|_| corrupt("forked journal is too large"))?
            .checked_add(1)
            .ok_or_else(|| corrupt("forked journal sequence space is exhausted"))?;
        let audit = EventEnvelope {
            schema_version: SCHEMA_VERSION,
            event_id: command.audit_event_id.clone(),
            seq: audit_seq,
            session_id: command.session_id.clone(),
            branch_id: None,
            run_id: None,
            agent_id: None,
            device_id: command.device_id.clone(),
            authority_epoch: 0,
            worker_generation: self.worker_generation,
            causation_id: None,
            correlation_id: None,
            committed_at_ms: now,
            render: RenderTargets {
                ui: true,
                durable: true,
                prompt: PromptRender::Omit,
            },
            payload: SessionForked {
                source_session_id: command.source_session_id.clone(),
                source_branch_id: command.source_branch_id.clone(),
                fork_node_id: command.fork_node_id.clone(),
                fork_seq: command.fork_seq,
                mode,
                description: description.clone(),
                accepted_proposal_digest: proposal_digest.clone(),
                omissions: omissions.clone(),
                context_epoch: if inherited_cache_segment.is_some() {
                    ForkContextEpoch::Inherited
                } else {
                    ForkContextEpoch::Fresh
                },
                inherited_cache_segment: inherited_cache_segment.clone(),
            }
            .to_payload_value()
            .map_err(|error| {
                store_error(
                    ErrorCode::InvalidArgument,
                    format!("cannot serialize session-fork audit fact: {error}"),
                    false,
                )
            })?,
        };
        insert_forked_envelope(&transaction, &audit)?;
        enqueue_hook_dispatch(&transaction, &audit)?;
        child_envelopes.push(audit);

        let created = CreatedSessionFork {
            session_id: command.session_id.clone(),
            source_session_id: command.source_session_id.clone(),
            source_branch_id: command.source_branch_id.clone(),
            fork_node_id: command.fork_node_id.clone(),
            fork_seq: command.fork_seq,
            created_seq: audit_seq,
            worker_generation: self.worker_generation,
            metadata,
            mode,
            description,
            model_proposal,
            proposal_digest,
            omission_count: u64::try_from(omissions.len()).unwrap_or(u64::MAX),
            inherited_cache_segment,
        };
        finalize_command_receipt(
            &transaction,
            &command.command_id,
            command.session_id.as_str(),
            None,
            Some(audit_seq),
            &created,
            now,
            if command.metafork.is_some() {
                "session-metafork"
            } else {
                "session-fork"
            },
        )?;
        rebuild_run_head_projection(&transaction, &command.session_id)?;
        update_workflow_graph_projection_after_append(
            &transaction,
            &command.session_id,
            &child_envelopes,
        )?;
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(SessionForkOutcome::Committed {
            created,
            envelopes: child_envelopes,
        })
    }

    /// Looks up a committed `branch.create` response before mutable branch,
    /// generation, or attachment validation (R2 response-loss replay).
    pub fn branch_create_receipt(
        &self,
        command_id: &str,
        request_digest: &str,
        request_json: &str,
    ) -> StoreResult<Option<CreatedBranch>> {
        validate_command_identity(command_id, request_digest, request_json)?;
        let connection = self.connection()?;
        lookup_command_response(
            &connection,
            command_id,
            "branch.create",
            request_digest,
            request_json,
            "branch-create",
        )
    }

    /// Atomically inserts a named ref, appends its topology fact, and
    /// finalizes the command receipt. Any late failure rolls all three back.
    pub fn create_branch(&self, command: &BranchCreateCommand) -> StoreResult<BranchCreateOutcome> {
        validate_command_identity(
            &command.command_id,
            &command.request_digest,
            &command.request_json,
        )?;
        if command.worker_generation != self.worker_generation {
            return Err(stale_generation(
                command.worker_generation,
                self.worker_generation,
            ));
        }
        if command.branch_id.as_str().is_empty()
            || command.fork_node_id.as_str().is_empty()
            || command.fork_seq == 0
            || command
                .source_branch_id
                .as_ref()
                .is_some_and(|branch| branch.as_str().is_empty())
            || command
                .name
                .as_ref()
                .is_some_and(|name| name.trim().is_empty())
        {
            return Err(store_error(
                ErrorCode::InvalidArgument,
                "branch id, fork node/sequence, source branch, and optional name must be valid",
                false,
            ));
        }

        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        if let Some(created) = lookup_command_response(
            &transaction,
            &command.command_id,
            "branch.create",
            &command.request_digest,
            &command.request_json,
            "branch-create",
        )? {
            transaction.commit().map_err(map_sqlite_error)?;
            return Ok(BranchCreateOutcome::IdempotentReplay { created });
        }
        require_typed_session(&transaction, &command.session_id)?;
        if branch_descriptor(&transaction, &command.session_id, &command.branch_id)?.is_some() {
            return Err(store_error(
                ErrorCode::InvalidArgument,
                "daemon-minted branch id already exists",
                false,
            ));
        }
        validate_branch_fork(
            &transaction,
            &command.session_id,
            command.source_branch_id.as_ref(),
            &command.fork_node_id,
            command.fork_seq,
            None,
        )?;

        let now = now_ms()?;
        claim_pending_receipt(
            &transaction,
            &command.command_id,
            "branch.create",
            &command.request_digest,
            &command.request_json,
            now,
        )?;
        let latest: i64 = transaction
            .query_row(
                "SELECT COALESCE(MAX(seq), 0) FROM events WHERE session_id = ?1",
                [command.session_id.as_str()],
                |row| row.get(0),
            )
            .map_err(map_sqlite_error)?;
        let created_seq = u64::try_from(latest)
            .map_err(|_| corrupt("database contains a negative event sequence"))?
            .checked_add(1)
            .ok_or_else(|| corrupt("event sequence space is exhausted"))?;
        let name = command
            .name
            .clone()
            .unwrap_or_else(|| command.branch_id.as_str().to_owned());
        let descriptor = BranchDescriptor {
            branch_id: command.branch_id.clone(),
            name: name.clone(),
            source_branch_id: command.source_branch_id.clone(),
            fork_node_id: command.fork_node_id.clone(),
            fork_seq: command.fork_seq,
            created_seq,
            created_at_ms: now,
            head_node_id: command.fork_node_id.clone(),
            head_seq: command.fork_seq,
        };
        let mut envelopes = vec![unstamped_raw_command_envelope(
            command.event_id.clone(),
            &command.session_id,
            None,
            None,
            command.device_id.clone(),
            self.worker_generation,
            BranchCreated {
                branch: descriptor.clone(),
            }
            .to_payload_value()
            .map_err(|error| {
                store_error(
                    ErrorCode::InvalidArgument,
                    format!("cannot serialize branch-created payload: {error}"),
                    false,
                )
            })?,
            PromptRender::Omit,
        )?];
        append_transaction_envelopes(&transaction, &command.session_id, now, &mut envelopes)?;
        if envelopes[0].seq != created_seq {
            return Err(corrupt(
                "branch-created fact sequence changed during transaction",
            ));
        }
        transaction
            .execute(
                "INSERT INTO branches(
                    session_id, branch_id, display_name, source_branch_id,
                    fork_node_id, fork_seq, created_seq, created_at_ms,
                    head_node_id, head_seq
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    command.session_id.as_str(),
                    command.branch_id.as_str(),
                    &name,
                    command.source_branch_id.as_ref().map(BranchId::as_str),
                    command.fork_node_id.as_str(),
                    to_sqlite_integer(command.fork_seq)?,
                    to_sqlite_integer(created_seq)?,
                    to_sqlite_integer(now)?,
                    command.fork_node_id.as_str(),
                    to_sqlite_integer(command.fork_seq)?,
                ],
            )
            .map_err(map_sqlite_error)?;
        let created = CreatedBranch {
            session_id: command.session_id.clone(),
            branch_id: command.branch_id.clone(),
            source_branch_id: command.source_branch_id.clone(),
            fork_node_id: command.fork_node_id.clone(),
            fork_seq: command.fork_seq,
            created_seq,
            worker_generation: self.worker_generation,
            name,
        };
        finalize_command_receipt(
            &transaction,
            &command.command_id,
            command.session_id.as_str(),
            None,
            Some(created_seq),
            &created,
            now,
            "branch-create",
        )?;
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(BranchCreateOutcome::Committed {
            created,
            envelope: Box::new(envelopes.remove(0)),
        })
    }

    /// Returns the generation embedded in an existing receipt request.
    ///
    /// This is intentionally method-fenced and returns no request or response
    /// bytes. It lets a higher-level idempotent door reconstruct the exact
    /// canonical request generation after a daemon restart without exposing a
    /// generic receipt-inspection surface.
    pub fn command_receipt_worker_generation(
        &self,
        command_id: &str,
        expected_method: &str,
    ) -> StoreResult<Option<u64>> {
        let connection = self.connection()?;
        let row = connection
            .query_row(
                "SELECT method, request_json FROM command_receipts WHERE command_id = ?1",
                [command_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(map_sqlite_error)?;
        let Some((method, request_json)) = row else {
            return Ok(None);
        };
        if method != expected_method {
            return Err(store_error(
                ErrorCode::InvalidArgument,
                "command id was already used by a different canonical method",
                false,
            ));
        }
        let request: serde_json::Value = serde_json::from_str(&request_json).map_err(|error| {
            corrupt(format!(
                "{expected_method} receipt request JSON is invalid: {error}"
            ))
        })?;
        request
            .get("worker_generation")
            .and_then(serde_json::Value::as_u64)
            .map(Some)
            .ok_or_else(|| {
                corrupt(format!(
                    "{expected_method} receipt request has no worker generation"
                ))
            })
    }

    /// Looks up a committed `session.select_model` response before session,
    /// generation, or metadata validation (R2 response-loss replay).
    pub fn session_select_model_receipt(
        &self,
        command_id: &str,
        request_digest: &str,
        request_json: &str,
    ) -> StoreResult<Option<SelectedModel>> {
        validate_command_identity(command_id, request_digest, request_json)?;
        let connection = self.connection()?;
        lookup_command_response(
            &connection,
            command_id,
            "session.select_model",
            request_digest,
            request_json,
            "session-select-model",
        )
    }

    /// Atomically applies one RESOLVED model selection: updates the session's
    /// typed metadata (provider + model, every other field preserved),
    /// appends the `model_selected` fact, and finalizes the command receipt.
    /// Any late failure rolls all three back.
    ///
    /// The daemon owns resolution and validation; this transaction owns only
    /// durability. The next logical turn re-reads the committed metadata, so
    /// commit here IS next-turn pickup.
    pub fn select_session_model(
        &self,
        command: &SessionSelectModelCommand,
    ) -> StoreResult<SessionSelectModelOutcome> {
        validate_command_identity(
            &command.command_id,
            &command.request_digest,
            &command.request_json,
        )?;
        if command.worker_generation != self.worker_generation {
            return Err(stale_generation(
                command.worker_generation,
                self.worker_generation,
            ));
        }
        if command.provider.trim().is_empty() || command.model.trim().is_empty() {
            return Err(store_error(
                ErrorCode::InvalidArgument,
                "model selection must carry a resolved provider and model",
                false,
            ));
        }

        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        if let Some(selected) = lookup_command_response(
            &transaction,
            &command.command_id,
            "session.select_model",
            &command.request_digest,
            &command.request_json,
            "session-select-model",
        )? {
            transaction.commit().map_err(map_sqlite_error)?;
            return Ok(SessionSelectModelOutcome::IdempotentReplay { selected });
        }
        require_typed_session(&transaction, &command.session_id)?;
        let metadata_json: String = transaction
            .query_row(
                "SELECT meta_json FROM sessions WHERE id = ?1",
                [command.session_id.as_str()],
                |row| row.get(0),
            )
            .map_err(map_sqlite_error)?;
        let Some(mut metadata) = decode_session_metadata(&command.session_id, &metadata_json)?
        else {
            return Err(store_error(
                ErrorCode::InvalidArgument,
                "legacy session has no live-worker metadata",
                false,
            ));
        };
        if let Some((expected_provider, expected_model)) = &command.expected_pair
            && (&metadata.provider != expected_provider || &metadata.model != expected_model)
        {
            return Err(store_error(
                ErrorCode::RevisionConflict,
                format!(
                    "automatic selection expected pair {expected_provider}/{expected_model} but                      the session moved to {}/{} — refusing to overwrite a newer explicit                      selection",
                    metadata.provider, metadata.model
                ),
                false,
            ));
        }
        metadata.provider = command.provider.clone();
        metadata.model = command.model.clone();
        let updated_metadata = serde_json::to_string(&metadata).map_err(|error| {
            store_error(
                ErrorCode::InvalidArgument,
                format!("cannot serialize session metadata: {error}"),
                false,
            )
        })?;

        let now = now_ms()?;
        claim_pending_receipt(
            &transaction,
            &command.command_id,
            "session.select_model",
            &command.request_digest,
            &command.request_json,
            now,
        )?;
        let updated_rows = transaction
            .execute(
                "UPDATE sessions SET meta_json = ?2 WHERE id = ?1",
                params![command.session_id.as_str(), updated_metadata],
            )
            .map_err(map_sqlite_error)?;
        if updated_rows != 1 {
            return Err(corrupt("session row disappeared during model selection"));
        }
        let mut envelopes = vec![unstamped_raw_command_envelope(
            command.event_id.clone(),
            &command.session_id,
            None,
            None,
            command.device_id.clone(),
            self.worker_generation,
            ModelSelected {
                provider: command.provider.clone(),
                model: command.model.clone(),
            }
            .to_payload_value()
            .map_err(|error| {
                store_error(
                    ErrorCode::InvalidArgument,
                    format!("cannot serialize model-selected payload: {error}"),
                    false,
                )
            })?,
            PromptRender::Omit,
        )?];
        append_transaction_envelopes(&transaction, &command.session_id, now, &mut envelopes)?;
        let selected = SelectedModel {
            session_id: command.session_id.clone(),
            provider: command.provider.clone(),
            model: command.model.clone(),
            selected_seq: envelopes[0].seq,
            worker_generation: self.worker_generation,
        };
        finalize_command_receipt(
            &transaction,
            &command.command_id,
            command.session_id.as_str(),
            None,
            Some(selected.selected_seq),
            &selected,
            now,
            "session-select-model",
        )?;
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(SessionSelectModelOutcome::Committed {
            selected,
            envelope: Box::new(envelopes.remove(0)),
        })
    }

    /// Looks up a committed `session.rename` response before session,
    /// generation, or metadata validation (R2 response-loss replay).
    pub fn session_rename_receipt(
        &self,
        command_id: &str,
        request_digest: &str,
        request_json: &str,
    ) -> StoreResult<Option<RenamedSession>> {
        validate_command_identity(command_id, request_digest, request_json)?;
        let connection = self.connection()?;
        lookup_command_response(
            &connection,
            command_id,
            "session.rename",
            request_digest,
            request_json,
            "session-rename",
        )
    }

    /// Returns the durable attention acknowledgement for a session. `None`
    /// is a real never-seen value, distinct from an absent legacy summary.
    pub fn session_seen_at(&self, session_id: &SessionId) -> StoreResult<Option<u64>> {
        let connection = self.connection()?;
        require_session(&connection, session_id)?;
        let seen_at_ms: Option<i64> = connection
            .query_row(
                "SELECT seen_at_ms FROM sessions WHERE id = ?1",
                [session_id.as_str()],
                |row| row.get(0),
            )
            .map_err(map_sqlite_error)?;
        seen_at_ms
            .map(|value| {
                u64::try_from(value).map_err(|_| corrupt("session attention timestamp is negative"))
            })
            .transpose()
    }

    /// Looks up a committed `session.seen` response before session or
    /// generation validation, preserving the same response-loss recovery law
    /// as `session.rename`.
    pub fn session_seen_receipt(
        &self,
        command_id: &str,
        request_digest: &str,
        request_json: &str,
    ) -> StoreResult<Option<SeenSession>> {
        validate_command_identity(command_id, request_digest, request_json)?;
        let connection = self.connection()?;
        lookup_command_response(
            &connection,
            command_id,
            "session.seen",
            request_digest,
            request_json,
            "session-seen",
        )
    }

    /// Atomically advances one session's shared attention acknowledgement,
    /// appends a non-meaningful `session_seen` config fact, and finalizes the
    /// command receipt. The SQL comparison is the durable monotonicity proof:
    /// `seen_at_ms` is never replaced by a smaller candidate.
    pub fn mark_session_seen(
        &self,
        command: &SessionSeenCommand,
    ) -> StoreResult<SessionSeenOutcome> {
        validate_command_identity(
            &command.command_id,
            &command.request_digest,
            &command.request_json,
        )?;
        if command.worker_generation != self.worker_generation {
            return Err(stale_generation(
                command.worker_generation,
                self.worker_generation,
            ));
        }

        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        if let Some(seen) = lookup_command_response(
            &transaction,
            &command.command_id,
            "session.seen",
            &command.request_digest,
            &command.request_json,
            "session-seen",
        )? {
            transaction.commit().map_err(map_sqlite_error)?;
            return Ok(SessionSeenOutcome::IdempotentReplay { seen });
        }
        require_session(&transaction, &command.session_id)?;

        let now = now_ms()?;
        claim_pending_receipt(
            &transaction,
            &command.command_id,
            "session.seen",
            &command.request_digest,
            &command.request_json,
            now,
        )?;
        transaction
            .execute(
                "UPDATE sessions
                 SET seen_at_ms = CASE
                     WHEN seen_at_ms IS NULL OR seen_at_ms < ?2 THEN ?2
                     ELSE seen_at_ms
                 END
                 WHERE id = ?1",
                params![command.session_id.as_str(), to_sqlite_integer(now)?],
            )
            .map_err(map_sqlite_error)?;
        let seen_at_ms: i64 = transaction
            .query_row(
                "SELECT seen_at_ms FROM sessions WHERE id = ?1",
                [command.session_id.as_str()],
                |row| row.get(0),
            )
            .map_err(map_sqlite_error)?;
        let seen_at_ms = u64::try_from(seen_at_ms)
            .map_err(|_| corrupt("session attention timestamp is negative"))?;
        let mut envelopes = vec![unstamped_raw_command_envelope(
            command.event_id.clone(),
            &command.session_id,
            None,
            None,
            command.device_id.clone(),
            self.worker_generation,
            haider_protocol::session::SessionConfigEventPayload::session_seen_value(seen_at_ms)
                .map_err(|error| {
                    store_error(
                        ErrorCode::InvalidArgument,
                        format!("cannot serialize session-seen payload: {error}"),
                        false,
                    )
                })?,
            PromptRender::Omit,
        )?];
        append_transaction_envelopes(&transaction, &command.session_id, now, &mut envelopes)?;
        let seen = SeenSession {
            session_id: command.session_id.clone(),
            seen_at_ms,
            seen_seq: envelopes[0].seq,
            worker_generation: self.worker_generation,
        };
        finalize_command_receipt(
            &transaction,
            &command.command_id,
            command.session_id.as_str(),
            None,
            Some(seen.seen_seq),
            &seen,
            now,
            "session-seen",
        )?;
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(SessionSeenOutcome::Committed {
            seen,
            envelope: Box::new(envelopes.remove(0)),
        })
    }

    /// Atomically applies one NORMALIZED rename (G2): updates the session's
    /// typed metadata title (every other field preserved), appends the
    /// `session_renamed` fact, and finalizes the command receipt. Any late
    /// failure rolls all three back — the exact `select_session_model`
    /// shape, including the worker-generation fence.
    ///
    /// The daemon owns normalization/validation; this transaction owns only
    /// durability. With `only_if_untitled` set (auto-title), an existing
    /// title short-circuits to [`SessionRenameOutcome::Skipped`] BEFORE any
    /// receipt claim — auto-title never overwrites and leaves no trace when
    /// it yields.
    pub fn rename_session(
        &self,
        command: &SessionRenameCommand,
    ) -> StoreResult<SessionRenameOutcome> {
        validate_command_identity(
            &command.command_id,
            &command.request_digest,
            &command.request_json,
        )?;
        if command.worker_generation != self.worker_generation {
            return Err(stale_generation(
                command.worker_generation,
                self.worker_generation,
            ));
        }
        if let Some(title) = &command.title
            && (title.trim().is_empty()
                || title.chars().count() > 80
                || title.chars().any(char::is_control))
        {
            return Err(store_error(
                ErrorCode::InvalidArgument,
                "session rename must carry a normalized title",
                false,
            ));
        }

        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        if let Some(renamed) = lookup_command_response(
            &transaction,
            &command.command_id,
            "session.rename",
            &command.request_digest,
            &command.request_json,
            "session-rename",
        )? {
            transaction.commit().map_err(map_sqlite_error)?;
            return Ok(SessionRenameOutcome::IdempotentReplay { renamed });
        }
        require_typed_session(&transaction, &command.session_id)?;
        let metadata_json: String = transaction
            .query_row(
                "SELECT meta_json FROM sessions WHERE id = ?1",
                [command.session_id.as_str()],
                |row| row.get(0),
            )
            .map_err(map_sqlite_error)?;
        let Some(mut metadata) = decode_session_metadata(&command.session_id, &metadata_json)?
        else {
            return Err(store_error(
                ErrorCode::InvalidArgument,
                "legacy session has no live-worker metadata",
                false,
            ));
        };
        if command.only_if_untitled && metadata.title.is_some() {
            return Ok(SessionRenameOutcome::Skipped);
        }
        metadata.title = command.title.clone();
        let updated_metadata = serde_json::to_string(&metadata).map_err(|error| {
            store_error(
                ErrorCode::InvalidArgument,
                format!("cannot serialize session metadata: {error}"),
                false,
            )
        })?;

        let now = now_ms()?;
        claim_pending_receipt(
            &transaction,
            &command.command_id,
            "session.rename",
            &command.request_digest,
            &command.request_json,
            now,
        )?;
        let updated_rows = transaction
            .execute(
                "UPDATE sessions SET meta_json = ?2 WHERE id = ?1",
                params![command.session_id.as_str(), updated_metadata],
            )
            .map_err(map_sqlite_error)?;
        if updated_rows != 1 {
            return Err(corrupt("session row disappeared during rename"));
        }
        let mut envelopes = vec![unstamped_raw_command_envelope(
            command.event_id.clone(),
            &command.session_id,
            None,
            None,
            command.device_id.clone(),
            self.worker_generation,
            haider_protocol::session::SessionConfigEventPayload::session_renamed_value(
                command.title.clone(),
            )
            .map_err(|error| {
                store_error(
                    ErrorCode::InvalidArgument,
                    format!("cannot serialize session-renamed payload: {error}"),
                    false,
                )
            })?,
            PromptRender::Omit,
        )?];
        append_transaction_envelopes(&transaction, &command.session_id, now, &mut envelopes)?;
        let renamed = RenamedSession {
            session_id: command.session_id.clone(),
            title: command.title.clone(),
            renamed_seq: envelopes[0].seq,
            worker_generation: self.worker_generation,
        };
        finalize_command_receipt(
            &transaction,
            &command.command_id,
            command.session_id.as_str(),
            None,
            Some(renamed.renamed_seq),
            &renamed,
            now,
            "session-rename",
        )?;
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(SessionRenameOutcome::Committed {
            renamed,
            envelope: Box::new(envelopes.remove(0)),
        })
    }

    /// Looks up a committed `session.select_effort` response before session,
    /// generation, or metadata validation (R2 response-loss replay).
    pub fn session_select_effort_receipt(
        &self,
        command_id: &str,
        request_digest: &str,
        request_json: &str,
    ) -> StoreResult<Option<SelectedEffort>> {
        validate_command_identity(command_id, request_digest, request_json)?;
        let connection = self.connection()?;
        lookup_command_response(
            &connection,
            command_id,
            "session.select_effort",
            request_digest,
            request_json,
            "session-select-effort",
        )
    }

    /// Atomically applies one RESOLVED effort selection: updates the
    /// session's typed metadata (`effort` only, every other field preserved),
    /// appends the `effort_selected` fact, and finalizes the command receipt
    /// — the exact `select_session_model` transaction shape (G3 clones the
    /// F1 law set).
    pub fn select_session_effort(
        &self,
        command: &SessionSelectEffortCommand,
    ) -> StoreResult<SessionSelectEffortOutcome> {
        if command
            .effort
            .as_deref()
            .is_some_and(|effort| effort.trim().is_empty())
        {
            return Err(store_error(
                ErrorCode::InvalidArgument,
                "effort selection must carry a non-empty effort or an explicit revert",
                false,
            ));
        }
        let effort = command.effort.clone();
        let fact = EffortSelected {
            effort: effort.clone(),
        }
        .to_payload_value()
        .map_err(|error| {
            store_error(
                ErrorCode::InvalidArgument,
                format!("cannot serialize effort-selected payload: {error}"),
                false,
            )
        })?;
        let session_id = command.session_id.clone();
        let generation = self.worker_generation;
        let outcome = self.select_session_config(
            SessionConfigSelection {
                command_id: &command.command_id,
                request_digest: &command.request_digest,
                request_json: &command.request_json,
                session_id: &command.session_id,
                worker_generation: command.worker_generation,
                method: "session.select_effort",
                description: "session-select-effort",
                event_id: command.event_id.clone(),
                device_id: command.device_id.clone(),
            },
            fact,
            |_| Ok(()),
            |metadata| metadata.effort = effort.clone(),
            move |selected_seq| SelectedEffort {
                session_id,
                effort: command.effort.clone(),
                selected_seq,
                worker_generation: generation,
            },
        )?;
        Ok(match outcome {
            SessionConfigOutcome::Committed { selected, envelope } => {
                SessionSelectEffortOutcome::Committed { selected, envelope }
            }
            SessionConfigOutcome::IdempotentReplay { selected } => {
                SessionSelectEffortOutcome::IdempotentReplay { selected }
            }
        })
    }

    /// Looks up a committed `session.select_agent_type` response before
    /// session, generation, or metadata validation (R2 response-loss replay).
    pub fn session_select_agent_type_receipt(
        &self,
        command_id: &str,
        request_digest: &str,
        request_json: &str,
    ) -> StoreResult<Option<SelectedAgentType>> {
        validate_command_identity(command_id, request_digest, request_json)?;
        let connection = self.connection()?;
        lookup_command_response(
            &connection,
            command_id,
            "session.select_agent_type",
            request_digest,
            request_json,
            "session-select-agent-type",
        )
    }

    /// Atomically applies one agent-type binding: validates the id against
    /// the Loom registry (a miss is a typed refusal, never a silent bind),
    /// updates the session's typed metadata (`agent_type` only), appends the
    /// `agent_type_selected` fact, and finalizes the command receipt.
    /// `None` reverts the session to plain.
    pub fn select_session_agent_type(
        &self,
        command: &SessionSelectAgentTypeCommand,
    ) -> StoreResult<SessionSelectAgentTypeOutcome> {
        if command
            .agent_type
            .as_deref()
            .is_some_and(|id| id.trim().is_empty())
        {
            return Err(store_error(
                ErrorCode::InvalidArgument,
                "agent-type selection must carry a non-empty id or an explicit revert",
                false,
            ));
        }
        let agent_type = command.agent_type.clone();
        let active_agent_type = agent_type.clone();
        let fact = haider_protocol::session::AgentTypeSelected {
            agent_type: agent_type.clone(),
        }
        .to_payload_value()
        .map_err(|error| {
            store_error(
                ErrorCode::InvalidArgument,
                format!("cannot serialize agent-type-selected payload: {error}"),
                false,
            )
        })?;
        let session_id = command.session_id.clone();
        let generation = self.worker_generation;
        let outcome = self.select_session_config(
            SessionConfigSelection {
                command_id: &command.command_id,
                request_digest: &command.request_digest,
                request_json: &command.request_json,
                session_id: &command.session_id,
                worker_generation: command.worker_generation,
                method: "session.select_agent_type",
                description: "session-select-agent-type",
                event_id: command.event_id.clone(),
                device_id: command.device_id.clone(),
            },
            fact,
            move |transaction| {
                let Some(id) = active_agent_type.as_deref() else {
                    return Ok(());
                };
                let active = transaction
                    .query_row(
                        "SELECT EXISTS(
                             SELECT 1 FROM loom_agent_types
                             WHERE id = ?1 AND archived = 0
                         )",
                        [id],
                        |row| row.get::<_, bool>(0),
                    )
                    .map_err(map_sqlite_error)?;
                if active {
                    Ok(())
                } else {
                    Err(store_error(
                        ErrorCode::InvalidArgument,
                        format!("agent type `{id}` is not registered in the active Loom registry"),
                        false,
                    ))
                }
            },
            |metadata| metadata.agent_type = agent_type.clone(),
            move |selected_seq| SelectedAgentType {
                session_id,
                agent_type: command.agent_type.clone(),
                selected_seq,
                worker_generation: generation,
            },
        )?;
        Ok(match outcome {
            SessionConfigOutcome::Committed { selected, envelope } => {
                SessionSelectAgentTypeOutcome::Committed { selected, envelope }
            }
            SessionConfigOutcome::IdempotentReplay { selected } => {
                SessionSelectAgentTypeOutcome::IdempotentReplay { selected }
            }
        })
    }

    /// Looks up a committed `session.select_fast` response before session,
    /// generation, or metadata validation (R2 response-loss replay).
    pub fn session_select_fast_receipt(
        &self,
        command_id: &str,
        request_digest: &str,
        request_json: &str,
    ) -> StoreResult<Option<SelectedFast>> {
        validate_command_identity(command_id, request_digest, request_json)?;
        let connection = self.connection()?;
        lookup_command_response(
            &connection,
            command_id,
            "session.select_fast",
            request_digest,
            request_json,
            "session-select-fast",
        )
    }

    /// Atomically applies one VALIDATED fast-mode toggle: updates the
    /// session's typed metadata (`fast` only), appends the
    /// `fast_mode_selected` fact, and finalizes the command receipt — the
    /// exact `select_session_model` transaction shape (G3).
    pub fn select_session_fast(
        &self,
        command: &SessionSelectFastCommand,
    ) -> StoreResult<SessionSelectFastOutcome> {
        let enabled = command.enabled;
        let fact = FastModeSelected { enabled }
            .to_payload_value()
            .map_err(|error| {
                store_error(
                    ErrorCode::InvalidArgument,
                    format!("cannot serialize fast-mode-selected payload: {error}"),
                    false,
                )
            })?;
        let session_id = command.session_id.clone();
        let generation = self.worker_generation;
        let outcome = self.select_session_config(
            SessionConfigSelection {
                command_id: &command.command_id,
                request_digest: &command.request_digest,
                request_json: &command.request_json,
                session_id: &command.session_id,
                worker_generation: command.worker_generation,
                method: "session.select_fast",
                description: "session-select-fast",
                event_id: command.event_id.clone(),
                device_id: command.device_id.clone(),
            },
            fact,
            |_| Ok(()),
            |metadata| metadata.fast = enabled,
            move |selected_seq| SelectedFast {
                session_id,
                enabled,
                selected_seq,
                worker_generation: generation,
            },
        )?;
        Ok(match outcome {
            SessionConfigOutcome::Committed { selected, envelope } => {
                SessionSelectFastOutcome::Committed { selected, envelope }
            }
            SessionConfigOutcome::IdempotentReplay { selected } => {
                SessionSelectFastOutcome::IdempotentReplay { selected }
            }
        })
    }

    /// The shared `select_session_model` transaction shape for G3's
    /// session-config selections: receipt replay inside the transaction,
    /// generation fence, typed-metadata mutation, one published fact, and the
    /// finalized receipt — all atomic, any late failure rolls all three back.
    fn select_session_config<R: serde::Serialize + serde::de::DeserializeOwned>(
        &self,
        selection: SessionConfigSelection<'_>,
        fact_payload: serde_json::Value,
        validate: impl FnOnce(&Connection) -> StoreResult<()>,
        mutate: impl FnOnce(&mut SessionMetadataV1),
        respond: impl FnOnce(u64) -> R,
    ) -> StoreResult<SessionConfigOutcome<R>> {
        validate_command_identity(
            selection.command_id,
            selection.request_digest,
            selection.request_json,
        )?;
        if selection.worker_generation != self.worker_generation {
            return Err(stale_generation(
                selection.worker_generation,
                self.worker_generation,
            ));
        }

        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        if let Some(selected) = lookup_command_response(
            &transaction,
            selection.command_id,
            selection.method,
            selection.request_digest,
            selection.request_json,
            selection.description,
        )? {
            transaction.commit().map_err(map_sqlite_error)?;
            return Ok(SessionConfigOutcome::IdempotentReplay { selected });
        }
        require_typed_session(&transaction, selection.session_id)?;
        validate(&transaction)?;
        let metadata_json: String = transaction
            .query_row(
                "SELECT meta_json FROM sessions WHERE id = ?1",
                [selection.session_id.as_str()],
                |row| row.get(0),
            )
            .map_err(map_sqlite_error)?;
        let Some(mut metadata) = decode_session_metadata(selection.session_id, &metadata_json)?
        else {
            return Err(store_error(
                ErrorCode::InvalidArgument,
                "legacy session has no live-worker metadata",
                false,
            ));
        };
        mutate(&mut metadata);
        let updated_metadata = serde_json::to_string(&metadata).map_err(|error| {
            store_error(
                ErrorCode::InvalidArgument,
                format!("cannot serialize session metadata: {error}"),
                false,
            )
        })?;

        let now = now_ms()?;
        claim_pending_receipt(
            &transaction,
            selection.command_id,
            selection.method,
            selection.request_digest,
            selection.request_json,
            now,
        )?;
        let updated_rows = transaction
            .execute(
                "UPDATE sessions SET meta_json = ?2 WHERE id = ?1",
                params![selection.session_id.as_str(), updated_metadata],
            )
            .map_err(map_sqlite_error)?;
        if updated_rows != 1 {
            return Err(corrupt(
                "session row disappeared during session-config selection",
            ));
        }
        let mut envelopes = vec![unstamped_raw_command_envelope(
            selection.event_id.clone(),
            selection.session_id,
            None,
            None,
            selection.device_id.clone(),
            self.worker_generation,
            fact_payload,
            PromptRender::Omit,
        )?];
        append_transaction_envelopes(&transaction, selection.session_id, now, &mut envelopes)?;
        let selected = respond(envelopes[0].seq);
        finalize_command_receipt(
            &transaction,
            selection.command_id,
            selection.session_id.as_str(),
            None,
            Some(envelopes[0].seq),
            &selected,
            now,
            selection.description,
        )?;
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(SessionConfigOutcome::Committed {
            selected,
            envelope: Box::new(envelopes.remove(0)),
        })
    }

    /// Reads one current named-ref descriptor. `None` remains the implicit
    /// legacy/main branch and therefore has no row.
    pub fn branch(
        &self,
        session_id: &SessionId,
        branch_id: &BranchId,
    ) -> StoreResult<Option<BranchDescriptor>> {
        let connection = self.connection()?;
        branch_descriptor(&connection, session_id, branch_id)
    }

    /// Lists named refs in immutable creation order.
    pub fn branches(&self, session_id: &SessionId) -> StoreResult<Vec<BranchDescriptor>> {
        let connection = self.connection()?;
        let mut statement = connection
            .prepare_cached(&format!(
                "{} WHERE session_id = ?1 ORDER BY created_seq ASC",
                branch_select()
            ))
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map([session_id.as_str()], stored_branch)
            .map_err(map_sqlite_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_sqlite_error)?;
        rows.into_iter().map(decode_branch).collect()
    }

    /// Resolves concrete named-ref descriptors from root to leaf. The
    /// implicit main branch contributes no concrete row.
    pub fn branch_lineage(
        &self,
        session_id: &SessionId,
        branch_id: Option<&BranchId>,
    ) -> StoreResult<Vec<BranchDescriptor>> {
        let connection = self.connection()?;
        branch_lineage_descriptors(&connection, session_id, branch_id)
    }

    /// Looks up a committed `turn.submit` response before any worker work.
    /// Obeys the R2 receipt-idempotency law stated on
    /// [`Self::session_create_receipt`].
    pub fn turn_accept_receipt(
        &self,
        command_id: &str,
        request_digest: &str,
        request_json: &str,
    ) -> StoreResult<Option<AcceptedTurn>> {
        validate_command_identity(command_id, request_digest, request_json)?;
        let connection = self.connection()?;
        lookup_turn_accept_receipt(&connection, command_id, request_digest, request_json)
    }

    /// Looks up a committed `run.retry` response before generation/state
    /// validation so response-loss replay remains safe across restarts.
    pub fn run_retry_receipt(
        &self,
        command_id: &str,
        request_digest: &str,
        request_json: &str,
    ) -> StoreResult<Option<AcceptedRunRetry>> {
        validate_command_identity(command_id, request_digest, request_json)?;
        let connection = self.connection()?;
        lookup_command_response(
            &connection,
            command_id,
            "run.retry",
            request_digest,
            request_json,
            "run-retry",
        )
    }

    /// Atomically accepts a fresh run from the latest failed main-timeline
    /// user turn, without appending another `UserMessage` or tree node.
    ///
    /// The live-run gate and receipt finalization share this transaction:
    /// two distinct retry commands cannot both observe the failed session as
    /// idle, and a replay of one committed command cannot mint another run.
    pub fn accept_run_retry(&self, command: &RunRetryCommand) -> StoreResult<RunRetryOutcome> {
        validate_command_identity(
            &command.command_id,
            &command.request_digest,
            &command.request_json,
        )?;
        if command.worker_generation != self.worker_generation {
            return Err(stale_generation(
                command.worker_generation,
                self.worker_generation,
            ));
        }
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        if let Some(accepted) = lookup_command_response(
            &transaction,
            &command.command_id,
            "run.retry",
            &command.request_digest,
            &command.request_json,
            "run-retry",
        )? {
            transaction.commit().map_err(map_sqlite_error)?;
            return Ok(RunRetryOutcome::IdempotentReplay { accepted });
        }
        require_typed_session(&transaction, &command.session_id)?;
        let states = latest_run_states(&transaction, &command.session_id)?;
        if states.contains_key(&command.run_id) {
            return Err(corrupt("daemon-minted retry run id already exists"));
        }
        // Queued turns may have newer state sequences than the worker's one
        // active run. Find an eligible main-timeline backoff across ALL
        // nonterminal run heads before applying the generic newest-live-run
        // refusal, so queued work cannot shadow the wait that `run.retry`
        // exists to wake.
        let mut eligible_backoff = None;
        for (run_id, (_, state_seq, _)) in states
            .iter()
            .filter(|(_, (state, _, _))| matches!(state, RunState::Retrying { .. }))
        {
            let Some(backoff_event_id) = main_timeline_retrying_event_id(
                &transaction,
                &command.session_id,
                run_id,
                *state_seq,
            )?
            else {
                continue;
            };
            let Some((prompt_run_id, user_seq)) =
                main_timeline_run_prompt_source(&transaction, &command.session_id, run_id)?
            else {
                continue;
            };
            if eligible_backoff
                .as_ref()
                .is_some_and(|(_, accepted_seq, _, _, _)| accepted_seq >= state_seq)
            {
                continue;
            }
            eligible_backoff = Some((
                run_id.clone(),
                *state_seq,
                backoff_event_id,
                prompt_run_id,
                user_seq,
            ));
        }
        if let Some((run_id, state_seq, backoff_event_id, prompt_run_id, user_seq)) =
            eligible_backoff
        {
            let now = now_ms()?;
            claim_pending_receipt(
                &transaction,
                &command.command_id,
                "run.retry",
                &command.request_digest,
                &command.request_json,
                now,
            )?;
            let accepted = AcceptedRunRetry {
                session_id: command.session_id.clone(),
                run_id: run_id.clone(),
                failed_run_id: run_id,
                prompt_run_id,
                user_seq,
                accepted_seq: state_seq,
                worker_generation: self.worker_generation,
                backoff_event_id: Some(backoff_event_id),
            };
            finalize_command_receipt(
                &transaction,
                &command.command_id,
                command.session_id.as_str(),
                Some(accepted.run_id.as_str()),
                Some(accepted.accepted_seq),
                &accepted,
                now,
                "run-retry",
            )?;
            transaction.commit().map_err(map_sqlite_error)?;
            return Ok(RunRetryOutcome::Committed {
                accepted,
                envelopes: Vec::new(),
            });
        }
        if let Some((run_id, (state, _, _))) = states
            .iter()
            .filter(|(_, (state, _, _))| !state.is_terminal())
            .max_by_key(|(_, (_, seq, _))| *seq)
        {
            return Err(store_error(
                ErrorCode::InvalidArgument,
                format!(
                    "run retry requires a terminal-failed session or main-timeline provider backoff; run {run_id} is still live ({state:?})"
                ),
                false,
            ));
        }
        let Some((failed_run_id, prompt_run_id, user_seq)) =
            latest_main_timeline_failed_turn(&transaction, &command.session_id)?
        else {
            return Err(store_error(
                ErrorCode::InvalidArgument,
                "run retry requires the latest main-timeline user turn to be terminal-failed",
                false,
            ));
        };
        let now = now_ms()?;
        claim_pending_receipt(
            &transaction,
            &command.command_id,
            "run.retry",
            &command.request_digest,
            &command.request_json,
            now,
        )?;
        let retried_payload = RunRetryEventPayload::RunRetried {
            failed_run_id: failed_run_id.clone(),
            prompt_run_id: prompt_run_id.clone(),
            user_seq,
        }
        .to_payload_value()
        .map_err(|error| {
            store_error(
                ErrorCode::InvalidArgument,
                format!("cannot serialize run-retried payload: {error}"),
                false,
            )
        })?;
        let mut envelopes = vec![
            unstamped_command_envelope(
                command.queued_event_id.clone(),
                &command.session_id,
                None,
                Some(command.run_id.clone()),
                command.device_id.clone(),
                self.worker_generation,
                EventPayload::RunState(RunState::Queued),
                PromptRender::Omit,
            )?,
            unstamped_raw_command_envelope(
                command.retried_event_id.clone(),
                &command.session_id,
                None,
                Some(command.run_id.clone()),
                command.device_id.clone(),
                self.worker_generation,
                retried_payload,
                PromptRender::Omit,
            )?,
            unstamped_command_envelope(
                command.active_event_id.clone(),
                &command.session_id,
                None,
                Some(command.run_id.clone()),
                command.device_id.clone(),
                self.worker_generation,
                EventPayload::SessionState(SessionState::ActiveRun),
                PromptRender::Omit,
            )?,
        ];
        append_transaction_envelopes(&transaction, &command.session_id, now, &mut envelopes)?;
        let accepted = AcceptedRunRetry {
            session_id: command.session_id.clone(),
            run_id: command.run_id.clone(),
            failed_run_id,
            prompt_run_id,
            user_seq,
            accepted_seq: envelopes[1].seq,
            worker_generation: self.worker_generation,
            backoff_event_id: None,
        };
        finalize_command_receipt(
            &transaction,
            &command.command_id,
            command.session_id.as_str(),
            Some(command.run_id.as_str()),
            Some(accepted.accepted_seq),
            &accepted,
            now,
            "run-retry",
        )?;
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(RunRetryOutcome::Committed {
            accepted,
            envelopes,
        })
    }

    /// Looks up a committed direct-shell acceptance before generation/busy
    /// validation so response-loss replay survives daemon restart.
    pub fn shell_exec_receipt(
        &self,
        command_id: &str,
        request_digest: &str,
        request_json: &str,
    ) -> StoreResult<Option<AcceptedShellExec>> {
        validate_command_identity(command_id, request_digest, request_json)?;
        let connection = self.connection()?;
        lookup_command_response(
            &connection,
            command_id,
            "shell.exec",
            request_digest,
            request_json,
            "shell-exec",
        )
    }

    /// Atomically accepts a direct user shell command without creating a
    /// `UserMessage`. The synthetic run owns the session before worker
    /// handoff, so it cannot open a parallel side-effect lane beside a turn.
    pub fn accept_shell_exec(
        &self,
        command: &ShellExecAcceptCommand,
    ) -> StoreResult<ShellExecAcceptOutcome> {
        validate_command_identity(
            &command.command_id,
            &command.request_digest,
            &command.request_json,
        )?;
        if command.worker_generation != self.worker_generation {
            return Err(stale_generation(
                command.worker_generation,
                self.worker_generation,
            ));
        }
        if command.command.trim().is_empty() || command.command.len() > 8_192 {
            return Err(store_error(
                ErrorCode::InvalidArgument,
                "shell command must contain 1..=8192 UTF-8 bytes",
                false,
            ));
        }
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        if let Some(accepted) = lookup_command_response(
            &transaction,
            &command.command_id,
            "shell.exec",
            &command.request_digest,
            &command.request_json,
            "shell-exec",
        )? {
            transaction.commit().map_err(map_sqlite_error)?;
            return Ok(ShellExecAcceptOutcome::IdempotentReplay { accepted });
        }
        require_typed_session(&transaction, &command.session_id)?;
        if let Some(branch_id) = command.branch_id.as_ref()
            && branch_descriptor(&transaction, &command.session_id, branch_id)?.is_none()
        {
            return Err(store_error(
                ErrorCode::InvalidArgument,
                format!("branch {branch_id} does not exist"),
                false,
            ));
        }
        let expected_agent = lookup_delegation_by_child_session(&transaction, &command.session_id)?
            .map(|delegation| delegation.agent_id);
        if command.agent_id != expected_agent {
            return Err(store_error(
                ErrorCode::InvalidArgument,
                match expected_agent {
                    Some(agent_id) => format!(
                        "direct shell scope must use delegated agent {agent_id} for this child session"
                    ),
                    None => "direct shell scope must not name an agent for a root session".into(),
                },
                false,
            ));
        }
        if has_nonterminal_run(&transaction, &command.session_id)? {
            return Err(store_error(
                ErrorCode::Busy,
                "direct shell execution requires an idle session",
                true,
            ));
        }
        let now = now_ms()?;
        claim_pending_receipt(
            &transaction,
            &command.command_id,
            "shell.exec",
            &command.request_digest,
            &command.request_json,
            now,
        )?;
        let started = TurnItem::CommandExecution {
            call_id: command.command_id.clone(),
            command: command.command.clone(),
            status: haider_protocol::item::ToolStatus::InProgress,
            exit_code: None,
        };
        let origin = UserCommandOriginV1 {
            origin: CommandExecutionOrigin::UserCommand,
            command_item_id: command.item_id.clone(),
            call_id: command.command_id.clone(),
        }
        .extension_item()
        .map_err(|error| {
            store_error(
                ErrorCode::InvalidArgument,
                format!("cannot serialize user-command origin: {error}"),
                false,
            )
        })?;
        let origin_item_id = ItemId::new(format!("user-command-origin-{}", command.item_id));
        let mut origin_envelope = unstamped_command_envelope(
            EventId::new(format!("user-command-origin-{}", command.item_event_id)),
            &command.session_id,
            command.branch_id.clone(),
            Some(command.run_id.clone()),
            command.device_id.clone(),
            self.worker_generation,
            EventPayload::Item(ItemEvent::Completed {
                item_id: origin_item_id,
                item: origin,
            }),
            PromptRender::Omit,
        )?;
        // Provenance is durable prompt metadata, not a second transcript row.
        origin_envelope.render.ui = false;
        origin_envelope.agent_id = command.agent_id.clone();
        let mut envelopes = vec![
            unstamped_command_envelope(
                command.running_event_id.clone(),
                &command.session_id,
                command.branch_id.clone(),
                Some(command.run_id.clone()),
                command.device_id.clone(),
                self.worker_generation,
                EventPayload::RunState(RunState::RunningTool),
                PromptRender::Omit,
            )?,
            unstamped_command_envelope(
                command.item_event_id.clone(),
                &command.session_id,
                command.branch_id.clone(),
                Some(command.run_id.clone()),
                command.device_id.clone(),
                self.worker_generation,
                EventPayload::Item(ItemEvent::Started {
                    item_id: command.item_id.clone(),
                    item: started,
                }),
                PromptRender::Omit,
            )?,
            origin_envelope,
            unstamped_command_envelope(
                command.active_event_id.clone(),
                &command.session_id,
                command.branch_id.clone(),
                Some(command.run_id.clone()),
                command.device_id.clone(),
                self.worker_generation,
                EventPayload::SessionState(SessionState::ActiveRun),
                PromptRender::Omit,
            )?,
        ];
        for envelope in &mut envelopes {
            envelope.agent_id = command.agent_id.clone();
        }
        append_transaction_envelopes(&transaction, &command.session_id, now, &mut envelopes)?;
        let accepted = AcceptedShellExec {
            session_id: command.session_id.clone(),
            run_id: command.run_id.clone(),
            item_id: command.item_id.clone(),
            accepted_seq: envelopes[1].seq,
            worker_generation: self.worker_generation,
        };
        finalize_command_receipt(
            &transaction,
            &command.command_id,
            command.session_id.as_str(),
            Some(command.run_id.as_str()),
            Some(accepted.accepted_seq),
            &accepted,
            now,
            "shell-exec",
        )?;
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(ShellExecAcceptOutcome::Committed {
            accepted,
            envelopes,
        })
    }

    /// Atomically commits the submit receipt, `Queued`, `UserMessage`, and,
    /// for the first runnable turn, aggregate `SessionState::ActiveRun`
    /// (R3: only after this transaction is durable may provider work start).
    ///
    /// CALLER CONTRACT: this method fences `worker_generation` BEFORE its
    /// in-transaction receipt replay, so calling it directly with a
    /// pre-restart command returns `stale_generation` instead of the
    /// committed response. Cross-restart response recovery is owned by the
    /// unfenced [`Self::turn_accept_receipt`], which the wire layer must
    /// consult first (R2 law on [`Self::session_create_receipt`]). The
    /// composition — unfenced replay, then fenced acceptance — reproduces
    /// the menu CAS's replay-before-fence semantics end to end.
    pub fn accept_turn(&self, command: &TurnAcceptCommand) -> StoreResult<TurnAcceptOutcome> {
        let mut command = command.clone();
        validate_command_identity(
            &command.command_id,
            &command.request_digest,
            &command.request_json,
        )?;
        if command.worker_generation != self.worker_generation {
            return Err(stale_generation(
                command.worker_generation,
                self.worker_generation,
            ));
        }
        if command.text.is_empty() {
            return Err(store_error(
                ErrorCode::InvalidArgument,
                "turn text must not be empty",
                false,
            ));
        }
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        if let Some(accepted) = lookup_turn_accept_receipt(
            &transaction,
            &command.command_id,
            &command.request_digest,
            &command.request_json,
        )? {
            transaction.commit().map_err(map_sqlite_error)?;
            return Ok(TurnAcceptOutcome::IdempotentReplay { accepted });
        }
        require_typed_session(&transaction, &command.session_id)?;
        let named_branch = command
            .branch_id
            .as_ref()
            .map(|branch_id| {
                branch_descriptor(&transaction, &command.session_id, branch_id)?.ok_or_else(|| {
                    store_error(
                        ErrorCode::InvalidArgument,
                        format!("branch {branch_id} does not exist"),
                        false,
                    )
                })
            })
            .transpose()?;
        let states = latest_run_states(&transaction, &command.session_id)?;
        // RPC submit does not carry a run id. For Subturn only, bind the
        // daemon-minted candidate to the newest actually-running response on
        // the requested branch inside this serialized transaction. Queue
        // remains a fresh run, and Steer's established explicit-run behavior
        // is unchanged.
        if command.mode == DeliveryMode::Subturn && !states.contains_key(&command.run_id) {
            let active = states
                .iter()
                .filter(|(_, (state, _, branch_id))| {
                    branch_id == &command.branch_id
                        && !state.is_terminal()
                        && !matches!(
                            state,
                            RunState::Queued
                                | RunState::Compacting
                                | RunState::Cancelling
                                | RunState::EffectOutcomeUnknown
                        )
                })
                .max_by_key(|(_, (_, seq, _))| *seq)
                .map(|(run_id, _)| run_id.clone());
            if let Some(active) = active {
                command.run_id = active;
            }
        }
        let same_run_delivery = states
            .get(&command.run_id)
            .is_some_and(|(state, _, branch_id)| {
                matches!(command.mode, DeliveryMode::Steer | DeliveryMode::Subturn)
                    && !state.is_terminal()
                    && *state != RunState::Cancelling
                    && branch_id == &command.branch_id
            });
        if states.contains_key(&command.run_id) && !same_run_delivery {
            return Err(corrupt("daemon-minted turn run id already exists"));
        }
        if !same_run_delivery
            && command.branch_id.as_ref().is_some_and(|requested_branch| {
                states.values().any(|(state, _, branch_id)| {
                    !state.is_terminal() && branch_id.as_ref() == Some(requested_branch)
                })
            })
        {
            // Named refs have one mutable head. Accepting a second run on the
            // same ref before the first reaches a terminal node would commit
            // its user node ahead of the first run's later assistant nodes,
            // leaving no honest immutable parent order. Cross-branch work may
            // still queue behind the session's active worker.
            return Err(store_error(
                ErrorCode::Busy,
                format!(
                    "branch {} already has a nonterminal run",
                    command
                        .branch_id
                        .as_ref()
                        .map_or("<unknown>", BranchId::as_str)
                ),
                true,
            ));
        }
        let has_active = states.values().any(|(state, _, _)| !state.is_terminal());
        let disposition = if same_run_delivery {
            match command.mode {
                DeliveryMode::Steer => TurnAdmissionDisposition::SteerPending,
                DeliveryMode::Subturn => TurnAdmissionDisposition::SubturnPending,
                DeliveryMode::Queue => unreachable!("queue cannot be a same-run delivery"),
            }
        } else if has_active {
            // A newly minted run remains an explicitly queued turn. Only a
            // same-run daemon steer may use `SteerPending`.
            TurnAdmissionDisposition::Queued
        } else {
            TurnAdmissionDisposition::Started
        };
        let now = now_ms()?;
        claim_pending_receipt(
            &transaction,
            &command.command_id,
            "turn.submit",
            &command.request_digest,
            &command.request_json,
            now,
        )?;

        let parent = if command.agent_id.is_none() {
            named_branch
                .as_ref()
                .map(|branch| branch.head_node_id.clone())
                .or(latest_tree_head(
                    &transaction,
                    &command.session_id,
                    None,
                    None,
                )?)
        } else {
            latest_tree_head(
                &transaction,
                &command.session_id,
                command.branch_id.as_ref(),
                command.agent_id.as_ref(),
            )?
        };
        // G2 auto-title coordinate: the session's FIRST main-timeline user
        // node is exactly the one committed with no tree parent on the
        // main lane (a named branch forks from an existing node; a steer
        // and a subagent turn always have ancestry).
        let first_user_turn = command.agent_id.is_none()
            && command.branch_id.is_none()
            && !same_run_delivery
            && parent.is_none();
        let user_node = TreeNode {
            node: NodeId::new(format!("node-{}", command.user_event_id)),
            parent,
            kind: NodeKind::UserTurn {
                text: command.text.clone(),
                attachments: command.attachments.clone(),
            },
        };
        let mut envelopes = if same_run_delivery {
            vec![
                unstamped_command_envelope(
                    command.user_event_id.clone(),
                    &command.session_id,
                    command.branch_id.clone(),
                    Some(command.run_id.clone()),
                    command.device_id.clone(),
                    self.worker_generation,
                    EventPayload::UserMessage {
                        text: command.text.clone(),
                        attachments: command.attachments.clone(),
                        mode: command.mode,
                    },
                    PromptRender::Verbatim,
                )?,
                unstamped_command_envelope(
                    EventId::new(format!("tree-{}", command.user_event_id)),
                    &command.session_id,
                    command.branch_id.clone(),
                    Some(command.run_id.clone()),
                    command.device_id.clone(),
                    self.worker_generation,
                    EventPayload::NodeCommitted(user_node),
                    PromptRender::Omit,
                )?,
            ]
        } else {
            vec![
                unstamped_command_envelope(
                    command.queued_event_id.clone(),
                    &command.session_id,
                    command.branch_id.clone(),
                    Some(command.run_id.clone()),
                    command.device_id.clone(),
                    self.worker_generation,
                    EventPayload::RunState(RunState::Queued),
                    PromptRender::Omit,
                )?,
                unstamped_command_envelope(
                    command.user_event_id.clone(),
                    &command.session_id,
                    command.branch_id.clone(),
                    Some(command.run_id.clone()),
                    command.device_id.clone(),
                    self.worker_generation,
                    EventPayload::UserMessage {
                        text: command.text.clone(),
                        attachments: command.attachments.clone(),
                        mode: command.mode,
                    },
                    PromptRender::Verbatim,
                )?,
                unstamped_command_envelope(
                    EventId::new(format!("tree-{}", command.user_event_id)),
                    &command.session_id,
                    command.branch_id.clone(),
                    Some(command.run_id.clone()),
                    command.device_id.clone(),
                    self.worker_generation,
                    EventPayload::NodeCommitted(user_node),
                    PromptRender::Omit,
                )?,
            ]
        };
        if disposition == TurnAdmissionDisposition::Started {
            envelopes.push(unstamped_command_envelope(
                command.active_event_id.clone(),
                &command.session_id,
                None,
                Some(command.run_id.clone()),
                command.device_id.clone(),
                self.worker_generation,
                EventPayload::SessionState(SessionState::ActiveRun),
                PromptRender::Omit,
            )?);
        }
        if disposition == TurnAdmissionDisposition::Queued {
            let ordinal = u32::try_from(queue_entries(&transaction, &command.session_id)?.1.len())
                .map_err(|_| {
                    store_error(ErrorCode::Busy, "queue ordinal space is exhausted", true)
                })?
                .checked_add(1)
                .ok_or_else(|| {
                    store_error(ErrorCode::Busy, "queue ordinal space is exhausted", true)
                })?;
            envelopes.push(unstamped_command_envelope(
                EventId::new(format!("queue-enqueued-{}", command.user_event_id)),
                &command.session_id,
                command.branch_id.clone(),
                Some(command.run_id.clone()),
                command.device_id.clone(),
                self.worker_generation,
                EventPayload::QueueChanged(QueueDelta {
                    revision: 0,
                    change: QueueChange::Enqueued {
                        row: QueueRow {
                            id: command.user_event_id.clone(),
                            text: command.text.clone(),
                            mode: command.mode,
                            ordinal,
                            created_at_ms: 0,
                        },
                    },
                }),
                PromptRender::Omit,
            )?);
        }
        let trust_hooks = serde_json::from_str::<serde_json::Value>(&command.request_json)
            .ok()
            .and_then(|request| {
                request
                    .get("trust_hooks")
                    .and_then(serde_json::Value::as_bool)
            })
            .unwrap_or(false);
        if trust_hooks {
            envelopes.push(unstamped_raw_command_envelope(
                EventId::new(format!("hook-trust-{}", command.queued_event_id)),
                &command.session_id,
                command.branch_id.clone(),
                Some(command.run_id.clone()),
                command.device_id.clone(),
                self.worker_generation,
                HookEventPayload::HookRunTrust { enabled: true }
                    .to_payload_value()
                    .map_err(|error| {
                        store_error(
                            ErrorCode::InvalidArgument,
                            format!("cannot serialize hook run trust fact: {error}"),
                            false,
                        )
                    })?,
                PromptRender::Omit,
            )?);
        }
        let headless_spec = serde_json::from_str::<serde_json::Value>(&command.request_json)
            .ok()
            .and_then(|request| request.get("headless").cloned())
            .map(serde_json::from_value::<haider_protocol::headless::HeadlessRunSpecV1>)
            .transpose()
            .map_err(|error| {
                store_error(
                    ErrorCode::InvalidArgument,
                    format!("cannot decode headless run spec: {error}"),
                    false,
                )
            })?;
        if let Some(spec) = headless_spec {
            let payload =
                haider_protocol::headless::HeadlessRunEventPayload::HeadlessRunConfigured(spec)
                    .to_payload_value()
                    .map_err(|error| {
                        store_error(
                            ErrorCode::InvalidArgument,
                            format!("cannot serialize headless run fact: {error}"),
                            false,
                        )
                    })?;
            envelopes.push(unstamped_raw_command_envelope(
                EventId::new(format!("headless-config-{}", command.queued_event_id)),
                &command.session_id,
                command.branch_id.clone(),
                Some(command.run_id.clone()),
                command.device_id.clone(),
                self.worker_generation,
                payload,
                PromptRender::Omit,
            )?);
        }
        for envelope in &mut envelopes {
            envelope.agent_id = command.agent_id.clone();
        }
        augment_workflow_graph_envelopes(
            &transaction,
            &self.cas,
            &command.session_id,
            &mut envelopes,
        )?;
        append_transaction_envelopes(&transaction, &command.session_id, now, &mut envelopes)?;
        let accepted_seq = if same_run_delivery {
            envelopes[0].seq
        } else {
            envelopes[1].seq
        };
        let accepted = AcceptedTurn {
            session_id: command.session_id.clone(),
            run_id: command.run_id.clone(),
            accepted_seq,
            worker_generation: self.worker_generation,
            branch_id: command.branch_id.clone(),
            disposition,
            first_user_turn,
            pdf_attachments: command
                .attachments
                .iter()
                .filter_map(|attachment| match attachment {
                    AttachmentBlock::Pdf {
                        artifact,
                        pages,
                        delivery,
                        ..
                    } => Some(haider_protocol::tool::PdfAttachmentReceipt {
                        artifact: artifact.clone(),
                        pages: *pages,
                        delivery: *delivery,
                    }),
                    _ => None,
                })
                .collect(),
        };
        finalize_command_receipt(
            &transaction,
            &command.command_id,
            command.session_id.as_str(),
            Some(command.run_id.as_str()),
            Some(accepted_seq),
            &accepted,
            now,
            "turn-submit",
        )?;
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(TurnAcceptOutcome::Committed {
            accepted,
            envelopes,
        })
    }

    /// Reads one coherent queue snapshot from the durable held-turn state.
    pub fn queue_snapshot(&self, session_id: &SessionId) -> StoreResult<QueueSnapshot> {
        let connection = self.connection()?;
        require_typed_session(&connection, session_id)?;
        let (revision, entries) = queue_entries(&connection, session_id)?;
        Ok(QueueSnapshot {
            revision,
            rows: entries.into_iter().map(|entry| entry.row).collect(),
        })
    }

    /// Revision-fenced removal. The comparison and cancelling transition are
    /// one IMMEDIATE transaction; a stale request cannot reach id lookup or
    /// mutate any durable or live queue state.
    pub fn queue_remove(&self, command: &QueueRemoveCommand) -> StoreResult<QueueRemoveOutcome> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        require_typed_session(&transaction, &command.session_id)?;
        let (current_revision, entries) = queue_entries(&transaction, &command.session_id)?;
        // MUTATION CHECK: drop this comparison and the stale-revision and
        // concurrent-remove pins allow an old snapshot to mutate a new queue.
        if command.revision != current_revision {
            return Err(queue_revision_conflict(command.revision, current_revision));
        }
        let entry = entries
            .into_iter()
            .find(|entry| entry.row.id == command.id)
            .ok_or_else(|| {
                store_error(
                    ErrorCode::InvalidArgument,
                    format!("queue item {} is not held", command.id),
                    false,
                )
            })?;
        let now = now_ms()?;
        let mut envelopes = vec![
            unstamped_command_envelope(
                command.cancelling_event_id.clone(),
                &command.session_id,
                entry.branch_id.clone(),
                Some(entry.run_id.clone()),
                command.device_id.clone(),
                self.worker_generation,
                EventPayload::RunState(RunState::Cancelling),
                PromptRender::Omit,
            )?,
            unstamped_command_envelope(
                command.delta_event_id.clone(),
                &command.session_id,
                entry.branch_id,
                Some(entry.run_id),
                command.device_id.clone(),
                self.worker_generation,
                EventPayload::QueueChanged(QueueDelta {
                    revision: 0,
                    change: QueueChange::Removed {
                        id: command.id.clone(),
                    },
                }),
                PromptRender::Omit,
            )?,
        ];
        append_transaction_envelopes(&transaction, &command.session_id, now, &mut envelopes)?;
        let revision = envelopes[1].seq;
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(QueueRemoveOutcome {
            revision,
            envelopes,
        })
    }

    /// Revision-fenced conversion of one queued turn into a steer delivery
    /// for the currently active response.
    pub fn queue_promote_preview(
        &self,
        command: &QueuePromoteCommand,
    ) -> StoreResult<QueuePromotePreview> {
        let connection = self.connection()?;
        require_typed_session(&connection, &command.session_id)?;
        let (entry, active_run_id, _) = queue_promote_target(&connection, command)?;
        Ok(QueuePromotePreview {
            active_run_id,
            text: entry.row.text,
        })
    }

    /// Revision-fenced conversion of one queued turn into a steer delivery
    /// for the currently active response.
    pub fn queue_promote_steer(
        &self,
        command: &QueuePromoteCommand,
    ) -> StoreResult<QueuePromoteOutcome> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        require_typed_session(&transaction, &command.session_id)?;
        let (entry, active_run_id, active_branch_id) = queue_promote_target(&transaction, command)?;
        let now = now_ms()?;
        let mut delivery = unstamped_command_envelope(
            command.delivery_event_id.clone(),
            &command.session_id,
            active_branch_id,
            Some(active_run_id.clone()),
            command.device_id.clone(),
            self.worker_generation,
            EventPayload::UserMessage {
                text: entry.row.text.clone(),
                attachments: Vec::new(),
                mode: DeliveryMode::Steer,
            },
            PromptRender::Verbatim,
        )?;
        // The original user event is the sole visible rendering. This second
        // fact is durable delivery truth for crash recovery and deduplication.
        delivery.render.ui = false;
        let mut envelopes = vec![
            unstamped_command_envelope(
                command.cancelling_event_id.clone(),
                &command.session_id,
                entry.branch_id.clone(),
                Some(entry.run_id.clone()),
                command.device_id.clone(),
                self.worker_generation,
                EventPayload::RunState(RunState::Cancelling),
                PromptRender::Omit,
            )?,
            delivery,
            unstamped_command_envelope(
                command.delta_event_id.clone(),
                &command.session_id,
                entry.branch_id,
                Some(entry.run_id),
                command.device_id.clone(),
                self.worker_generation,
                EventPayload::QueueChanged(QueueDelta {
                    revision: 0,
                    change: QueueChange::PromotedSteer {
                        id: command.id.clone(),
                    },
                }),
                PromptRender::Omit,
            )?,
        ];
        append_transaction_envelopes(&transaction, &command.session_id, now, &mut envelopes)?;
        let delivery_seq = envelopes[1].seq;
        let revision = envelopes[2].seq;
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(QueuePromoteOutcome {
            revision,
            active_run_id,
            delivery_seq,
            text: entry.row.text,
            envelopes,
        })
    }

    /// Removes a queued row exactly once when its worker takes ownership for
    /// delivery. A concurrent remove/promote that committed first leaves no
    /// row and therefore produces no consumption event.
    pub fn queue_consume(
        &self,
        command: &QueueConsumeCommand,
    ) -> StoreResult<Option<QueueConsumeOutcome>> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        require_typed_session(&transaction, &command.session_id)?;
        let (_, entries) = queue_entries(&transaction, &command.session_id)?;
        let Some(entry) = entries
            .into_iter()
            .find(|entry| entry.run_id == command.run_id)
        else {
            transaction.commit().map_err(map_sqlite_error)?;
            return Ok(None);
        };
        let mut envelopes = [unstamped_command_envelope(
            command.delta_event_id.clone(),
            &command.session_id,
            entry.branch_id,
            Some(entry.run_id),
            command.device_id.clone(),
            self.worker_generation,
            EventPayload::QueueChanged(QueueDelta {
                revision: 0,
                change: QueueChange::Consumed {
                    id: entry.row.id.clone(),
                },
            }),
            PromptRender::Omit,
        )?];
        append_transaction_envelopes(&transaction, &command.session_id, now_ms()?, &mut envelopes)?;
        let [envelope] = envelopes;
        let revision = envelope.seq;
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(Some(QueueConsumeOutcome {
            revision,
            id: entry.row.id,
            envelope: Box::new(envelope),
        }))
    }

    /// Looks up a committed `turn.cancel` response before in-memory routing.
    /// Obeys the R2 receipt-idempotency law stated on
    /// [`Self::session_create_receipt`].
    pub fn turn_cancel_receipt(
        &self,
        command_id: &str,
        request_digest: &str,
        request_json: &str,
    ) -> StoreResult<Option<CancelledTurn>> {
        validate_command_identity(command_id, request_digest, request_json)?;
        let connection = self.connection()?;
        lookup_turn_cancel_receipt(&connection, command_id, request_digest, request_json)
    }

    /// Atomically records cancellation intent before any worker is signalled
    /// (R5: `Cancelling` is durable before any wake; an already-terminal run
    /// replies `already_terminal` with its terminal sequence).
    ///
    /// CALLER CONTRACT: generation-fenced before receipt replay, exactly
    /// like [`Self::accept_turn`] — cross-restart response recovery belongs
    /// to the unfenced [`Self::turn_cancel_receipt`], consulted first by
    /// the wire layer.
    pub fn cancel_turn(&self, command: &TurnCancelCommand) -> StoreResult<TurnCancelOutcome> {
        validate_command_identity(
            &command.command_id,
            &command.request_digest,
            &command.request_json,
        )?;
        if command.worker_generation != self.worker_generation {
            return Err(stale_generation(
                command.worker_generation,
                self.worker_generation,
            ));
        }
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        if let Some(cancelled) = lookup_turn_cancel_receipt(
            &transaction,
            &command.command_id,
            &command.request_digest,
            &command.request_json,
        )? {
            transaction.commit().map_err(map_sqlite_error)?;
            return Ok(TurnCancelOutcome::IdempotentReplay { cancelled });
        }
        require_session(&transaction, &command.session_id)?;
        let Some((state, state_seq, branch_id)) =
            latest_run_state(&transaction, &command.session_id, &command.run_id)?
        else {
            return Err(store_error(
                ErrorCode::RunNotActive,
                format!(
                    "run {} does not exist in session {}",
                    command.run_id, command.session_id
                ),
                false,
            ));
        };
        let now = now_ms()?;
        claim_pending_receipt(
            &transaction,
            &command.command_id,
            "turn.cancel",
            &command.request_digest,
            &command.request_json,
            now,
        )?;

        let (cancelled, envelope) = if state.is_terminal() {
            (
                CancelledTurn {
                    session_id: command.session_id.clone(),
                    run_id: command.run_id.clone(),
                    status: TurnCancellationStatus::AlreadyTerminal,
                    terminal_seq: Some(state_seq),
                },
                None,
            )
        } else if state == RunState::Cancelling {
            (
                CancelledTurn {
                    session_id: command.session_id.clone(),
                    run_id: command.run_id.clone(),
                    status: TurnCancellationStatus::Accepted,
                    terminal_seq: None,
                },
                None,
            )
        } else {
            let mut envelopes = vec![unstamped_command_envelope(
                command.cancelling_event_id.clone(),
                &command.session_id,
                branch_id,
                Some(command.run_id.clone()),
                command.device_id.clone(),
                self.worker_generation,
                EventPayload::RunState(RunState::Cancelling),
                PromptRender::Omit,
            )?];
            append_transaction_envelopes(&transaction, &command.session_id, now, &mut envelopes)?;
            (
                CancelledTurn {
                    session_id: command.session_id.clone(),
                    run_id: command.run_id.clone(),
                    status: TurnCancellationStatus::Accepted,
                    terminal_seq: None,
                },
                envelopes.pop().map(Box::new),
            )
        };
        finalize_command_receipt(
            &transaction,
            &command.command_id,
            command.session_id.as_str(),
            Some(command.run_id.as_str()),
            cancelled.terminal_seq,
            &cancelled,
            now,
            "turn-cancel",
        )?;
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(TurnCancelOutcome::Committed {
            cancelled,
            envelope,
        })
    }

    /// Claims the durable login command (transaction A of R10's
    /// two-transaction shape).
    ///
    /// FENCE-VS-REPLAY RESOLUTION (`docs/OPTIMIZATIONS.md`, trigger fired by
    /// this first non-wire receipt caller): login builds on the GENERIC
    /// receipt path — the required unfenced replay preflight
    /// ([`lookup_command_response`]) runs INSIDE this claim transaction, so
    /// the account actor (a direct, non-wire caller) can never silently skip
    /// it, and no replay moved behind a generation fence (login has no
    /// generation to fence). Menu CAS and the turn commands keep their two
    /// explicit mechanisms unchanged.
    ///
    /// Unlike the W3c1 single-transaction commands, the claimed receipt STAYS
    /// `pending` while Keychain + descriptor commit outside SQLite — the
    /// pending receipt is the recovery protocol, not a claim of impossible
    /// cross-store atomicity. `Store::login_receipts` +
    /// `finalize_login_receipt`/`fail_login_receipt` close the loop.
    pub fn login_claim_receipt(
        &self,
        command_id: &str,
        request_digest: &str,
        request_json: &str,
    ) -> StoreResult<LoginClaim> {
        validate_command_identity(command_id, request_digest, request_json)?;
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        // Replay/mismatch preflight: committed -> replay, digest mismatch or
        // recorded failure -> typed error, pending/absent -> fall through.
        if let Some(response) = lookup_command_response::<LoginReceiptResponse>(
            &transaction,
            command_id,
            LOGIN_METHOD,
            request_digest,
            request_json,
            "account-login",
        )? {
            transaction.commit().map_err(map_sqlite_error)?;
            return Ok(LoginClaim::Committed(Box::new(response)));
        }
        let existed = transaction
            .query_row(
                "SELECT 1 FROM command_receipts WHERE command_id = ?1",
                [command_id],
                |_| Ok(()),
            )
            .optional()
            .map_err(map_sqlite_error)?
            .is_some();
        claim_pending_receipt(
            &transaction,
            command_id,
            LOGIN_METHOD,
            request_digest,
            request_json,
            now_ms()?,
        )?;
        transaction.commit().map_err(map_sqlite_error)?;
        if existed {
            // A pending row from a crashed or retryable earlier attempt: the
            // caller reconciles vault/descriptor state before revalidating.
            Ok(LoginClaim::ResumePending)
        } else {
            Ok(LoginClaim::Fresh)
        }
    }

    /// Finalizes a committed login (transaction B): the descriptor is the
    /// durable response a same-command retry replays. Receipt metadata NEVER
    /// contains the secret.
    pub fn finalize_login_receipt(
        &self,
        command_id: &str,
        response: &LoginReceiptResponse,
    ) -> StoreResult<u64> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        let revision = finalize_management_command_receipt(
            &transaction,
            command_id,
            LOGIN_METHOD,
            "",
            None,
            None,
            response,
            now_ms()?,
            "account-login",
            true,
        )?;
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(revision)
    }

    /// Records a DEFINITIVE login failure (401/403): nothing else persists,
    /// and a same-command retry is answered from this terminal record.
    pub fn fail_login_receipt(
        &self,
        command_id: &str,
        failure: &LoginReceiptFailure,
    ) -> StoreResult<()> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        fail_command_receipt(
            &transaction,
            command_id,
            failure,
            now_ms()?,
            "account-login",
        )?;
        transaction.commit().map_err(map_sqlite_error)
    }

    /// Every pending/committed login receipt, for the `run_inner` startup
    /// reconciliation phase (R10 step 10). Failed receipts are terminal and
    /// need no reconciliation.
    pub fn login_receipts(&self) -> StoreResult<Vec<LoginReceiptRow>> {
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT command_id, state, request_json, response_json, final_revision
                 FROM command_receipts
                 WHERE method = ?1 AND state IN ('pending', 'committed')
                 ORDER BY created_at_ms, command_id",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map([LOGIN_METHOD], |row| {
                let final_revision = row.get::<_, Option<i64>>(4)?.map(sql_u64).transpose()?;
                Ok(LoginReceiptRow {
                    command_id: row.get(0)?,
                    state: row.get(1)?,
                    request_json: row.get(2)?,
                    response_json: row.get(3)?,
                    final_revision,
                })
            })
            .map_err(map_sqlite_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_sqlite_error)?;
        Ok(rows)
    }

    /// Claims a durable OAuth `account.add` without ever recording its
    /// ephemeral ready reference or token bundle.
    pub fn account_add_claim_receipt(
        &self,
        command_id: &str,
        request_digest: &str,
        request_json: &str,
    ) -> StoreResult<AccountAddClaim> {
        validate_command_identity(command_id, request_digest, request_json)?;
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        if let Some(response) = lookup_command_response::<AccountAddReceiptResponse>(
            &transaction,
            command_id,
            ACCOUNT_ADD_METHOD,
            request_digest,
            request_json,
            "account-add",
        )? {
            transaction.commit().map_err(map_sqlite_error)?;
            return Ok(AccountAddClaim::Committed(Box::new(response)));
        }
        let existed = transaction
            .query_row(
                "SELECT 1 FROM command_receipts WHERE command_id = ?1",
                [command_id],
                |_| Ok(()),
            )
            .optional()
            .map_err(map_sqlite_error)?
            .is_some();
        claim_pending_receipt(
            &transaction,
            command_id,
            ACCOUNT_ADD_METHOD,
            request_digest,
            request_json,
            now_ms()?,
        )?;
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(if existed {
            AccountAddClaim::ResumePending
        } else {
            AccountAddClaim::Fresh
        })
    }

    pub fn finalize_account_add_receipt(
        &self,
        command_id: &str,
        response: &AccountAddReceiptResponse,
    ) -> StoreResult<u64> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        let revision = finalize_management_command_receipt(
            &transaction,
            command_id,
            ACCOUNT_ADD_METHOD,
            "",
            None,
            None,
            response,
            now_ms()?,
            "account-add",
            true,
        )?;
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(revision)
    }

    pub fn account_add_receipts(&self) -> StoreResult<Vec<AccountAddReceiptRow>> {
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT command_id, state, request_json, response_json, final_revision
                 FROM command_receipts
                 WHERE method = ?1 AND state IN ('pending', 'committed')
                 ORDER BY created_at_ms, command_id",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map([ACCOUNT_ADD_METHOD], |row| {
                let final_revision = row.get::<_, Option<i64>>(4)?.map(sql_u64).transpose()?;
                Ok(AccountAddReceiptRow {
                    command_id: row.get(0)?,
                    state: row.get(1)?,
                    request_json: row.get(2)?,
                    response_json: row.get(3)?,
                    final_revision,
                })
            })
            .map_err(map_sqlite_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_sqlite_error)?;
        Ok(rows)
    }

    /// Claims a W5 account/provider mutation after checking receipt replay
    /// and, only for a genuinely new command, the optional expected revision.
    ///
    /// `recovery_json` contains public, server-derived coordinates needed to
    /// finish a pending command after a crash. It is never part of semantic
    /// command identity and must never contain secret material. A
    /// provider-configure semantic no-op embeds its public response there so
    /// claim and current-revision finalization share this one transaction.
    pub fn management_claim_receipt<T>(
        &self,
        command_id: &str,
        method: &str,
        request_digest: &str,
        request_json: &str,
        recovery_json: Option<&str>,
        expected_revision: Option<u64>,
    ) -> StoreResult<ManagementClaim<T>>
    where
        T: serde::de::DeserializeOwned,
    {
        if !matches!(
            method,
            ACCOUNT_SET_ACTIVE_METHOD
                | ACCOUNT_SET_DEFAULT_MODEL_METHOD
                | PROVIDER_CONFIGURE_METHOD
                | PROVIDER_REMOVE_METHOD
        ) {
            return Err(store_error(
                ErrorCode::InvalidArgument,
                format!("method `{method}` is not a generic management mutation"),
                false,
            ));
        }
        validate_command_identity(command_id, request_digest, request_json)?;
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        let mut claim = claim_management_receipt_in_transaction(
            &transaction,
            command_id,
            method,
            request_digest,
            request_json,
            recovery_json,
            expected_revision,
        )?;
        if method == PROVIDER_CONFIGURE_METHOD {
            let authoritative_recovery = match &claim {
                ManagementClaim::Fresh => recovery_json,
                ManagementClaim::ResumePending { recovery_json } => recovery_json.as_deref(),
                ManagementClaim::Committed { .. } => None,
            };
            if let Some((response_json, response)) =
                provider_configure_noop_response(authoritative_recovery)?
            {
                let revision = finalize_management_command_receipt(
                    &transaction,
                    command_id,
                    method,
                    "",
                    None,
                    None,
                    &response_json,
                    now_ms()?,
                    "provider-configure no-op",
                    false,
                )?;
                claim = ManagementClaim::Committed {
                    response: Box::new(response),
                    revision,
                };
            }
        }
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(claim)
    }

    /// Atomically applies one digest-pinned hook trust mutation and commits
    /// its R2 response in the same transaction. There is no external side
    /// effect and therefore no durable pending recovery state.
    pub fn apply_hook_trust_command(
        &self,
        command: &HookTrustCommand,
    ) -> StoreResult<HookTrustChange> {
        validate_command_identity(
            &command.command_id,
            &command.request_digest,
            &command.request_json,
        )?;
        let method = if command.trusted {
            HOOKS_TRUST_METHOD
        } else {
            HOOKS_REVOKE_METHOD
        };
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        if let Some(response) = lookup_command_response::<HookTrustChange>(
            &transaction,
            &command.command_id,
            method,
            &command.request_digest,
            &command.request_json,
            "hook trust mutation",
        )? {
            transaction.commit().map_err(map_sqlite_error)?;
            return Ok(response);
        }
        claim_pending_receipt(
            &transaction,
            &command.command_id,
            method,
            &command.request_digest,
            &command.request_json,
            now_ms()?,
        )?;
        let committed: i64 = transaction
            .query_row(
                "SELECT COUNT(*) FROM command_receipts
                 WHERE state = 'committed' AND method IN (?1, ?2)",
                params![HOOKS_TRUST_METHOD, HOOKS_REVOKE_METHOD],
                |row| row.get(0),
            )
            .map_err(map_sqlite_error)?;
        let revision = u64::try_from(committed)
            .map_err(|_| corrupt("hook trust receipt count is negative"))?
            .checked_add(1)
            .ok_or_else(|| corrupt("hook trust revision space is exhausted"))?;
        let response = HookTrustChange {
            digest: command.digest.clone(),
            trusted: command.trusted,
            revision,
            workspace: command.workspace.clone(),
        };
        finalize_command_receipt(
            &transaction,
            &command.command_id,
            "",
            None,
            None,
            &response,
            now_ms()?,
            "hook trust mutation",
        )?;
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(response)
    }

    /// Reduces committed hook trust/revoke receipts in commit order. A later
    /// row for the same digest wins; unrelated management revisions are not
    /// touched by this separate trust domain.
    pub fn hook_trust_changes(&self) -> StoreResult<Vec<HookTrustChange>> {
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT response_json FROM command_receipts
                 WHERE state = 'committed' AND method IN (?1, ?2)
                 ORDER BY updated_at_ms, rowid",
            )
            .map_err(map_sqlite_error)?;
        statement
            .query_map(params![HOOKS_TRUST_METHOD, HOOKS_REVOKE_METHOD], |row| {
                row.get::<_, String>(0)
            })
            .map_err(map_sqlite_error)?
            .map(|json| {
                let json = json.map_err(map_sqlite_error)?;
                serde_json::from_str(&json).map_err(|error| {
                    corrupt(format!("committed hook trust response is invalid: {error}"))
                })
            })
            .collect()
    }

    /// Reads committed facts whose post-commit hook dispatch has not yet
    /// completed. The outbox row is inserted in the same transaction as the
    /// event, so recovery can distinguish durable truth from an accepted but
    /// uncommitted attempt without relying on live publication.
    pub fn pending_hook_dispatches(&self, limit: usize) -> StoreResult<Vec<RawEnvelope>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let connection = self.connection()?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut statement = connection
            .prepare_cached(
                "SELECT e.session_id, e.seq, e.envelope_json
                 FROM hook_dispatch_outbox AS o
                 JOIN events AS e
                   ON e.session_id = o.session_id AND e.seq = o.seq
                 ORDER BY e.committed_at_ms ASC, e.session_id ASC, e.seq ASC
                 LIMIT ?1",
            )
            .map_err(map_sqlite_error)?;
        let mut rows = statement.query([limit]).map_err(map_sqlite_error)?;
        let mut envelopes = Vec::new();
        while let Some(row) = rows.next().map_err(map_sqlite_error)? {
            let session_id: String = row.get(0).map_err(map_sqlite_error)?;
            let seq: i64 = row.get(1).map_err(map_sqlite_error)?;
            let envelope = decode_envelope_column(row, 2).map_err(|error| {
                corrupt(format!(
                    "invalid hook-outbox envelope for session {session_id}, seq {seq}: {error}"
                ))
            })?;
            let stored_seq = u64::try_from(seq)
                .map_err(|_| corrupt("hook outbox contains a negative event sequence"))?;
            if envelope.session_id.as_str() != session_id || envelope.seq != stored_seq {
                return Err(corrupt(
                    "hook outbox coordinates disagree with the authoritative envelope",
                ));
            }
            envelopes.push(envelope);
        }
        Ok(envelopes)
    }

    /// Idempotently acknowledges one recovered/live hook-dispatch row after
    /// all matching hooks have handled the committed envelope.
    pub fn complete_hook_dispatch(&self, session_id: &SessionId, seq: u64) -> StoreResult<()> {
        let connection = self.connection()?;
        connection
            .execute(
                "DELETE FROM hook_dispatch_outbox WHERE session_id = ?1 AND seq = ?2",
                params![session_id.as_str(), to_sqlite_integer(seq)?],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    /// Idempotently acknowledges one drain cycle's handled hook-dispatch rows
    /// in a single durable transaction (one fsync instead of one per event).
    ///
    /// All-or-nothing: either every listed row is deleted or none are, so a
    /// crash between the event commit and this acknowledgement leaves exactly
    /// the unacknowledged completions in the outbox for at-least-once replay.
    pub fn complete_hook_dispatches(&self, acks: &[(SessionId, u64)]) -> StoreResult<()> {
        if acks.is_empty() {
            return Ok(());
        }
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        {
            let mut statement = transaction
                .prepare_cached(
                    "DELETE FROM hook_dispatch_outbox WHERE session_id = ?1 AND seq = ?2",
                )
                .map_err(map_sqlite_error)?;
            for (session_id, seq) in acks {
                statement
                    .execute(params![session_id.as_str(), to_sqlite_integer(*seq)?])
                    .map_err(map_sqlite_error)?;
            }
        }
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(())
    }

    /// Read-only replay/pending preflight used before server-derived resource
    /// validation. `None` means the command id is genuinely new.
    pub fn management_receipt_preflight<T>(
        &self,
        command_id: &str,
        method: &str,
        request_digest: &str,
        request_json: &str,
    ) -> StoreResult<Option<ManagementClaim<T>>>
    where
        T: serde::de::DeserializeOwned,
    {
        if !is_management_method(method) {
            return Err(store_error(
                ErrorCode::InvalidArgument,
                format!("method `{method}` is not a management receipt"),
                false,
            ));
        }
        validate_command_identity(command_id, request_digest, request_json)?;
        let connection = self.connection()?;
        if let Some(response) = lookup_command_response::<T>(
            &connection,
            command_id,
            method,
            request_digest,
            request_json,
            "management mutation",
        )? {
            let revision: Option<i64> = connection
                .query_row(
                    "SELECT final_revision FROM command_receipts WHERE command_id = ?1",
                    [command_id],
                    |row| row.get(0),
                )
                .map_err(map_sqlite_error)?;
            let revision = revision
                .ok_or_else(|| corrupt("committed management receipt has no final revision"))
                .and_then(|revision| {
                    u64::try_from(revision)
                        .map_err(|_| corrupt("management receipt has a negative final revision"))
                })?;
            return Ok(Some(ManagementClaim::Committed {
                response: Box::new(response),
                revision,
            }));
        }
        let recovery_json = connection
            .query_row(
                "SELECT recovery_json FROM command_receipts WHERE command_id = ?1",
                [command_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(map_sqlite_error)?;
        Ok(recovery_json.map(|recovery_json| ManagementClaim::ResumePending { recovery_json }))
    }

    /// Atomically claims both the durable remove receipt and the alias
    /// reservation that fences concurrent/restarted account creation.
    #[allow(clippy::too_many_arguments)]
    pub fn account_remove_claim_receipt<T>(
        &self,
        command_id: &str,
        request_digest: &str,
        request_json: &str,
        recovery_json: &str,
        expected_revision: Option<u64>,
        alias: &str,
        provider: &str,
        was_active: bool,
    ) -> StoreResult<ManagementClaim<T>>
    where
        T: serde::de::DeserializeOwned,
    {
        validate_command_identity(command_id, request_digest, request_json)?;
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        let claim = claim_management_receipt_in_transaction(
            &transaction,
            command_id,
            ACCOUNT_REMOVE_METHOD,
            request_digest,
            request_json,
            Some(recovery_json),
            expected_revision,
        )?;
        if !matches!(claim, ManagementClaim::Committed { .. }) {
            transaction
                .execute(
                    "INSERT OR IGNORE INTO account_alias_reservations(
                        alias, command_id, provider, was_active, created_at_ms
                     ) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        alias,
                        command_id,
                        provider,
                        i64::from(was_active),
                        to_sqlite_integer(now_ms()?)?,
                    ],
                )
                .map_err(map_sqlite_error)?;
            let reservation: Option<(String, String, i64)> = transaction
                .query_row(
                    "SELECT command_id, provider, was_active
                     FROM account_alias_reservations WHERE alias = ?1",
                    [alias],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()
                .map_err(map_sqlite_error)?;
            if reservation.as_ref()
                != Some(&(
                    command_id.to_owned(),
                    provider.to_owned(),
                    i64::from(was_active),
                ))
            {
                return Err(store_error(
                    ErrorCode::Busy,
                    format!("credential alias `{alias}` is reserved by pending removal cleanup"),
                    true,
                ));
            }
        }
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(claim)
    }

    /// Finalizes a generic W5 management receipt and records its revision in
    /// the same SQLite transaction. Provider-configure receipts may carry the
    /// additive `revision_unchanged: true` response marker for a semantic
    /// no-op; those commit at the current revision instead of advancing it.
    pub fn finalize_management_receipt<T: serde::Serialize>(
        &self,
        command_id: &str,
        method: &str,
        response: &T,
    ) -> StoreResult<u64> {
        if !matches!(
            method,
            ACCOUNT_SET_ACTIVE_METHOD
                | ACCOUNT_SET_DEFAULT_MODEL_METHOD
                | PROVIDER_CONFIGURE_METHOD
        ) {
            return Err(store_error(
                ErrorCode::InvalidArgument,
                format!("method `{method}` is not a generic management mutation"),
                false,
            ));
        }
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        let advance_revision = if method == PROVIDER_CONFIGURE_METHOD {
            let response = serde_json::to_value(response).map_err(|error| {
                store_error(
                    ErrorCode::InvalidArgument,
                    format!("cannot serialize management mutation response: {error}"),
                    false,
                )
            })?;
            response
                .get("revision_unchanged")
                .and_then(serde_json::Value::as_bool)
                != Some(true)
        } else {
            true
        };
        let revision = finalize_management_command_receipt(
            &transaction,
            command_id,
            method,
            "",
            None,
            None,
            response,
            now_ms()?,
            "management mutation",
            advance_revision,
        )?;
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(revision)
    }

    /// Finalizes remove and releases its alias reservation atomically with the
    /// receipt and management-revision commit.
    pub fn finalize_account_remove_receipt<T: serde::Serialize>(
        &self,
        command_id: &str,
        response: &T,
    ) -> StoreResult<u64> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        let revision = finalize_management_command_receipt(
            &transaction,
            command_id,
            ACCOUNT_REMOVE_METHOD,
            "",
            None,
            None,
            response,
            now_ms()?,
            "account-remove",
            true,
        )?;
        let released = transaction
            .execute(
                "DELETE FROM account_alias_reservations WHERE command_id = ?1",
                [command_id],
            )
            .map_err(map_sqlite_error)?;
        if released != 1 {
            return Err(corrupt(
                "account-remove finalizer did not release exactly one alias reservation",
            ));
        }
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(revision)
    }

    /// Finalizes provider removal, deletes its durable discovered-model cache,
    /// and allocates the management revision in one SQLite transaction.
    pub fn finalize_provider_remove_receipt<T: serde::Serialize>(
        &self,
        command_id: &str,
        provider: &str,
        response: &T,
    ) -> StoreResult<u64> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        let revision = finalize_management_command_receipt(
            &transaction,
            command_id,
            PROVIDER_REMOVE_METHOD,
            "",
            None,
            None,
            response,
            now_ms()?,
            "provider-remove",
            true,
        )?;
        transaction
            .execute(
                "DELETE FROM provider_models WHERE provider = ?1",
                [provider],
            )
            .map_err(map_sqlite_error)?;
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(revision)
    }

    pub fn management_receipts(&self, method: &str) -> StoreResult<Vec<ManagementReceiptRow>> {
        if !is_management_method(method) {
            return Err(store_error(
                ErrorCode::InvalidArgument,
                format!("method `{method}` is not a management receipt"),
                false,
            ));
        }
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT command_id, method, state, request_json, recovery_json,
                        response_json, final_revision
                 FROM command_receipts
                 WHERE method = ?1 AND state IN ('pending', 'committed')
                 ORDER BY created_at_ms, command_id",
            )
            .map_err(map_sqlite_error)?;
        statement
            .query_map([method], management_receipt_row)
            .map_err(map_sqlite_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_sqlite_error)
    }

    /// All provider-profile mutation receipts in durable creation order.
    /// Interleaving the method families is required so a later remove beats an
    /// older pending configure without making removal a permanent tombstone.
    pub fn provider_management_receipts(&self) -> StoreResult<Vec<ManagementReceiptRow>> {
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT command_id, method, state, request_json, recovery_json,
                        response_json, final_revision
                 FROM command_receipts
                 WHERE method IN (?1, ?2, ?3)
                   AND state IN ('pending', 'committed')
                 ORDER BY created_at_ms, rowid",
            )
            .map_err(map_sqlite_error)?;
        statement
            .query_map(
                params![
                    ACCOUNT_SET_DEFAULT_MODEL_METHOD,
                    PROVIDER_CONFIGURE_METHOD,
                    PROVIDER_REMOVE_METHOD
                ],
                management_receipt_row,
            )
            .map_err(map_sqlite_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_sqlite_error)
    }

    /// Pending removals and their durable reservations, used before Ready.
    pub fn account_remove_receipts(&self) -> StoreResult<Vec<AccountRemoveReceiptRow>> {
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT r.command_id, r.method, r.state, r.request_json,
                        r.recovery_json, r.response_json, r.final_revision,
                        a.alias, a.provider, a.was_active
                 FROM command_receipts r
                 JOIN account_alias_reservations a ON a.command_id = r.command_id
                 WHERE r.method = ?1 AND r.state = 'pending'
                 ORDER BY r.created_at_ms, r.command_id",
            )
            .map_err(map_sqlite_error)?;
        statement
            .query_map([ACCOUNT_REMOVE_METHOD], |row| {
                Ok(AccountRemoveReceiptRow {
                    receipt: ManagementReceiptRow {
                        command_id: row.get(0)?,
                        method: row.get(1)?,
                        state: row.get(2)?,
                        request_json: row.get(3)?,
                        recovery_json: row.get(4)?,
                        response_json: row.get(5)?,
                        final_revision: row.get::<_, Option<i64>>(6)?.map(sql_u64).transpose()?,
                    },
                    alias: row.get(7)?,
                    provider: row.get(8)?,
                    was_active: row.get::<_, i64>(9)? != 0,
                })
            })
            .map_err(map_sqlite_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_sqlite_error)
    }

    pub fn reserved_account_aliases(&self) -> StoreResult<Vec<String>> {
        let connection = self.connection()?;
        let mut statement = connection
            .prepare("SELECT alias FROM account_alias_reservations ORDER BY alias")
            .map_err(map_sqlite_error)?;
        statement
            .query_map([], |row| row.get(0))
            .map_err(map_sqlite_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_sqlite_error)
    }

    /// Gives one already-committed management receipt its missing final
    /// revision. This is the pre-ready migration/reconciliation seam for
    /// receipts written by a daemon predating schema v6.
    pub fn ensure_committed_management_revision(
        &self,
        command_id: &str,
        method: &str,
    ) -> StoreResult<u64> {
        if !is_management_method(method) {
            return Err(store_error(
                ErrorCode::InvalidArgument,
                format!("method `{method}` is not a management receipt"),
                false,
            ));
        }
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        let (stored_method, state, final_revision): (String, String, Option<i64>) = transaction
            .query_row(
                "SELECT method, state, final_revision
                 FROM command_receipts
                 WHERE command_id = ?1",
                [command_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map_err(map_sqlite_error)?;
        if stored_method != method || state != "committed" {
            return Err(corrupt(format!(
                "management receipt `{command_id}` is not a committed `{method}` receipt"
            )));
        }
        let revision = if let Some(revision) = final_revision {
            u64::try_from(revision)
                .map_err(|_| corrupt("database contains a negative management revision"))?
        } else {
            let revision = next_management_revision_in_transaction(&transaction)?;
            let updated = transaction
                .execute(
                    "UPDATE command_receipts
                     SET final_revision = ?2
                     WHERE command_id = ?1 AND method = ?3
                       AND state = 'committed' AND final_revision IS NULL",
                    params![command_id, to_sqlite_integer(revision)?, method],
                )
                .map_err(map_sqlite_error)?;
            if updated != 1 {
                return Err(corrupt(
                    "committed management receipt lost its missing-revision claim",
                ));
            }
            revision
        };
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(revision)
    }

    /// Appends aggregate `Idle` only if every durable run is terminal at the
    /// same serialized SQLite write point.
    ///
    /// `interrupted: true` means user cancellation, drain-caused cancellation,
    /// recovery, panic, or failed recovery resumption. Natural `Done` and
    /// ordinary provider/error completion are `false`; merely being in drain
    /// does not rewrite a naturally completed turn's cause.
    ///
    /// This is the aggregate-state half of R3. A worker may observe its local
    /// queue as empty while a concurrent submit is already durable; checking
    /// the journal inside an IMMEDIATE transaction prevents a later false
    /// `Idle` from overwriting that submit's `ActiveRun`.
    pub fn settle_session_idle(&self, envelope: &mut RawEnvelope) -> StoreResult<bool> {
        if envelope.worker_generation != self.worker_generation {
            return Err(stale_generation(
                envelope.worker_generation,
                self.worker_generation,
            ));
        }
        if !matches!(
            serde_json::from_value::<EventPayload>(envelope.payload.clone()),
            Ok(EventPayload::SessionState(SessionState::Idle { .. }))
        ) {
            return Err(store_error(
                ErrorCode::InvalidArgument,
                "conditional aggregate settlement requires a SessionState::Idle envelope",
                false,
            ));
        }
        let session_id = envelope.session_id.clone();
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        require_session(&transaction, &session_id)?;
        if has_nonterminal_run(&transaction, &session_id)? {
            transaction.commit().map_err(map_sqlite_error)?;
            return Ok(false);
        }
        let mut stamped = [envelope.clone()];
        append_transaction_envelopes(&transaction, &session_id, now_ms()?, &mut stamped)?;
        transaction.commit().map_err(map_sqlite_error)?;
        *envelope = stamped[0].clone();
        Ok(true)
    }

    /// Checkpoints committed WAL pages before orderly profile close.
    ///
    /// W3b1 seam (additive), used by the daemon drain barrier. Under the
    /// default `NORMAL` policy an OS crash can lose the most recent checkpoint
    /// window; this orderly checkpoint persists all committed WAL pages and
    /// shrinks the WAL a successor must replay. A busy checkpoint surfaces as
    /// retryable `StoreLocked`.
    pub fn flush(&self) -> StoreResult<()> {
        let connection = self.connection()?;
        let (busy, _, _): (u32, u32, u32) = connection
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .map_err(map_sqlite_error)?;
        if busy != 0 {
            return Err(store_error(
                ErrorCode::StoreLocked,
                "SQLite WAL checkpoint could not acquire the required lock",
                true,
            ));
        }
        Ok(())
    }

    /// The profile's content-addressed storage.
    pub fn cas(&self) -> &FileCas {
        &self.cas
    }

    /// Current supported and migrated SQLite schema version.
    pub fn schema_version(&self) -> StoreResult<u32> {
        let connection = self.connection()?;
        let version = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(map_sqlite_error)?;
        Ok(version)
    }

    /// Atomically resolves one durable menu and appends its authoritative
    /// `MenuAnswered` envelope.
    ///
    /// ARBITRATION LAW (authoritative statement — daemon callers refer here):
    /// the first COMMITTED answer wins, decided entirely inside one immediate
    /// SQLite transaction. A retry of the same `command_id` gets
    /// [`MenuResolutionOutcome::IdempotentReplay`] with the original
    /// sequence; a different command after any resolution gets
    /// [`MenuResolutionOutcome::AlreadyResolved`] carrying the winner's
    /// `resolution_seq`; `worker_generation` identifies and fences the
    /// durable menu OPENING, while a post-restart answer is stamped with the
    /// current store generation. The same-command idempotency lookup
    /// deliberately precedes the fence, because a lost-response retry must
    /// recover its own committed coordinate even across a restart's new
    /// generation. Every attachment then learns the outcome from the event
    /// stream — the journal, not any caller's reply, is the source of truth.
    ///
    /// `menu_resolutions` is only a uniqueness/idempotency index; historical
    /// journals are scanned so a pre-index `MenuAnswered` still fences a
    /// later answer.
    pub fn resolve_menu(
        &self,
        command: &MenuResolutionCommand,
    ) -> StoreResult<MenuResolutionOutcome> {
        if command.command_id.is_empty() {
            return Err(store_error(
                ErrorCode::InvalidArgument,
                "menu command id must not be empty",
                false,
            ));
        }
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        let outcome =
            resolve_menu_transaction(&transaction, &self.cas, command, self.worker_generation)?;
        transaction.commit().map_err(map_sqlite_error)?;
        if let MenuResolutionOutcome::Committed {
            envelope,
            follow_up,
            ..
        } = &outcome
        {
            let mut graph_events = Vec::with_capacity(1 + follow_up.len());
            graph_events.push(envelope.as_ref().clone());
            graph_events.extend(follow_up.iter().cloned());
            self.extend_graph_reduction(&connection, &command.session_id, &graph_events);
        }
        Ok(outcome)
    }

    /// Reads one true-weight-budgeted replay page: committed envelopes with
    /// `seq > since_seq` in sequence order.
    ///
    /// Additive daemon seam (like [`Self::resolve_menu`]): an envelope-count
    /// limit alone cannot bound the transient memory. The exact page bound is
    /// `byte_budget + one maximally-sized committed row` in true-weight units:
    /// retained rows stop at the budget, while one candidate row may be
    /// materialized to identify the cut-off. A non-empty result always
    /// contains at least one envelope even when that first row exceeds the
    /// budget, and stops immediately afterward. That one-row progress
    /// guarantee keeps a byte-paged reader from stalling; it is also why the
    /// extra row must be stated explicitly. The next page resumes from the
    /// caller's last-received sequence (keyset, no prefix re-read).
    pub fn read_page(
        &self,
        session: &SessionId,
        since_seq: u64,
        max_envelopes: usize,
        byte_budget: usize,
    ) -> StoreResult<Vec<RawEnvelope>> {
        if max_envelopes == 0 {
            return Ok(Vec::new());
        }
        let connection = self.connection()?;
        // A limit beyond i64::MAX is effectively unbounded; clamp, don't error.
        let limit = i64::try_from(max_envelopes).unwrap_or(i64::MAX);
        let mut statement = connection
            .prepare_cached(
                "SELECT seq, envelope_json, event_id, committed_at_ms
                 FROM events
                 WHERE session_id = ?1 AND seq > ?2
                 ORDER BY seq ASC
                 LIMIT ?3",
            )
            .map_err(map_sqlite_error)?;
        let mut rows = statement
            .query(params![
                session.as_str(),
                to_sqlite_integer(since_seq)?,
                limit
            ])
            .map_err(map_sqlite_error)?;
        let mut envelopes = Vec::new();
        let mut spent = 0_usize;
        while let Some(row) = rows.next().map_err(map_sqlite_error)? {
            let stored_seq: i64 = row.get(0).map_err(map_sqlite_error)?;
            let stored_event_id: String = row.get(2).map_err(map_sqlite_error)?;
            let stored_committed_at_ms: i64 = row.get(3).map_err(map_sqlite_error)?;
            let envelope = decode_envelope_column(row, 1).map_err(|error| {
                corrupt(format!(
                    "invalid envelope for session {session}, seq {stored_seq}: {error}"
                ))
            })?;
            validate_stored_envelope(
                session,
                stored_seq,
                &stored_event_id,
                stored_committed_at_ms,
                &envelope,
            )?;
            let weight = envelope_weight_bytes(&envelope);
            if !envelopes.is_empty() && spent.saturating_add(weight) > byte_budget {
                break;
            }
            spent = spent.saturating_add(weight);
            envelopes.push(envelope);
            if spent >= byte_budget {
                break;
            }
        }
        Ok(envelopes)
    }

    /// Reads one sequence-ordered page for a reducer that declares the outer
    /// payload kinds it consumes.
    ///
    /// The denormalized `payload_kind` predicate is evaluated before SQLite
    /// materializes `envelope_json`, so unrelated envelopes are not decoded.
    /// A `NULL` kind remains eligible for journals written before the kind
    /// backfill. If an eligible envelope itself cannot be decoded, the read is
    /// retried through [`EventStore::read`], preserving the historical full
    /// scan's corruption behavior instead of turning an optimization into a
    /// new failure surface.
    pub fn read_reducer_page(
        &self,
        session: &SessionId,
        since_seq: u64,
        limit: usize,
        byte_budget: usize,
        payload_kinds: &[&str],
    ) -> StoreResult<Vec<RawEnvelope>> {
        if limit == 0 || payload_kinds.is_empty() {
            return Ok(Vec::new());
        }
        let connection = self.connection()?;
        let filtered = read_reducer_page_with_connection(
            &connection,
            session,
            since_seq,
            limit,
            byte_budget,
            payload_kinds,
        );
        drop(connection);
        match filtered {
            Ok(envelopes) => Ok(envelopes),
            Err(FilteredReadError::Decode) => {
                self.read_page(session, since_seq, limit, byte_budget)
            }
            Err(FilteredReadError::Store(error)) => Err(error),
        }
    }

    /// Byte-bounded variant used by durable streamed reducers. The journal
    /// boundary is sampled while the same connection lock guards the filtered
    /// read, so callers may checkpoint an empty filtered suffix without
    /// racing a committed append whose relevant fact was not folded.
    pub fn read_reducer_page_with_boundary(
        &self,
        session: &SessionId,
        since_seq: u64,
        limit: usize,
        byte_budget: usize,
        payload_kinds: &[&str],
    ) -> StoreResult<ReducerPage> {
        if limit == 0 || payload_kinds.is_empty() {
            return Ok(ReducerPage {
                envelopes: Vec::new(),
                observed_head: None,
            });
        }
        let connection = self.connection()?;
        let boundary = journal_boundary_with_connection(&connection, session)?;
        let filtered = read_reducer_page_with_connection(
            &connection,
            session,
            since_seq,
            limit,
            byte_budget,
            payload_kinds,
        );
        drop(connection);
        match filtered {
            Ok(envelopes) => Ok(ReducerPage {
                envelopes,
                observed_head: boundary,
            }),
            Err(FilteredReadError::Decode) => self
                .read_page(session, since_seq, limit, byte_budget)
                .map(|envelopes| ReducerPage {
                    envelopes,
                    observed_head: None,
                }),
            Err(FilteredReadError::Store(error)) => Err(error),
        }
    }

    /// Replays a session's complete journal in committed sequence order.
    pub fn journal_replay(&self, session: &SessionId) -> StoreResult<Vec<RawEnvelope>> {
        let mut replay = Vec::new();
        let mut since_seq = 0;
        loop {
            let page = self.read(session, since_seq, REPLAY_PAGE_SIZE)?;
            if page.is_empty() {
                return Ok(replay);
            }
            since_seq = page.last().map_or(since_seq, |envelope| envelope.seq);
            replay.extend(page);
        }
    }

    fn connection(&self) -> StoreResult<MutexGuard<'_, Connection>> {
        self.connection.lock().map_err(|_| {
            store_error(
                ErrorCode::Internal,
                "SQLite journal connection lock is poisoned",
                false,
            )
        })
    }

    #[allow(clippy::result_large_err)]
    fn graph_reductions(
        &self,
        connection: &Connection,
        session_id: &SessionId,
    ) -> StoreResult<GraphReductions> {
        if let Some(reductions) = self
            .graph_reductions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(session_id)
            .map(|cached| cached.reductions.clone())
        {
            return Ok(reductions);
        }
        let envelopes = load_graph_reduction_envelopes(connection, session_id)?;
        let reductions = reduce_graphs(&envelopes);
        self.graph_reductions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                session_id.clone(),
                CachedGraphReduction {
                    envelopes,
                    reductions: reductions.clone(),
                },
            );
        Ok(reductions)
    }

    fn extend_graph_reduction(
        &self,
        connection: &Connection,
        session_id: &SessionId,
        envelopes: &[RawEnvelope],
    ) {
        let mut graph_reductions = self
            .graph_reductions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(cached) = graph_reductions.get_mut(session_id) {
            cached.envelopes.extend(
                envelopes
                    .iter()
                    .filter(|envelope| graph_reduction_event(&envelope.payload))
                    .cloned(),
            );
            cached.reductions = reduce_graphs(&cached.envelopes);
        }
        drop(graph_reductions);
        self.extend_graph_telemetry(connection, session_id, envelopes);
    }

    fn extend_graph_telemetry(
        &self,
        connection: &Connection,
        session_id: &SessionId,
        envelopes: &[RawEnvelope],
    ) {
        let telemetry_envelopes = envelopes
            .iter()
            .filter(|envelope| graph_telemetry_event(&envelope.payload))
            .cloned()
            .collect::<Vec<_>>();
        if telemetry_envelopes.is_empty() {
            return;
        }
        let mut telemetry = self
            .graph_telemetry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cached = telemetry
            .by_session
            .entry(session_id.clone())
            .or_insert_with(|| CachedSessionGraphTelemetry {
                through_seq: 0,
                accumulator: GraphTelemetryAccumulator::default(),
                projection: GraphTelemetryProjection::default(),
            });
        for envelope in &telemetry_envelopes {
            cached.accumulator.apply(envelope);
        }
        cached.projection = cached.accumulator.projection();
        cached.through_seq = cached.through_seq.max(
            telemetry_envelopes
                .last()
                .map_or(0, |envelope| envelope.seq),
        );
        let through_seq = cached.through_seq;
        let accumulator = cached.accumulator.clone();
        let projection = cached.projection.clone();
        drop(telemetry);
        let _ = persist_graph_telemetry_projection(
            connection,
            session_id,
            through_seq,
            &accumulator,
            &projection,
        );
    }

    fn invalidate_graph_reduction(&self, session_id: &SessionId) {
        self.graph_reductions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(session_id);
        self.graph_telemetry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .by_session
            .remove(session_id);
    }
}

fn validate_command_identity(
    command_id: &str,
    request_digest: &str,
    request_json: &str,
) -> StoreResult<()> {
    if command_id.is_empty() || request_digest.is_empty() || request_json.is_empty() {
        return Err(store_error(
            ErrorCode::InvalidArgument,
            "command id, request digest, and canonical request JSON must not be empty",
            false,
        ));
    }
    serde_json::from_str::<serde_json::Value>(request_json).map_err(|error| {
        store_error(
            ErrorCode::InvalidArgument,
            format!("canonical request JSON is invalid: {error}"),
            false,
        )
    })?;
    Ok(())
}

fn validate_monitor_control_method(method: &str) -> StoreResult<()> {
    if matches!(method, "monitor.register" | "monitor.remove") {
        Ok(())
    } else {
        Err(store_error(
            ErrorCode::InvalidArgument,
            "monitor control receipts accept only monitor.register or monitor.remove",
            false,
        ))
    }
}

#[allow(clippy::too_many_arguments)]
fn claim_management_receipt_in_transaction<T>(
    transaction: &Transaction<'_>,
    command_id: &str,
    method: &str,
    request_digest: &str,
    request_json: &str,
    recovery_json: Option<&str>,
    expected_revision: Option<u64>,
) -> StoreResult<ManagementClaim<T>>
where
    T: serde::de::DeserializeOwned,
{
    if let Some(response) = lookup_command_response::<T>(
        transaction,
        command_id,
        method,
        request_digest,
        request_json,
        "management mutation",
    )? {
        let revision: Option<i64> = transaction
            .query_row(
                "SELECT final_revision FROM command_receipts WHERE command_id = ?1",
                [command_id],
                |row| row.get(0),
            )
            .map_err(map_sqlite_error)?;
        let revision = revision
            .ok_or_else(|| corrupt("committed management receipt has no final revision"))
            .and_then(|revision| {
                u64::try_from(revision)
                    .map_err(|_| corrupt("management receipt has a negative final revision"))
            })?;
        return Ok(ManagementClaim::Committed {
            response: Box::new(response),
            revision,
        });
    }

    let existing_recovery: Option<Option<String>> = transaction
        .query_row(
            "SELECT recovery_json FROM command_receipts WHERE command_id = ?1",
            [command_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(map_sqlite_error)?;
    if existing_recovery.is_none()
        && let Some(expected_revision) = expected_revision
    {
        let current: i64 = transaction
            .query_row(
                "SELECT management_revision FROM profile_meta WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .map_err(map_sqlite_error)?;
        let current_revision = u64::try_from(current)
            .map_err(|_| corrupt("database contains a negative management revision"))?;
        if expected_revision != current_revision {
            let mut error = store_error(
                ErrorCode::RevisionConflict,
                format!(
                    "expected management revision {expected_revision}, current revision is {current_revision}"
                ),
                true,
            );
            error.details = Some(serde_json::json!({
                "expected_revision": expected_revision,
                "current_revision": current_revision,
            }));
            return Err(error);
        }
    }

    claim_pending_receipt(
        transaction,
        command_id,
        method,
        request_digest,
        request_json,
        now_ms()?,
    )?;
    if existing_recovery.is_none() {
        transaction
            .execute(
                "UPDATE command_receipts SET recovery_json = ?2
                 WHERE command_id = ?1 AND state = 'pending'",
                params![command_id, recovery_json],
            )
            .map_err(map_sqlite_error)?;
        Ok(ManagementClaim::Fresh)
    } else {
        Ok(ManagementClaim::ResumePending {
            recovery_json: existing_recovery.flatten(),
        })
    }
}

fn provider_configure_noop_response<T>(
    recovery_json: Option<&str>,
) -> StoreResult<Option<(serde_json::Value, T)>>
where
    T: serde::de::DeserializeOwned,
{
    let Some(recovery_json) = recovery_json else {
        return Ok(None);
    };
    let recovery: serde_json::Value = serde_json::from_str(recovery_json).map_err(|error| {
        corrupt(format!(
            "provider-configure recovery JSON is invalid: {error}"
        ))
    })?;
    let Some(response_json) = recovery
        .get(PROVIDER_CONFIGURE_NOOP_RESPONSE_FIELD)
        .cloned()
    else {
        return Ok(None);
    };
    let response = serde_json::from_value(response_json.clone()).map_err(|error| {
        corrupt(format!(
            "provider-configure no-op response is invalid: {error}"
        ))
    })?;
    Ok(Some((response_json, response)))
}

fn management_receipt_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ManagementReceiptRow> {
    Ok(ManagementReceiptRow {
        command_id: row.get(0)?,
        method: row.get(1)?,
        state: row.get(2)?,
        request_json: row.get(3)?,
        recovery_json: row.get(4)?,
        response_json: row.get(5)?,
        final_revision: row.get::<_, Option<i64>>(6)?.map(sql_u64).transpose()?,
    })
}

fn lookup_session_create_receipt(
    connection: &Connection,
    command_id: &str,
    request_digest: &str,
    request_json: &str,
) -> StoreResult<Option<CreatedSession>> {
    let row = connection
        .query_row(
            "SELECT method, request_digest, request_json, state,
                    session_id, accepted_seq, response_json
             FROM command_receipts
             WHERE command_id = ?1",
            [command_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<i64>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                ))
            },
        )
        .optional()
        .map_err(map_sqlite_error)?;
    let Some((method, stored_digest, stored_json, state, session_id, accepted_seq, response_json)) =
        row
    else {
        return Ok(None);
    };
    if method != "session.create" || stored_digest != request_digest || stored_json != request_json
    {
        return Err(store_error(
            ErrorCode::InvalidArgument,
            "command id was already used with a different method or semantic request body",
            false,
        ));
    }
    match state.as_str() {
        "pending" => Ok(None),
        "committed" => {
            let response_json = response_json.ok_or_else(|| {
                corrupt("committed session-create receipt is missing response JSON")
            })?;
            let created: CreatedSession =
                serde_json::from_str(&response_json).map_err(|error| {
                    corrupt(format!(
                        "committed session-create response JSON is invalid: {error}"
                    ))
                })?;
            let stored_session = session_id.ok_or_else(|| {
                corrupt("committed session-create receipt is missing its session id")
            })?;
            let stored_seq = accepted_seq
                .ok_or_else(|| corrupt("committed session-create receipt is missing its sequence"))
                .and_then(|value| {
                    u64::try_from(value)
                        .map_err(|_| corrupt("command receipt contains a negative sequence"))
                })?;
            if created.session_id.as_str() != stored_session
                || created.created_seq != stored_seq
                || created.created_seq != 1
            {
                return Err(corrupt(
                    "session-create receipt response disagrees with indexed coordinates",
                ));
            }
            Ok(Some(created))
        }
        "failed" => Err(store_error(
            ErrorCode::InvalidArgument,
            "session-create command is already recorded as failed",
            false,
        )),
        other => Err(corrupt(format!(
            "command receipt has unknown state {other}"
        ))),
    }
}

fn lookup_turn_accept_receipt(
    connection: &Connection,
    command_id: &str,
    request_digest: &str,
    request_json: &str,
) -> StoreResult<Option<AcceptedTurn>> {
    lookup_command_response(
        connection,
        command_id,
        "turn.submit",
        request_digest,
        request_json,
        "turn-submit",
    )
}

fn lookup_turn_cancel_receipt(
    connection: &Connection,
    command_id: &str,
    request_digest: &str,
    request_json: &str,
) -> StoreResult<Option<CancelledTurn>> {
    lookup_command_response(
        connection,
        command_id,
        "turn.cancel",
        request_digest,
        request_json,
        "turn-cancel",
    )
}

fn lookup_command_response<T>(
    connection: &Connection,
    command_id: &str,
    expected_method: &str,
    request_digest: &str,
    request_json: &str,
    description: &str,
) -> StoreResult<Option<T>>
where
    T: serde::de::DeserializeOwned,
{
    let row = connection
        .query_row(
            "SELECT method, request_digest, request_json, state, response_json
             FROM command_receipts WHERE command_id = ?1",
            [command_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            },
        )
        .optional()
        .map_err(map_sqlite_error)?;
    let Some((method, stored_digest, stored_json, state, response_json)) = row else {
        return Ok(None);
    };
    if method != expected_method || stored_digest != request_digest || stored_json != request_json {
        return Err(store_error(
            ErrorCode::InvalidArgument,
            "command id was already used with a different method or semantic request body",
            false,
        ));
    }
    match state.as_str() {
        "pending" => Ok(None),
        "committed" => {
            let response = response_json.ok_or_else(|| {
                corrupt(format!("committed {description} receipt has no response"))
            })?;
            serde_json::from_str(&response).map(Some).map_err(|error| {
                corrupt(format!(
                    "committed {description} response is invalid: {error}"
                ))
            })
        }
        "failed" => Err(store_error(
            ErrorCode::InvalidArgument,
            format!("{description} command is already recorded as failed"),
            false,
        )),
        other => Err(corrupt(format!(
            "{description} receipt has unknown state {other}"
        ))),
    }
}

fn require_session(connection: &Connection, session_id: &SessionId) -> StoreResult<()> {
    let exists = connection
        .query_row(
            "SELECT 1 FROM sessions WHERE id = ?1",
            [session_id.as_str()],
            |_| Ok(()),
        )
        .optional()
        .map_err(map_sqlite_error)?
        .is_some();
    if exists {
        Ok(())
    } else {
        Err(store_error(
            ErrorCode::SessionNotFound,
            format!("session {session_id} does not exist"),
            false,
        ))
    }
}

fn require_typed_session(connection: &Connection, session_id: &SessionId) -> StoreResult<()> {
    require_session(connection, session_id)?;
    let metadata: String = connection
        .query_row(
            "SELECT meta_json FROM sessions WHERE id = ?1",
            [session_id.as_str()],
            |row| row.get(0),
        )
        .map_err(map_sqlite_error)?;
    if decode_session_metadata(session_id, &metadata)?.is_none() {
        return Err(store_error(
            ErrorCode::InvalidArgument,
            "legacy session has no live-worker metadata",
            false,
        ));
    }
    Ok(())
}

fn latest_seq_in_connection(connection: &Connection, session_id: &SessionId) -> StoreResult<u64> {
    let latest: i64 = connection
        .query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM events WHERE session_id = ?1",
            [session_id.as_str()],
            |row| row.get(0),
        )
        .map_err(map_sqlite_error)?;
    u64::try_from(latest).map_err(|_| corrupt("database contains a negative event sequence"))
}

#[allow(clippy::result_large_err)]
fn graph_evidence_provenance(
    connection: &Connection,
    session_id: &SessionId,
    graph_id: &GraphId,
    reduction: &GraphReduction,
    after_seq: u64,
    through_seq: u64,
    limit: usize,
) -> StoreResult<Vec<GraphEvidenceProvenanceRow>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let mut statement = connection
        .prepare_cached(
            "SELECT envelope_json FROM events
             WHERE session_id = ?1
               AND seq > ?2
               AND seq <= ?3
               AND (
                   payload_kind = 'evidence_recorded'
                   OR (
                       payload_kind IS NULL
                       AND instr(envelope_json, '\"type\":\"evidence_recorded\"') > 0
                       AND json_extract(envelope_json, '$.payload.graph_id') = ?4
                   )
               )
             ORDER BY seq ASC",
        )
        .map_err(map_sqlite_error)?;
    let mut rows = statement
        .query(params![
            session_id.as_str(),
            to_sqlite_integer(after_seq)?,
            to_sqlite_integer(through_seq)?,
            graph_id.as_str(),
        ])
        .map_err(map_sqlite_error)?;
    let mut recorded = Vec::<(RawEnvelope, EvidenceRecorded)>::new();
    while let Some(row) = rows.next().map_err(map_sqlite_error)? {
        let envelope = decode_envelope_column(row, 0).map_err(|error| {
            corrupt(format!(
                "invalid graph provenance envelope in session {session_id}: {error}"
            ))
        })?;
        match decode_payload::<EventPayload>(&envelope.payload) {
            Ok(EventPayload::EvidenceRecorded(evidence)) if evidence.graph_id == *graph_id => {
                recorded.push((envelope, evidence));
                if recorded.len() == limit {
                    break;
                }
            }
            _ => {}
        }
    }
    let required_signals = recorded
        .iter()
        .filter_map(|(_, evidence)| match &evidence.source {
            GraphEvidenceSource::ProcessSignal {
                run_id,
                call_id,
                effect_id,
            } => Some((run_id.clone(), call_id.clone(), effect_id.clone())),
            GraphEvidenceSource::Model { .. }
            | GraphEvidenceSource::WorkspaceMutation { .. }
            | GraphEvidenceSource::ComputerObservation { .. }
            | GraphEvidenceSource::ChildContract { .. } => None,
        })
        .collect::<HashSet<_>>();
    let mut signals = HashMap::<(RunId, String, EffectId), ProcessSignalRecorded>::new();
    if !required_signals.is_empty() {
        let mut signal_statement = connection
            .prepare_cached(
                "SELECT envelope_json FROM events
                 WHERE session_id = ?1
                   AND seq <= ?2
                   AND (
                       payload_kind = 'process_signal_recorded'
                       OR (
                           payload_kind IS NULL
                           AND instr(envelope_json, '\"type\":\"process_signal_recorded\"') > 0
                       )
                   )
                 ORDER BY seq ASC",
            )
            .map_err(map_sqlite_error)?;
        let mut rows = signal_statement
            .query(params![
                session_id.as_str(),
                to_sqlite_integer(through_seq)?
            ])
            .map_err(map_sqlite_error)?;
        while let Some(row) = rows.next().map_err(map_sqlite_error)? {
            let envelope = decode_envelope_column(row, 0).map_err(|error| {
                corrupt(format!(
                    "invalid graph signal provenance envelope in session {session_id}: {error}"
                ))
            })?;
            if let Ok(EventPayload::ProcessSignalRecorded(signal)) =
                serde_json::from_value::<EventPayload>(envelope.payload)
            {
                let key = (
                    signal.run_id.clone(),
                    signal.call_id.clone(),
                    signal.effect_id.clone(),
                );
                if required_signals.contains(&key) {
                    signals.insert(key, signal);
                    if signals.len() == required_signals.len() {
                        break;
                    }
                }
            }
        }
    }
    let required_mutations = recorded
        .iter()
        .filter_map(|(_, evidence)| match &evidence.source {
            GraphEvidenceSource::WorkspaceMutation { run_id, effect_id } => {
                Some((run_id.clone(), effect_id.clone()))
            }
            _ => None,
        })
        .collect::<HashSet<_>>();
    let mut mutations = HashMap::<(RunId, EffectId), GraphWorkspaceMutationProvenance>::new();
    if !required_mutations.is_empty() {
        let mut mutation_statement = connection
            .prepare_cached(
                "SELECT envelope_json FROM events
                 WHERE session_id = ?1
                   AND seq <= ?2
                   AND (
                       payload_kind IN ('effect', 'task_completed')
                       OR (
                           payload_kind IS NULL
                           AND instr(envelope_json, '\"workspace_mutation\"') > 0
                       )
                   )
                 ORDER BY seq ASC",
            )
            .map_err(map_sqlite_error)?;
        let mut rows = mutation_statement
            .query(params![
                session_id.as_str(),
                to_sqlite_integer(through_seq)?
            ])
            .map_err(map_sqlite_error)?;
        while let Some(row) = rows.next().map_err(map_sqlite_error)? {
            let envelope = decode_envelope_column(row, 0).map_err(|error| {
                corrupt(format!(
                    "invalid graph mutation provenance envelope in session {session_id}: {error}"
                ))
            })?;
            let Some(run_id) = envelope.run_id else {
                continue;
            };
            let Ok(EventPayload::Effect(EffectPhase::Outcome {
                effect,
                workspace_mutation: Some(mutation),
                ..
            })) = serde_json::from_value::<EventPayload>(envelope.payload)
            else {
                continue;
            };
            let key = (run_id, effect.clone());
            if !required_mutations.contains(&key) {
                continue;
            }
            let (Some(workspace_revision), Some(subject_digest)) =
                (mutation.workspace_revision, mutation.subject_digest)
            else {
                continue;
            };
            mutations.insert(
                key,
                GraphWorkspaceMutationProvenance {
                    effect_id: effect,
                    mutation_digest: mutation.mutation_digest,
                    workspace_revision,
                    subject_digest,
                },
            );
        }
    }
    recorded
        .into_iter()
        .map(|(envelope, evidence)| {
            let slot_spec = reduction
                .template_nodes
                .iter()
                .find(|spec| spec.name == evidence.node)
                .and_then(|spec| {
                    evidence.slot.as_deref().and_then(|slot_id| {
                        spec.verify_slots.iter().find(|slot| slot.id == slot_id)
                    })
                });
            let signal = match &evidence.source {
                GraphEvidenceSource::ProcessSignal {
                    run_id,
                    call_id,
                    effect_id,
                } => signals
                    .get(&(run_id.clone(), call_id.clone(), effect_id.clone()))
                    .map(|signal| GraphSignalProvenance {
                        command_arg_digest: signal.command_arg_digest.clone(),
                        exit_code: signal.exit_code,
                        transcript_digest: signal.transcript_digest.clone(),
                        workspace_revision: signal.workspace_revision.clone(),
                        subject_digest: signal.subject_digest.clone(),
                        artifact: signal.artifact.clone(),
                    }),
                GraphEvidenceSource::Model { .. }
                | GraphEvidenceSource::WorkspaceMutation { .. }
                | GraphEvidenceSource::ComputerObservation { .. }
                | GraphEvidenceSource::ChildContract { .. } => None,
            };
            let workspace_mutation = match &evidence.source {
                GraphEvidenceSource::WorkspaceMutation { run_id, effect_id } => {
                    mutations.get(&(run_id.clone(), effect_id.clone())).cloned()
                }
                _ => None,
            };
            let computer_observation = matches!(
                &evidence.source,
                GraphEvidenceSource::ComputerObservation { .. }
            );
            Ok(GraphEvidenceProvenanceRow {
                seq: envelope.seq,
                committed_at_ms: envelope.committed_at_ms,
                graph_id: evidence.graph_id,
                node: evidence.node,
                attempt: evidence.attempt,
                slot: evidence.slot,
                authority: if computer_observation {
                    EvidenceAuthority::DaemonVerified
                } else {
                    slot_spec.map_or(EvidenceAuthority::ModelAttested, |slot| slot.authority)
                },
                subject_selector: if computer_observation {
                    Some(SubjectSelector::WorkspaceRevision)
                } else {
                    slot_spec.map(|slot| slot.subject_selector)
                },
                verdict: evidence.verdict,
                fingerprint: evidence.fingerprint,
                subject_digest: evidence.subject_digest,
                source: evidence.source,
                signal,
                workspace_mutation,
            })
        })
        .collect()
}

fn load_graph_reduction(
    connection: &Connection,
    session_id: &SessionId,
) -> StoreResult<GraphReduction> {
    Ok(
        reduce_graphs(&load_graph_reduction_envelopes(connection, session_id)?)
            .active()
            .cloned()
            .unwrap_or_default(),
    )
}

fn load_graph_reductions(
    connection: &Connection,
    session_id: &SessionId,
) -> StoreResult<GraphReductions> {
    Ok(reduce_graphs(&load_graph_reduction_envelopes(
        connection, session_id,
    )?))
}

fn load_graph_reduction_envelopes(
    connection: &Connection,
    session_id: &SessionId,
) -> StoreResult<Vec<RawEnvelope>> {
    // The indexed outer payload kind is a stable marker. Prefix matching may
    // over-select a future graph/menu variant, which the tolerant reducer
    // ignores, but it cannot omit a current graph-reduction input.
    let mut statement = connection
        .prepare_cached(
            "SELECT envelope_json FROM events
             WHERE session_id = ?1
               AND (
                   (payload_kind >= 'graph_' AND payload_kind < 'graph`')
                   OR payload_kind = 'todo_graph_attached'
                   OR payload_kind = 'evidence_recorded'
                   OR (payload_kind >= 'menu_' AND payload_kind < 'menu`')
               )
             ORDER BY seq ASC",
        )
        .map_err(map_sqlite_error)?;
    let mut rows = statement
        .query([session_id.as_str()])
        .map_err(map_sqlite_error)?;
    let mut envelopes = Vec::new();
    while let Some(row) = rows.next().map_err(map_sqlite_error)? {
        envelopes.push(decode_envelope_column(row, 0).map_err(|error| {
            corrupt(format!(
                "invalid graph-reduction envelope in session {session_id}: {error}"
            ))
        })?);
    }
    Ok(envelopes)
}

/// Historical activation translation needs the first real workflow input in
/// addition to legacy graph-reducer facts. Ordinary graph reads keep their
/// narrower index scan; this is used only by the one-time migration.
fn load_workflow_backfill_envelopes(
    connection: &Connection,
    session_id: &SessionId,
) -> StoreResult<Vec<RawEnvelope>> {
    let mut statement = connection
        .prepare_cached(
            "SELECT envelope_json FROM events
             WHERE session_id = ?1
               AND (
                   (payload_kind >= 'graph_' AND payload_kind < 'graph`')
                   OR payload_kind = 'todo_graph_attached'
                   OR payload_kind = 'evidence_recorded'
                   OR payload_kind = 'user_message'
                   OR (payload_kind >= 'menu_' AND payload_kind < 'menu`')
               )
             ORDER BY seq ASC",
        )
        .map_err(map_sqlite_error)?;
    let mut rows = statement
        .query([session_id.as_str()])
        .map_err(map_sqlite_error)?;
    let mut envelopes = Vec::new();
    while let Some(row) = rows.next().map_err(map_sqlite_error)? {
        envelopes.push(decode_envelope_column(row, 0).map_err(|error| {
            corrupt(format!(
                "invalid workflow-backfill envelope in session {session_id}: {error}"
            ))
        })?);
    }
    Ok(envelopes)
}

fn backfill_payload_kinds(connection: &mut Connection) -> StoreResult<()> {
    let decoded = {
        let mut statement = connection
            .prepare(
                "SELECT rowid, envelope_json FROM events
                 WHERE payload_kind IS NULL OR payload_kind = 'item_legacy'",
            )
            .map_err(map_sqlite_error)?;
        let mut rows = statement.query([]).map_err(map_sqlite_error)?;
        let mut decoded = Vec::new();
        while let Some(row) = rows.next().map_err(map_sqlite_error)? {
            let rowid: i64 = row.get(0).map_err(map_sqlite_error)?;
            let envelope = decode_envelope_column(row, 1).map_err(|error| {
                corrupt(format!(
                    "invalid legacy envelope during payload-kind backfill: {error}"
                ))
            })?;
            let telemetry_session = graph_telemetry_event(&envelope.payload)
                .then(|| envelope.session_id.as_str().to_owned());
            decoded.push((rowid, payload_kind(&envelope).to_owned(), telemetry_session));
        }
        decoded
    };
    if decoded.is_empty() {
        return Ok(());
    }
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sqlite_error)?;
    {
        let mut update = transaction
            .prepare("UPDATE events SET payload_kind = ?2 WHERE rowid = ?1")
            .map_err(map_sqlite_error)?;
        let mut dirty_sessions = HashSet::new();
        for (rowid, kind, telemetry_session) in decoded {
            update
                .execute(params![rowid, kind])
                .map_err(map_sqlite_error)?;
            if let Some(session_id) = telemetry_session {
                dirty_sessions.insert(session_id);
            }
        }
        drop(update);
        for session_id in dirty_sessions {
            transaction
                .execute(
                    "INSERT OR IGNORE INTO graph_telemetry_dirty(session_id) VALUES (?1)",
                    [session_id],
                )
                .map_err(map_sqlite_error)?;
        }
    }
    transaction.commit().map_err(map_sqlite_error)
}

fn telemetry_payload_predicate() -> &'static str {
    "((payload_kind >= 'graph_' AND payload_kind < 'graph`')
       OR (payload_kind >= 'menu_' AND payload_kind < 'menu`')
       OR payload_kind IN ('todo_graph_attached', 'evidence_recorded', 'item_tool_call', 'tool_result'))"
}

fn load_graph_telemetry_envelopes(
    connection: &Connection,
    session_id: &SessionId,
) -> StoreResult<Vec<RawEnvelope>> {
    let sql = format!(
        "SELECT envelope_json FROM events WHERE session_id = ?1 AND {} ORDER BY seq ASC",
        telemetry_payload_predicate()
    );
    let mut statement = connection.prepare(&sql).map_err(map_sqlite_error)?;
    let mut rows = statement
        .query([session_id.as_str()])
        .map_err(map_sqlite_error)?;
    let mut envelopes = Vec::new();
    while let Some(row) = rows.next().map_err(map_sqlite_error)? {
        envelopes.push(decode_envelope_column(row, 0).map_err(|error| {
            corrupt(format!(
                "invalid graph telemetry envelope in session {session_id}: {error}"
            ))
        })?);
    }
    Ok(envelopes)
}

fn persist_graph_telemetry_projection(
    connection: &Connection,
    session_id: &SessionId,
    through_seq: u64,
    accumulator: &GraphTelemetryAccumulator,
    projection: &GraphTelemetryProjection,
) -> StoreResult<()> {
    if through_seq == 0 {
        return Ok(());
    }
    let accumulator = rmp_serde::to_vec_named(accumulator).map_err(|error| {
        store_error(
            ErrorCode::Internal,
            format!("cannot encode graph telemetry continuation state: {error}"),
            false,
        )
    })?;
    let projection = rmp_serde::to_vec_named(projection).map_err(|error| {
        store_error(
            ErrorCode::Internal,
            format!("cannot encode graph telemetry projection: {error}"),
            false,
        )
    })?;
    connection
        .execute(
            "INSERT INTO graph_telemetry_projection(
                session_id, through_seq, reducer_version, tool_state, projection
             ) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(session_id) DO UPDATE SET
                through_seq = excluded.through_seq,
                reducer_version = excluded.reducer_version,
                tool_state = excluded.tool_state,
                projection = excluded.projection",
            params![
                session_id.as_str(),
                to_sqlite_integer(through_seq)?,
                GRAPH_TELEMETRY_REDUCER_VERSION,
                accumulator,
                projection,
            ],
        )
        .map_err(map_sqlite_error)?;
    connection
        .execute(
            "DELETE FROM graph_telemetry_dirty WHERE session_id = ?1",
            [session_id.as_str()],
        )
        .map_err(map_sqlite_error)?;
    Ok(())
}

fn rebuild_graph_telemetry_cache(connection: &Connection) -> StoreResult<GraphTelemetryCache> {
    let mut persisted = HashMap::<SessionId, CachedSessionGraphTelemetry>::new();
    let mut rebuild_sessions = HashSet::<SessionId>::new();
    let mut statement = connection
        .prepare(
            "SELECT session_id, through_seq, reducer_version, tool_state, projection
             FROM graph_telemetry_projection ORDER BY session_id ASC",
        )
        .map_err(map_sqlite_error)?;
    let mut rows = statement.query([]).map_err(map_sqlite_error)?;
    while let Some(row) = rows.next().map_err(map_sqlite_error)? {
        let session_id = SessionId::new(row.get::<_, String>(0).map_err(map_sqlite_error)?);
        let through_seq = u64::try_from(row.get::<_, i64>(1).map_err(map_sqlite_error)?)
            .map_err(|_| corrupt("negative telemetry projection head"))?;
        let version: u32 = row.get(2).map_err(map_sqlite_error)?;
        if version != GRAPH_TELEMETRY_REDUCER_VERSION {
            rebuild_sessions.insert(session_id);
            continue;
        }
        let tool_bytes: Vec<u8> = row.get(3).map_err(map_sqlite_error)?;
        let projection_bytes: Vec<u8> = row.get(4).map_err(map_sqlite_error)?;
        let (Ok(accumulator), Ok(projection)) = (
            rmp_serde::from_slice::<GraphTelemetryAccumulator>(&tool_bytes),
            rmp_serde::from_slice::<GraphTelemetryProjection>(&projection_bytes),
        ) else {
            rebuild_sessions.insert(session_id);
            continue;
        };
        persisted.insert(
            session_id,
            CachedSessionGraphTelemetry {
                through_seq,
                accumulator,
                projection,
            },
        );
    }
    drop(rows);
    drop(statement);

    let mut statement = connection
        .prepare("SELECT session_id FROM graph_telemetry_dirty ORDER BY session_id ASC")
        .map_err(map_sqlite_error)?;
    let dirty = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(map_sqlite_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(map_sqlite_error)?;
    rebuild_sessions.extend(dirty.into_iter().map(SessionId::new));
    let mut by_session = persisted;
    for session_id in rebuild_sessions {
        by_session.remove(&session_id);
        let envelopes = load_graph_telemetry_envelopes(connection, &session_id)?;
        let Some(last) = envelopes.last() else {
            connection
                .execute(
                    "DELETE FROM graph_telemetry_dirty WHERE session_id = ?1",
                    [session_id.as_str()],
                )
                .map_err(map_sqlite_error)?;
            continue;
        };
        let through_seq = last.seq;
        let mut accumulator = GraphTelemetryAccumulator::default();
        for envelope in &envelopes {
            accumulator.apply(envelope);
        }
        let projection = accumulator.projection();
        persist_graph_telemetry_projection(
            connection,
            &session_id,
            through_seq,
            &accumulator,
            &projection,
        )?;
        by_session.insert(
            session_id,
            CachedSessionGraphTelemetry {
                through_seq,
                accumulator,
                projection,
            },
        );
    }
    Ok(GraphTelemetryCache { by_session })
}

trait GraphCommandCoordinates {
    fn command_id(&self) -> &str;
    fn session_id(&self) -> &SessionId;
    fn worker_generation(&self) -> u64;
    fn device_id(&self) -> &DeviceId;
}

impl GraphCommandCoordinates for GraphPinCommand {
    fn command_id(&self) -> &str {
        &self.command_id
    }
    fn session_id(&self) -> &SessionId {
        &self.session_id
    }
    fn worker_generation(&self) -> u64 {
        self.worker_generation
    }
    fn device_id(&self) -> &DeviceId {
        &self.device_id
    }
}

impl GraphCommandCoordinates for ChildGraphAttachCommand {
    fn command_id(&self) -> &str {
        &self.command_id
    }
    fn session_id(&self) -> &SessionId {
        &self.session_id
    }
    fn worker_generation(&self) -> u64 {
        self.worker_generation
    }
    fn device_id(&self) -> &DeviceId {
        &self.device_id
    }
}

impl GraphCommandCoordinates for GraphRunSetOpenCommand {
    fn command_id(&self) -> &str {
        &self.command_id
    }
    fn session_id(&self) -> &SessionId {
        &self.session_id
    }
    fn worker_generation(&self) -> u64 {
        self.worker_generation
    }
    fn device_id(&self) -> &DeviceId {
        &self.device_id
    }
}

impl GraphCommandCoordinates for GraphSwitchCommand {
    fn command_id(&self) -> &str {
        &self.command_id
    }
    fn session_id(&self) -> &SessionId {
        &self.session_id
    }
    fn worker_generation(&self) -> u64 {
        self.worker_generation
    }
    fn device_id(&self) -> &DeviceId {
        &self.device_id
    }
}

impl GraphCommandCoordinates for GraphAbandonCommand {
    fn command_id(&self) -> &str {
        &self.command_id
    }
    fn session_id(&self) -> &SessionId {
        &self.session_id
    }
    fn worker_generation(&self) -> u64 {
        self.worker_generation
    }
    fn device_id(&self) -> &DeviceId {
        &self.device_id
    }
}

impl GraphCommandCoordinates for GraphEvidenceCommand {
    fn command_id(&self) -> &str {
        &self.command_id
    }
    fn session_id(&self) -> &SessionId {
        &self.session_id
    }
    fn worker_generation(&self) -> u64 {
        self.worker_generation
    }
    fn device_id(&self) -> &DeviceId {
        &self.device_id
    }
}

impl GraphCommandCoordinates for ComputerEvidenceCommand {
    fn command_id(&self) -> &str {
        &self.command_id
    }
    fn session_id(&self) -> &SessionId {
        &self.session_id
    }
    fn worker_generation(&self) -> u64 {
        self.worker_generation
    }
    fn device_id(&self) -> &DeviceId {
        &self.device_id
    }
}

fn graph_command_envelopes(
    command: &impl GraphCommandCoordinates,
    payloads: Vec<EventPayload>,
) -> StoreResult<Vec<RawEnvelope>> {
    payloads
        .into_iter()
        .enumerate()
        .map(|(index, payload)| {
            let mut hasher = blake3::Hasher::new();
            hasher.update(command.session_id().as_str().as_bytes());
            hasher.update(&[0]);
            hasher.update(command.command_id().as_bytes());
            hasher.update(&[0]);
            hasher.update(&u64::try_from(index).unwrap_or(u64::MAX).to_be_bytes());
            unstamped_command_envelope(
                EventId::new(format!("graph-fact-{}", hasher.finalize().to_hex())),
                command.session_id(),
                None,
                None,
                command.device_id().clone(),
                command.worker_generation(),
                payload,
                PromptRender::Omit,
            )
        })
        .collect()
}

#[allow(clippy::result_large_err)]
fn plan_items_from_event(
    envelope: &RawEnvelope,
    expected_item_id: &ItemId,
) -> StoreResult<Vec<TodoItem>> {
    let payload = decode_payload::<EventPayload>(&envelope.payload).map_err(|_| {
        store_error(
            ErrorCode::InvalidArgument,
            "requested Plan coordinate is not a typed event",
            false,
        )
    })?;
    match payload {
        EventPayload::Item(
            ItemEvent::Started {
                item_id,
                item: TurnItem::Plan { items },
            }
            | ItemEvent::Completed {
                item_id,
                item: TurnItem::Plan { items },
            },
        ) if item_id == *expected_item_id => Ok(items),
        _ => Err(store_error(
            ErrorCode::InvalidArgument,
            format!(
                "event {} is not Plan item {}",
                envelope.seq, expected_item_id
            ),
            false,
        )),
    }
}

#[allow(clippy::result_large_err)]
fn validate_todo_plan_items(items: &[TodoItem]) -> StoreResult<()> {
    if items.is_empty() || items.len() > GRAPH_MAX_TODO_CHILDREN {
        return Err(store_error(
            ErrorCode::InvalidArgument,
            format!(
                "a graph run-set requires 1..={GRAPH_MAX_TODO_CHILDREN} todos, got {}",
                items.len()
            ),
            false,
        ));
    }
    let ids = items.iter().map(|todo| todo.id).collect::<HashSet<_>>();
    if ids.len() != items.len() {
        return Err(store_error(
            ErrorCode::InvalidArgument,
            "a graph run-set Plan contains duplicate todo ids",
            false,
        ));
    }
    for todo in items {
        if todo
            .dep
            .is_some_and(|dependency| !ids.contains(&dependency))
        {
            return Err(store_error(
                ErrorCode::InvalidArgument,
                format!("todo {} depends on an unknown todo id", todo.id),
                false,
            ));
        }
        let mut dependency = todo.dep;
        let mut hops = 0_usize;
        while let Some(current) = dependency {
            if current == todo.id {
                return Err(store_error(
                    ErrorCode::InvalidArgument,
                    format!("todo dependency cycle reaches id {}", todo.id),
                    false,
                ));
            }
            hops = hops.saturating_add(1);
            if hops > items.len() {
                return Err(store_error(
                    ErrorCode::InvalidArgument,
                    "todo dependency chain exceeds the Plan bound",
                    false,
                ));
            }
            dependency = items
                .iter()
                .find(|candidate| candidate.id == current)
                .and_then(|candidate| candidate.dep);
        }
    }
    Ok(())
}

fn normalize_graph_why(why: &str) -> StoreResult<String> {
    let why = why.split_whitespace().collect::<Vec<_>>().join(" ");
    if why.is_empty() {
        return Err(store_error(
            ErrorCode::InvalidArgument,
            "graph abandon reason must not be empty",
            false,
        ));
    }
    let mut end = why.len().min(256);
    while end > 0 && !why.is_char_boundary(end) {
        end -= 1;
    }
    Ok(why[..end].to_owned())
}

fn graph_pending_menus(status: &GraphStatus) -> Vec<MenuId> {
    if status.pending_menus.is_empty() {
        status.pending_menu.iter().cloned().collect()
    } else {
        status.pending_menus.clone()
    }
}

/// Snapshot the unfinished child graph identities and menu ownership for the
/// active aggregate. Callers can then append a forest-wide terminal batch
/// without holding borrows into the reduction while constructing payloads.
fn active_unfinished_run_set_children(reductions: &GraphReductions) -> Vec<(GraphId, Vec<MenuId>)> {
    reductions
        .active_run_set
        .as_ref()
        .and_then(|run_set_id| reductions.run_sets.get(run_set_id))
        .into_iter()
        .flat_map(|run_set| run_set.children.iter())
        .filter(|child| {
            !matches!(
                child.phase,
                GraphPhase::Completed | GraphPhase::Abandoned | GraphPhase::Superseded
            )
        })
        .map(|child| {
            let menus = reductions
                .graph(&child.graph_id)
                .and_then(|reduction| reduction.status.as_ref())
                .map(graph_pending_menus)
                .unwrap_or_default();
            (child.graph_id.clone(), menus)
        })
        .collect()
}

fn graph_retry_menus(
    status: &GraphStatus,
    specs: &[haider_protocol::graph::GraphNodeSpec],
    target: &GraphNodeName,
) -> Vec<MenuId> {
    let invalidated = graph_descendants_inclusive(specs, target);
    let invalidated_menu_ids = specs
        .iter()
        .filter(|spec| {
            invalidated.contains(&spec.name) && matches!(spec.gate, GraphGateKind::HumanConfirm)
        })
        .filter_map(|spec| {
            status
                .nodes
                .iter()
                .find(|node| node.node == spec.name)
                .and_then(|node| node.current_attempt)
                .map(|attempt| graph_confirm_menu(status, &spec.name, attempt).id)
        })
        .collect::<HashSet<_>>();
    graph_pending_menus(status)
        .into_iter()
        .filter(|menu| invalidated_menu_ids.contains(menu))
        .collect()
}

fn graph_descendants_inclusive(
    specs: &[haider_protocol::graph::GraphNodeSpec],
    target: &GraphNodeName,
) -> HashSet<GraphNodeName> {
    let mut descendants = HashSet::from([target.clone()]);
    loop {
        let before = descendants.len();
        for spec in specs {
            if spec
                .depends_on
                .iter()
                .any(|dependency| descendants.contains(dependency))
            {
                descendants.insert(spec.name.clone());
            }
        }
        if descendants.len() == before {
            return descendants;
        }
    }
}

fn linear_template(reduction: &GraphReduction, status: &GraphStatus) -> bool {
    let Some(first) = reduction.template_nodes.first() else {
        return false;
    };
    let start = status.start_node.as_ref().unwrap_or(&first.name);
    &first.name == start
        && first.depends_on.is_empty()
        && reduction
            .template_nodes
            .windows(2)
            .all(|pair| pair[1].depends_on.as_slice() == std::slice::from_ref(&pair[0].name))
}

#[allow(clippy::result_large_err)]
fn dependency_followups(
    reduction: &GraphReduction,
    status: &GraphStatus,
    satisfied_node: &GraphNodeName,
    attempt: u32,
) -> StoreResult<Vec<EventPayload>> {
    // A targeted hop can leave an independent ready sibling on an older
    // node-local epoch. Newly unlocked nodes join the latest graph traversal
    // epoch; already-ready siblings must not be reopened or lose evidence.
    let opening_attempt = status.attempt.max(attempt);
    let satisfied = |name: &GraphNodeName| {
        name == satisfied_node
            || status
                .nodes
                .iter()
                .find(|node| &node.node == name)
                .is_some_and(|node| node.satisfied)
    };
    let unsatisfied = reduction
        .template_nodes
        .iter()
        .filter(|spec| !satisfied(&spec.name))
        .collect::<Vec<_>>();
    if unsatisfied.is_empty() {
        return Ok(vec![EventPayload::GraphCompleted(GraphCompleted {
            graph_id: status.graph_id.clone(),
        })]);
    }
    let ready = unsatisfied
        .into_iter()
        .filter(|spec| spec.depends_on.iter().all(&satisfied))
        .filter(|spec| !status.node_is_ready(&spec.name))
        .collect::<Vec<_>>();
    let linear = linear_template(reduction, status);
    let mut payloads = Vec::new();
    for spec in ready {
        if linear {
            payloads.push(EventPayload::GraphAdvanced(GraphAdvanced {
                graph_id: status.graph_id.clone(),
                from_node: satisfied_node.clone(),
                to_node: spec.name.clone(),
            }));
        } else {
            payloads.push(EventPayload::GraphNodeReadied(GraphNodeReadied {
                graph_id: status.graph_id.clone(),
                node: spec.name.clone(),
                attempt: opening_attempt,
            }));
        }
        payloads.push(EventPayload::GraphAttemptOpened(GraphAttemptOpened {
            graph_id: status.graph_id.clone(),
            node: spec.name.clone(),
            attempt: opening_attempt,
        }));
        if matches!(spec.gate, GraphGateKind::HumanConfirm) {
            payloads.push(EventPayload::MenuOpened(graph_confirm_menu(
                status,
                &spec.name,
                opening_attempt,
            )));
        }
    }
    Ok(payloads)
}

/// Collapses one completed todo child into its aggregate contract and opens
/// direct dependents by frozen todo id. Abandoned/superseded children count as
/// aggregate terminals in the reducer but never unlock work as if successful.
#[allow(clippy::result_large_err)]
fn todo_child_completed_followups(
    reductions: &GraphReductions,
    completed_graph_id: &GraphId,
) -> StoreResult<Vec<EventPayload>> {
    let Some(run_set_id) = reductions.active_run_set.as_ref() else {
        return Ok(Vec::new());
    };
    let Some(run_set) = reductions.run_sets.get(run_set_id) else {
        return Ok(Vec::new());
    };
    let Some(completed_child) = run_set
        .children
        .iter()
        .find(|child| &child.graph_id == completed_graph_id)
    else {
        return Ok(Vec::new());
    };
    let mut followups = Vec::new();
    let mut dependents = run_set
        .children
        .iter()
        .filter(|child| child.depends_on_todo_id == Some(completed_child.todo_id))
        .collect::<Vec<_>>();
    dependents.sort_by_key(|child| (child.ordinal, child.todo_id));
    for dependent in dependents {
        let reduction = reductions.graph(&dependent.graph_id).ok_or_else(|| {
            corrupt(format!(
                "todo child graph {} is absent from its run-set",
                dependent.graph_id
            ))
        })?;
        let status = reduction.status.as_ref().ok_or_else(|| {
            corrupt(format!(
                "todo child graph {} has no reduced status",
                dependent.graph_id
            ))
        })?;
        if status.phase != GraphPhase::Active || status.attempt != 0 {
            continue;
        }
        let start_node = status.start_node.clone().ok_or_else(|| {
            corrupt(format!(
                "todo child graph {} has no declared start node",
                dependent.graph_id
            ))
        })?;
        followups.push(EventPayload::GraphAttemptOpened(GraphAttemptOpened {
            graph_id: dependent.graph_id.clone(),
            node: start_node.clone(),
            attempt: 1,
        }));
        let human_start = reduction.template_nodes.iter().any(|spec| {
            spec.name == start_node && matches!(spec.gate, GraphGateKind::HumanConfirm)
        });
        if human_start {
            followups.push(EventPayload::MenuOpened(graph_confirm_menu(
                status,
                &start_node,
                1,
            )));
        }
    }
    let terminal_before = run_set
        .children
        .iter()
        .filter(|child| {
            matches!(
                child.phase,
                GraphPhase::Completed | GraphPhase::Abandoned | GraphPhase::Superseded
            )
        })
        .count();
    let completes_aggregate = terminal_before.saturating_add(1)
        >= usize::try_from(run_set.required_children).unwrap_or(usize::MAX);
    if completes_aggregate {
        followups.push(EventPayload::GraphCompleted(GraphCompleted {
            graph_id: run_set.root_graph_id.clone(),
        }));
    }
    Ok(followups)
}

fn graph_evidence_limit(spec: &haider_protocol::graph::GraphNodeSpec) -> StoreResult<u32> {
    spec.max_evidence_per_attempt.ok_or_else(|| {
        store_error(
            ErrorCode::StoreCorrupt,
            format!(
                "open graph node {} has no evidence-round bound",
                spec.name.label()
            ),
            false,
        )
    })
}

#[allow(clippy::result_large_err)]
fn validate_pinned_graph_template(
    template: &haider_protocol::graph::GraphTemplateSpec,
) -> StoreResult<()> {
    if let Err(error) = validate_graph_template(template) {
        let mut typed = store_error(ErrorCode::InvalidArgument, error.message, false);
        typed.details = Some(serde_json::json!({
            "kind": "malformed_graph_template",
            "rejection": error.kind,
        }));
        return Err(typed);
    }
    for node in &template.nodes {
        for slot in &node.verify_slots {
            if slot.authority == EvidenceAuthority::DaemonVerified
                && slot.subject_selector == SubjectSelector::Freeform
            {
                let mut typed = store_error(
                    ErrorCode::InvalidArgument,
                    format!(
                        "daemon-verified evidence slot `{}` cannot use a freeform subject",
                        slot.id
                    ),
                    false,
                );
                typed.details = Some(serde_json::json!({
                    "kind": "malformed_graph_template",
                    "rejection": GraphTemplateRejection::InvalidGate,
                }));
                return Err(typed);
            }
        }
    }
    Ok(())
}

#[allow(clippy::result_large_err)]
fn validate_graph_evidence_authority(
    transaction: &Transaction<'_>,
    session_id: &SessionId,
    command: &GraphEvidenceCommand,
    slot: Option<&EvidenceSlotSpec>,
    graph_id: &GraphId,
    attempt: u32,
) -> StoreResult<GraphEvidenceSource> {
    if let Some(contract) = command.child_contract.as_ref() {
        return validate_child_contract_authority(
            transaction,
            session_id,
            command,
            slot,
            graph_id,
            attempt,
            contract,
        );
    }
    let Some(slot) = slot else {
        if command.signal.is_some()
            || command.workspace_mutation.is_some()
            || command.subject_digest.is_some()
        {
            return Err(graph_evidence_error(
                ErrorCode::InvalidArgument,
                "wrong_evidence_authority",
                "legacy un-slotted evidence must use model testimony",
            ));
        }
        return Ok(GraphEvidenceSource::Model {
            run_id: command.run_id.clone(),
            call_id: command.call_id.clone(),
        });
    };
    match slot.authority {
        EvidenceAuthority::ModelAttested => {
            if command.signal.is_some() || command.workspace_mutation.is_some() {
                return Err(graph_evidence_error(
                    ErrorCode::InvalidArgument,
                    "wrong_evidence_authority",
                    format!(
                        "evidence slot `{}` accepts model testimony, not a process signal",
                        slot.id
                    ),
                ));
            }
            command
                .subject_digest
                .as_deref()
                .filter(|subject| !subject.trim().is_empty())
                .ok_or_else(|| {
                    graph_evidence_error(
                        ErrorCode::InvalidArgument,
                        "stale_evidence_subject",
                        format!("evidence slot `{}` requires a subject digest", slot.id),
                    )
                })?;
            Ok(GraphEvidenceSource::Model {
                run_id: command.run_id.clone(),
                call_id: command.call_id.clone(),
            })
        }
        EvidenceAuthority::DaemonVerified => {
            if let Some(mutation_ref) = command.workspace_mutation.as_ref() {
                if command.signal.is_some() {
                    return Err(graph_evidence_error(
                        ErrorCode::InvalidArgument,
                        "wrong_evidence_authority",
                        "daemon evidence cannot claim both process and mutation provenance",
                    ));
                }
                return validate_workspace_mutation_authority(
                    transaction,
                    session_id,
                    command,
                    slot,
                    graph_id,
                    attempt,
                    mutation_ref,
                );
            }
            let signal_ref = command.signal.as_ref().ok_or_else(|| {
                graph_evidence_error(
                    ErrorCode::InvalidArgument,
                    "wrong_evidence_authority",
                    format!(
                        "evidence slot `{}` requires a daemon process signal",
                        slot.id
                    ),
                )
            })?;
            let subject_digest = command
                .subject_digest
                .as_deref()
                .filter(|subject| !subject.trim().is_empty())
                .ok_or_else(|| {
                    graph_evidence_error(
                        ErrorCode::InvalidArgument,
                        "stale_evidence_subject",
                        format!("evidence slot `{}` requires a subject digest", slot.id),
                    )
                })?;
            if signal_ref.run_id != command.run_id {
                return Err(graph_evidence_error(
                    ErrorCode::InvalidArgument,
                    "mismatched_signal_provenance",
                    "process signal belongs to a different provider run",
                ));
            }
            let loaded = load_process_signal(transaction, session_id, signal_ref)?;
            let signal = &loaded.signal;
            validate_process_signal_provenance(transaction, session_id, signal)?;
            if signal.subject_digest != subject_digest {
                return Err(graph_evidence_error(
                    ErrorCode::RevisionConflict,
                    "stale_evidence_subject",
                    format!(
                        "evidence slot `{}` references a stale process subject",
                        slot.id
                    ),
                ));
            }
            validate_process_signal_freshness(
                transaction,
                session_id,
                graph_id,
                command.node.clone(),
                attempt,
                slot,
                &loaded,
            )?;
            if command.verdict == EvidenceVerdict::Green && signal.exit_code != Some(0) {
                return Err(graph_evidence_error(
                    ErrorCode::InvalidArgument,
                    "non_zero_exit_claimed_green",
                    format!(
                        "evidence slot `{}` claimed green from process exit {:?}",
                        slot.id, signal.exit_code
                    ),
                ));
            }
            Ok(GraphEvidenceSource::ProcessSignal {
                run_id: signal.run_id.clone(),
                call_id: signal.call_id.clone(),
                effect_id: signal.effect_id.clone(),
            })
        }
    }
}

// Keeping each duplicate/mismatch diagnostic beside its lifecycle arm makes
// this security validator auditable; folding mutation into match guards would
// obscure which phase advanced the local proof state.
#[allow(clippy::collapsible_match, clippy::result_large_err)]
fn validate_computer_observation_effect(
    transaction: &Transaction<'_>,
    session_id: &SessionId,
    command: &ComputerEvidenceCommand,
) -> StoreResult<u64> {
    let mut statement = transaction
        .prepare_cached(
            "SELECT envelope_json FROM events
             WHERE session_id = ?1
               AND (
                   payload_kind = 'effect'
                   OR (
                       payload_kind IS NULL
                       AND instr(envelope_json, '\"type\":\"effect\"') > 0
                   )
               )
             ORDER BY seq ASC",
        )
        .map_err(map_sqlite_error)?;
    let mut rows = statement
        .query([session_id.as_str()])
        .map_err(map_sqlite_error)?;
    let mut intent_seq = None;
    let mut authorized_seq = None;
    let mut dispatched_seq = None;
    let mut outcome_seq = None;
    let expected_summary = match command.observation {
        ComputerObservationKind::Screenshot => "computer screenshot",
        ComputerObservationKind::Inspect => "computer inspect",
    };

    while let Some(row) = rows.next().map_err(map_sqlite_error)? {
        let envelope = decode_envelope_column(row, 0).map_err(|error| {
            corrupt(format!(
                "invalid effect envelope in session {session_id}: {error}"
            ))
        })?;
        if envelope.run_id.as_ref() != Some(&command.run_id) {
            continue;
        }
        let Ok(EventPayload::Effect(phase)) =
            serde_json::from_value::<EventPayload>(envelope.payload)
        else {
            continue;
        };
        match phase {
            EffectPhase::Intent(intent) if intent.effect == command.effect_id => {
                if intent_seq.replace(envelope.seq).is_some()
                    || intent.class != EffectClass::ScreenObserve
                    || intent.summary != expected_summary
                    || intent.args_digest != command.effect_args_digest
                {
                    return Err(store_error(
                        ErrorCode::InvalidArgument,
                        "computer evidence does not match one ScreenObserve intent",
                        false,
                    ));
                }
            }
            EffectPhase::Authorized { effect, verdict }
                if effect == command.effect_id
                    && matches!(
                        verdict,
                        AuthorizationVerdict::Allow | AuthorizationVerdict::PreAuthorized { .. }
                    ) =>
            {
                if authorized_seq.replace(envelope.seq).is_some() {
                    return Err(store_error(
                        ErrorCode::InvalidArgument,
                        "computer evidence has duplicate authorization provenance",
                        false,
                    ));
                }
            }
            EffectPhase::Dispatched { effect } if effect == command.effect_id => {
                if dispatched_seq.replace(envelope.seq).is_some() {
                    return Err(store_error(
                        ErrorCode::InvalidArgument,
                        "computer evidence has duplicate dispatch provenance",
                        false,
                    ));
                }
            }
            EffectPhase::Outcome {
                effect,
                outcome: EffectOutcome::Ok,
                ..
            } if effect == command.effect_id => {
                if outcome_seq.replace(envelope.seq).is_some() {
                    return Err(store_error(
                        ErrorCode::InvalidArgument,
                        "computer evidence has duplicate outcome provenance",
                        false,
                    ));
                }
            }
            EffectPhase::Outcome { effect, .. } if effect == command.effect_id => {
                return Err(store_error(
                    ErrorCode::InvalidArgument,
                    "computer evidence requires a successful daemon-observed effect outcome",
                    false,
                ));
            }
            _ => {}
        }
    }

    let (Some(intent_seq), Some(authorized_seq), Some(dispatched_seq), Some(outcome_seq)) =
        (intent_seq, authorized_seq, dispatched_seq, outcome_seq)
    else {
        return Err(store_error(
            ErrorCode::InvalidArgument,
            "computer evidence has incomplete effect lifecycle provenance",
            false,
        ));
    };
    if !(intent_seq < authorized_seq
        && authorized_seq < dispatched_seq
        && dispatched_seq < outcome_seq)
    {
        return Err(store_error(
            ErrorCode::InvalidArgument,
            "computer evidence effect lifecycle is out of order",
            false,
        ));
    }
    Ok(outcome_seq)
}

#[allow(clippy::too_many_arguments, clippy::result_large_err)]
fn validate_workspace_mutation_authority(
    transaction: &Transaction<'_>,
    session_id: &SessionId,
    command: &GraphEvidenceCommand,
    slot: &EvidenceSlotSpec,
    graph_id: &GraphId,
    attempt: u32,
    mutation_ref: &WorkspaceMutationRef,
) -> StoreResult<GraphEvidenceSource> {
    if mutation_ref.run_id != command.run_id {
        return Err(graph_evidence_error(
            ErrorCode::InvalidArgument,
            "mismatched_signal_provenance",
            "workspace mutation belongs to a different provider run",
        ));
    }
    if slot.subject_selector == SubjectSelector::Freeform {
        return Err(graph_evidence_error(
            ErrorCode::InvalidArgument,
            "wrong_evidence_authority",
            format!(
                "daemon-verified evidence slot `{}` cannot use a freeform subject",
                slot.id
            ),
        ));
    }
    let loaded = load_workspace_mutation(transaction, session_id, mutation_ref)?;
    let expected_subject = loaded.mutation.subject_digest.as_deref().ok_or_else(|| {
        graph_evidence_error(
            ErrorCode::InvalidArgument,
            "mismatched_signal_provenance",
            "workspace mutation has no daemon-stamped subject digest",
        )
    })?;
    if command.subject_digest.as_deref() != Some(expected_subject) {
        return Err(graph_evidence_error(
            ErrorCode::RevisionConflict,
            "stale_evidence_subject",
            format!(
                "evidence slot `{}` references a stale workspace-mutation subject",
                slot.id
            ),
        ));
    }
    let revision = loaded
        .mutation
        .workspace_revision
        .as_ref()
        .ok_or_else(|| corrupt("workspace mutation has no stamped revision"))?;
    if current_workspace_revision(transaction, session_id)? != *revision {
        return Err(graph_evidence_error(
            ErrorCode::RevisionConflict,
            "stale_evidence_subject",
            format!(
                "evidence slot `{}` predates a later workspace mutation",
                slot.id
            ),
        ));
    }
    let epoch_seq = graph_attempt_opened_seq(
        transaction,
        session_id,
        graph_id,
        command.node.clone(),
        attempt,
    )?;
    if loaded.seq < epoch_seq {
        return Err(graph_evidence_error(
            ErrorCode::RevisionConflict,
            "stale_evidence_subject",
            format!(
                "evidence slot `{}` references a mutation from an older graph epoch",
                slot.id
            ),
        ));
    }
    if command.verdict == EvidenceVerdict::Green && loaded.outcome != EffectOutcome::Ok {
        return Err(graph_evidence_error(
            ErrorCode::InvalidArgument,
            "non_zero_exit_claimed_green",
            format!(
                "evidence slot `{}` claimed green from a failed workspace mutation",
                slot.id
            ),
        ));
    }
    Ok(GraphEvidenceSource::WorkspaceMutation {
        run_id: mutation_ref.run_id.clone(),
        effect_id: mutation_ref.effect_id.clone(),
    })
}

#[allow(clippy::result_large_err)]
fn load_workspace_mutation(
    transaction: &Transaction<'_>,
    session_id: &SessionId,
    mutation_ref: &WorkspaceMutationRef,
) -> StoreResult<LoadedWorkspaceMutation> {
    let mut statement = transaction
        .prepare_cached(
            "SELECT envelope_json FROM events
             WHERE session_id = ?1
               AND (
                   payload_kind IN ('effect', 'task_completed')
                   OR (
                       payload_kind IS NULL
                       AND instr(envelope_json, '\"workspace_mutation\"') > 0
                   )
               )
             ORDER BY seq ASC",
        )
        .map_err(map_sqlite_error)?;
    let mut rows = statement
        .query([session_id.as_str()])
        .map_err(map_sqlite_error)?;
    while let Some(row) = rows.next().map_err(map_sqlite_error)? {
        let envelope = decode_envelope_column(row, 0).map_err(|error| {
            corrupt(format!(
                "invalid workspace-mutation envelope in session {session_id}: {error}"
            ))
        })?;
        if envelope.run_id.as_ref() != Some(&mutation_ref.run_id) {
            continue;
        }
        let Ok(EventPayload::Effect(EffectPhase::Outcome {
            effect,
            outcome,
            workspace_mutation: Some(mutation),
            ..
        })) = serde_json::from_value::<EventPayload>(envelope.payload)
        else {
            continue;
        };
        if effect != mutation_ref.effect_id {
            continue;
        }
        if mutation.effect_id != effect {
            return Err(corrupt(format!(
                "workspace mutation effect mismatch in session {session_id}, seq {}",
                envelope.seq
            )));
        }
        let revision = mutation
            .workspace_revision
            .as_ref()
            .ok_or_else(|| corrupt("workspace mutation has no stamped revision"))?;
        let expected_subject =
            workspace_mutation_subject_digest(&effect, &mutation.mutation_digest, revision);
        if mutation.subject_digest.as_deref() != Some(expected_subject.as_str()) {
            return Err(corrupt(format!(
                "workspace mutation subject mismatch in session {session_id}, seq {}",
                envelope.seq
            )));
        }
        let mut intent_statement = transaction
            .prepare_cached(
                "SELECT envelope_json FROM events
                 WHERE session_id = ?1
                   AND (
                       payload_kind = 'effect'
                       OR (
                           payload_kind IS NULL
                           AND instr(envelope_json, '\"type\":\"effect\"') > 0
                       )
                   )
                 ORDER BY seq ASC",
            )
            .map_err(map_sqlite_error)?;
        let mut intent_rows = intent_statement
            .query([session_id.as_str()])
            .map_err(map_sqlite_error)?;
        let mut matched_intent = false;
        while let Some(row) = intent_rows.next().map_err(map_sqlite_error)? {
            let intent_envelope = decode_envelope_column(row, 0).map_err(|error| {
                corrupt(format!(
                    "invalid effect envelope in session {session_id}: {error}"
                ))
            })?;
            let Ok(EventPayload::Effect(EffectPhase::Intent(intent))) =
                serde_json::from_value::<EventPayload>(intent_envelope.payload)
            else {
                continue;
            };
            if intent.effect == effect {
                if intent.class != EffectClass::FsWrite
                    || intent_envelope.run_id.as_ref() != Some(&mutation_ref.run_id)
                {
                    return Err(graph_evidence_error(
                        ErrorCode::InvalidArgument,
                        "mismatched_signal_provenance",
                        "workspace mutation does not match a durable filesystem intent",
                    ));
                }
                matched_intent = true;
                break;
            }
        }
        if !matched_intent {
            return Err(graph_evidence_error(
                ErrorCode::InvalidArgument,
                "mismatched_signal_provenance",
                "workspace mutation is missing its durable filesystem intent",
            ));
        }
        return Ok(LoadedWorkspaceMutation {
            mutation,
            outcome,
            seq: envelope.seq,
        });
    }
    Err(graph_evidence_error(
        ErrorCode::InvalidArgument,
        "mismatched_signal_provenance",
        "workspace mutation reference does not match a durable effect outcome",
    ))
}

#[allow(clippy::too_many_arguments, clippy::result_large_err)]
fn validate_child_contract_authority(
    transaction: &Transaction<'_>,
    session_id: &SessionId,
    command: &GraphEvidenceCommand,
    slot: Option<&EvidenceSlotSpec>,
    graph_id: &GraphId,
    attempt: u32,
    contract: &ChildContractRef,
) -> StoreResult<GraphEvidenceSource> {
    if command.signal.is_some() || command.workspace_mutation.is_some() {
        return Err(graph_evidence_error(
            ErrorCode::InvalidArgument,
            "wrong_evidence_authority",
            "a child contract cannot also claim process-signal provenance",
        ));
    }
    let slot = slot.ok_or_else(|| {
        graph_evidence_error(
            ErrorCode::InvalidArgument,
            "unknown_evidence_slot",
            "a child contract requires one declared parent slot",
        )
    })?;
    let expected_subject = child_contract_subject_digest(contract);
    if command.subject_digest.as_deref() != Some(expected_subject.as_str()) {
        return Err(graph_evidence_error(
            ErrorCode::RevisionConflict,
            "stale_evidence_subject",
            "collapsed child subject does not match its revision/report provenance",
        ));
    }
    if slot.subject_selector == SubjectSelector::WorkspaceRevision
        && contract.workspace_revision.is_none()
    {
        return Err(graph_evidence_error(
            ErrorCode::RevisionConflict,
            "stale_evidence_subject",
            "parent workspace-revision slot requires a child terminal revision",
        ));
    }
    let attachments = load_child_graph_attachments(transaction, session_id)?
        .into_iter()
        .filter(|attached| {
            attached.parent_run_id == command.run_id
                && attached.parent_call_id == command.call_id
                && attached.parent_attempt.graph_id == *graph_id
                && attached.parent_attempt.node == command.node
                && attached.parent_attempt.attempt == attempt
                && attached.parent_slot == slot.id
                && attached.child_session_id == contract.child_session_id
                && attached.child_run_id == contract.child_run_id
        })
        .collect::<Vec<_>>();
    let [attached] = attachments.as_slice() else {
        return Err(graph_evidence_error(
            ErrorCode::InvalidArgument,
            "mismatched_child_provenance",
            "child contract has zero or multiple exact parent-attempt attachments",
        ));
    };
    if attached.parent_authority != slot.authority {
        return Err(graph_evidence_error(
            ErrorCode::InvalidArgument,
            "child_authority_growth",
            "child contract attempted to exceed its attached parent authority grant",
        ));
    }
    if !child_graph_is_descendant(
        transaction,
        &contract.child_session_id,
        &attached.child_graph_id,
        &contract.child_graph_id,
    )? {
        return Err(graph_evidence_error(
            ErrorCode::InvalidArgument,
            "mismatched_child_provenance",
            "terminal child graph is not the attached graph or its authored replacement",
        ));
    }
    let delegation = lookup_delegation_by_parent_call(
        transaction,
        session_id,
        &attached.parent_run_id,
        &attached.parent_call_id,
    )?
    .ok_or_else(|| {
        graph_evidence_error(
            ErrorCode::InvalidArgument,
            "mismatched_child_provenance",
            "child contract delegation no longer exists",
        )
    })?;
    if delegation.tool_item_id != attached.parent_tool_item_id
        || delegation.child_session_id != contract.child_session_id
        || delegation.child_run_id != contract.child_run_id
    {
        return Err(graph_evidence_error(
            ErrorCode::InvalidArgument,
            "mismatched_child_provenance",
            "child contract differs from durable delegation coordinates",
        ));
    }
    let report = delegation.report.as_ref().ok_or_else(|| {
        graph_evidence_error(
            ErrorCode::InvalidArgument,
            "mismatched_child_provenance",
            "child contract has no durable terminal report",
        )
    })?;
    let report_bytes = serde_json::to_vec(report)
        .map_err(|error| corrupt(format!("cannot re-encode durable child report: {error}")))?;
    if blake3::hash(&report_bytes).to_hex().as_str() != contract.report_digest
        || report.workspace_revision != contract.workspace_revision
    {
        return Err(graph_evidence_error(
            ErrorCode::RevisionConflict,
            "mismatched_child_provenance",
            "child report digest or revision differs from the collapsed contract",
        ));
    }
    let child = load_graph_reductions(transaction, &contract.child_session_id)?
        .graph(&contract.child_graph_id)
        .and_then(|reduction| reduction.status.clone())
        .ok_or_else(|| {
            graph_evidence_error(
                ErrorCode::InvalidArgument,
                "mismatched_child_provenance",
                "collapsed child graph does not exist in the child session",
            )
        })?;
    let terminal_matches = match child.phase {
        GraphPhase::Completed => {
            command.verdict == EvidenceVerdict::Green && report.verified != ReportVerification::Red
        }
        GraphPhase::Abandoned => command.verdict == EvidenceVerdict::Red,
        _ => false,
    };
    if !terminal_matches {
        return Err(graph_evidence_error(
            ErrorCode::InvalidArgument,
            "non_terminal_child_contract",
            "child evidence must match one completed or abandoned terminal graph",
        ));
    }
    if child.phase == GraphPhase::Completed
        && slot.authority == EvidenceAuthority::DaemonVerified
        && !child.nodes.iter().any(|node| {
            node.evidence_slots.iter().any(|evidence| {
                evidence.authority == EvidenceAuthority::DaemonVerified
                    && evidence.verdict == Some(EvidenceVerdict::Green)
                    && matches!(
                        evidence.source,
                        Some(GraphEvidenceSource::ProcessSignal { .. })
                    )
            })
        })
    {
        return Err(graph_evidence_error(
            ErrorCode::InvalidArgument,
            "child_authority_growth",
            "completed child graph lacks daemon process proof required by its parent slot",
        ));
    }
    Ok(GraphEvidenceSource::ChildContract {
        child_session_id: contract.child_session_id.clone(),
        child_run_id: contract.child_run_id.clone(),
        child_graph_id: contract.child_graph_id.clone(),
        report_digest: contract.report_digest.clone(),
        workspace_revision: contract.workspace_revision.clone(),
    })
}

#[allow(clippy::result_large_err)]
fn load_child_graph_attachments(
    connection: &Connection,
    session_id: &SessionId,
) -> StoreResult<Vec<ChildGraphAttached>> {
    let mut statement = connection
        .prepare_cached(
            "SELECT envelope_json FROM events
             WHERE session_id = ?1
               AND (
                   payload_kind = 'child_graph_attached'
                   OR (
                       payload_kind IS NULL
                       AND instr(envelope_json, '\"type\":\"child_graph_attached\"') > 0
                   )
               )
             ORDER BY seq ASC",
        )
        .map_err(map_sqlite_error)?;
    let mut rows = statement
        .query([session_id.as_str()])
        .map_err(map_sqlite_error)?;
    let mut attached = Vec::new();
    while let Some(row) = rows.next().map_err(map_sqlite_error)? {
        let envelope = decode_envelope_column(row, 0).map_err(|error| {
            corrupt(format!(
                "invalid child attachment envelope in session {session_id}: {error}"
            ))
        })?;
        if let Ok(EventPayload::ChildGraphAttached(item)) =
            serde_json::from_value::<EventPayload>(envelope.payload)
        {
            attached.push(item);
        }
    }
    Ok(attached)
}

#[allow(clippy::result_large_err)]
fn child_graph_is_descendant(
    connection: &Connection,
    session_id: &SessionId,
    attached: &GraphId,
    terminal: &GraphId,
) -> StoreResult<bool> {
    if attached == terminal {
        return Ok(true);
    }
    let mut next = HashMap::<GraphId, GraphId>::new();
    for envelope in load_graph_reduction_envelopes(connection, session_id)? {
        if let Ok(EventPayload::GraphSuperseded(replaced)) =
            serde_json::from_value::<EventPayload>(envelope.payload)
        {
            next.insert(replaced.old, replaced.new);
        }
    }
    let mut cursor = attached.clone();
    let mut visited = HashSet::new();
    while visited.insert(cursor.clone()) {
        let Some(replacement) = next.get(&cursor).cloned() else {
            return Ok(false);
        };
        if &replacement == terminal {
            return Ok(true);
        }
        cursor = replacement;
    }
    Ok(false)
}

fn graph_evidence_error(
    code: ErrorCode,
    kind: &'static str,
    message: impl Into<String>,
) -> HaiderError {
    let mut error = store_error(code, message, false);
    error.details = Some(serde_json::json!({ "kind": kind }));
    error
}

fn child_cache_error(kind: &'static str, message: impl Into<String>) -> HaiderError {
    let mut error = store_error(ErrorCode::RevisionConflict, message, false);
    error.details = Some(serde_json::json!({ "kind": kind }));
    error
}

fn workflow_revision_conflict(
    expected_digest: &str,
    current_digest: &str,
    current_revision: u32,
) -> HaiderError {
    let mut error = store_error(
        ErrorCode::RevisionConflict,
        "workflow changed before its immutable instance could be selected",
        true,
    );
    error.details = Some(serde_json::json!({
        "kind": "workflow_revision_conflict",
        "expected_digest": expected_digest,
        "current_digest": current_digest,
        "current_revision": current_revision,
    }));
    error
}

#[allow(clippy::result_large_err)]
fn load_child_template_observations(
    connection: &Connection,
) -> StoreResult<Vec<(SessionId, ChildTemplateObserved)>> {
    let mut statement = connection
        .prepare_cached(
            "SELECT envelope_json FROM events
             WHERE payload_kind = 'child_template_observed'
                OR (
                    payload_kind IS NULL
                    AND instr(envelope_json, '\"type\":\"child_template_observed\"') > 0
                )
             ORDER BY session_id ASC, seq ASC",
        )
        .map_err(map_sqlite_error)?;
    let mut rows = statement.query([]).map_err(map_sqlite_error)?;
    let mut observed = Vec::new();
    while let Some(row) = rows.next().map_err(map_sqlite_error)? {
        let envelope = decode_envelope_column(row, 0).map_err(|error| {
            corrupt(format!("invalid child cache observation envelope: {error}"))
        })?;
        if let Ok(EventPayload::ChildTemplateObserved(item)) =
            serde_json::from_value::<EventPayload>(envelope.payload)
        {
            observed.push((envelope.session_id, item));
        }
    }
    Ok(observed)
}

#[allow(clippy::result_large_err)]
fn validate_child_cache_bucket(
    connection: &Connection,
    key: &ChildTemplateCacheKey,
    observations: &[(SessionId, ChildTemplateObserved)],
) -> StoreResult<()> {
    let bucket = key.digest();
    let mut template_digest = None::<&str>;
    for (session_id, observed) in observations
        .iter()
        .filter(|(_, observed)| observed.cache_key.digest() == bucket)
    {
        if observed.cache_key != *key {
            return Err(child_cache_error(
                "colliding_child_template_cache",
                "child template cache digest contains a different simple key payload",
            ));
        }
        let recomputed = graph_template_digest(&observed.template);
        if recomputed != observed.digest
            || child_gate_structure(&observed.template) != key.gate_structure
            || template_digest.is_some_and(|digest| digest != observed.digest)
            || validate_pinned_graph_template(&observed.template).is_err()
        {
            return Err(child_cache_error(
                "poisoned_child_template_cache",
                "child template cache bucket contains a colliding or malformed template",
            ));
        }
        validate_child_template_observation_provenance(
            connection,
            session_id,
            observed,
            "poisoned_child_template_cache",
        )?;
        template_digest = Some(&observed.digest);
    }
    Ok(())
}

#[allow(clippy::result_large_err)]
fn validate_child_template_observation_provenance(
    connection: &Connection,
    session_id: &SessionId,
    observed: &ChildTemplateObserved,
    error_kind: &'static str,
) -> StoreResult<()> {
    let reject = |message| child_cache_error(error_kind, message);
    let collapse = load_envelope(connection, session_id, observed.collapse_evidence_seq)?
        .ok_or_else(|| reject("cached observation names no collapsed evidence"))?;
    let evidence = match serde_json::from_value::<EventPayload>(collapse.payload) {
        Ok(EventPayload::EvidenceRecorded(evidence)) => evidence,
        _ => return Err(reject("cached observation sequence is not graph evidence")),
    };
    let source_matches = matches!(
        &evidence.source,
        GraphEvidenceSource::ChildContract {
            child_session_id,
            child_run_id,
            child_graph_id,
            report_digest,
            workspace_revision,
        } if child_session_id == &observed.child_contract.child_session_id
            && child_run_id == &observed.child_contract.child_run_id
            && child_graph_id == &observed.child_contract.child_graph_id
            && report_digest == &observed.child_contract.report_digest
            && workspace_revision == &observed.child_contract.workspace_revision
    );
    if evidence.graph_id != observed.parent_attempt.graph_id
        || evidence.node != observed.parent_attempt.node
        || evidence.attempt != observed.parent_attempt.attempt
        || evidence.verdict != EvidenceVerdict::Green
        || !source_matches
    {
        return Err(reject(
            "cached observation is not exact green collapsed child evidence",
        ));
    }
    let attachments = load_child_graph_attachments(connection, session_id)?
        .into_iter()
        .filter(|attached| {
            attached.parent_attempt == observed.parent_attempt
                && attached.child_session_id == observed.child_contract.child_session_id
                && attached.child_run_id == observed.child_contract.child_run_id
                && attached.child_graph_id == observed.child_contract.child_graph_id
        })
        .collect::<Vec<_>>();
    let [attached] = attachments.as_slice() else {
        return Err(reject(
            "cached observation has no single exact unchanged child attachment",
        ));
    };
    if evidence.slot.as_deref() != Some(attached.parent_slot.as_str())
        || evidence.subject_digest.as_deref()
            != Some(child_contract_subject_digest(&observed.child_contract).as_str())
        || attached.cache_key != observed.cache_key
        || attached.template != observed.template.name
        || attached.digest != observed.digest
    {
        return Err(reject(
            "cached observation differs from its child attachment or contract subject",
        ));
    }
    Ok(())
}

fn child_cache_distinct_attempts(
    key: &ChildTemplateCacheKey,
    observations: &[(SessionId, ChildTemplateObserved)],
) -> u32 {
    u32::try_from(
        observations
            .iter()
            .filter(|(_, observed)| observed.cache_key == *key)
            .map(|(session_id, observed)| {
                (
                    session_id.clone(),
                    observed.parent_attempt.graph_id.clone(),
                    observed.parent_attempt.node.clone(),
                    observed.parent_attempt.attempt,
                )
            })
            .collect::<HashSet<_>>()
            .len(),
    )
    .unwrap_or(u32::MAX)
}

fn stable_digest(parts: &[&str]) -> String {
    let mut hasher = blake3::Hasher::new();
    for part in parts {
        hasher.update(&(part.len() as u64).to_be_bytes());
        hasher.update(part.as_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

fn projection_checkpoint_digest(
    session_id: &SessionId,
    projection: &str,
    timeline_key: &str,
    through_seq: u64,
    boundary_event_id: &str,
    payload: &[u8],
) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"haider/session-projection-checkpoint/v1\0");
    for bytes in [
        session_id.as_str().as_bytes(),
        projection.as_bytes(),
        timeline_key.as_bytes(),
        boundary_event_id.as_bytes(),
        payload,
    ] {
        hasher.update(&u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_be_bytes());
        hasher.update(bytes);
    }
    hasher.update(&through_seq.to_be_bytes());
    hasher.finalize()
}

#[allow(clippy::result_large_err)]
fn validate_child_cache_key(key: &ChildTemplateCacheKey) -> StoreResult<()> {
    let bounded = !key.task_shape.is_empty()
        && key.task_shape.len() <= 128
        && !key.effective_grant_digest.is_empty()
        && key.effective_grant_digest.len() <= 128
        && !key.gate_structure.is_empty()
        && key.gate_structure.len() <= 8 * 1024;
    if bounded {
        Ok(())
    } else {
        Err(store_error(
            ErrorCode::InvalidArgument,
            "child template cache key exceeds its simple bounded shape",
            false,
        ))
    }
}

fn process_signal_event_id(session_id: &SessionId, effect_id: &EffectId) -> EventId {
    let mut hasher = blake3::Hasher::new();
    for value in [session_id.as_str(), effect_id.as_str()] {
        hasher.update(&(value.len() as u64).to_be_bytes());
        hasher.update(value.as_bytes());
    }
    EventId::new(format!("process-signal-{}", hasher.finalize().to_hex()))
}

struct LoadedProcessSignal {
    signal: ProcessSignalRecorded,
    seq: u64,
    branch_id: Option<BranchId>,
}

struct LoadedWorkspaceMutation {
    mutation: WorkspaceMutation,
    outcome: EffectOutcome,
    seq: u64,
}

#[allow(clippy::result_large_err)]
fn load_process_signal(
    transaction: &Transaction<'_>,
    session_id: &SessionId,
    signal_ref: &ProcessSignalRef,
) -> StoreResult<LoadedProcessSignal> {
    let event_id = process_signal_event_id(session_id, &signal_ref.effect_id);
    let envelope = load_envelope_by_event_id(transaction, &event_id)?.ok_or_else(|| {
        graph_evidence_error(
            ErrorCode::InvalidArgument,
            "mismatched_signal_provenance",
            "referenced process signal is not recorded in this session",
        )
    })?;
    let signal = match serde_json::from_value::<EventPayload>(envelope.payload) {
        Ok(EventPayload::ProcessSignalRecorded(signal)) => signal,
        _ => {
            return Err(corrupt(format!("event {event_id} is not a process signal")));
        }
    };
    if envelope.session_id != *session_id
        || envelope.run_id.as_ref() != Some(&signal.run_id)
        || signal.run_id != signal_ref.run_id
        || signal.call_id != signal_ref.call_id
        || signal.effect_id != signal_ref.effect_id
    {
        return Err(graph_evidence_error(
            ErrorCode::InvalidArgument,
            "mismatched_signal_provenance",
            "process signal reference does not match its durable run/call/effect provenance",
        ));
    }
    Ok(LoadedProcessSignal {
        signal,
        seq: envelope.seq,
        branch_id: envelope.branch_id,
    })
}

#[allow(clippy::result_large_err)]
fn validate_process_signal_freshness(
    transaction: &Transaction<'_>,
    session_id: &SessionId,
    graph_id: &GraphId,
    node: GraphNodeName,
    attempt: u32,
    slot: &EvidenceSlotSpec,
    loaded: &LoadedProcessSignal,
) -> StoreResult<()> {
    let epoch_seq = graph_attempt_opened_seq(transaction, session_id, graph_id, node, attempt)?;
    if loaded.seq < epoch_seq {
        return Err(graph_evidence_error(
            ErrorCode::RevisionConflict,
            "stale_evidence_subject",
            format!(
                "evidence slot `{}` references a process signal from an older graph epoch",
                slot.id
            ),
        ));
    }

    if let Some(revision) = loaded.signal.workspace_revision.as_ref()
        && current_workspace_revision(transaction, session_id)? != *revision
    {
        return Err(graph_evidence_error(
            ErrorCode::RevisionConflict,
            "stale_evidence_subject",
            format!(
                "evidence slot `{}` predates a later workspace mutation",
                slot.id
            ),
        ));
    }

    let signals = process_signals_since(
        transaction,
        session_id,
        epoch_seq,
        loaded.branch_id.as_ref(),
    )?;
    let current = match slot.subject_selector {
        SubjectSelector::Command => signals.iter().find(|candidate| {
            candidate.signal.command_arg_digest == loaded.signal.command_arg_digest
        }),
        SubjectSelector::WorkspaceRevision => {
            let revision = loaded.signal.workspace_revision.as_ref().ok_or_else(|| {
                graph_evidence_error(
                    ErrorCode::RevisionConflict,
                    "stale_evidence_subject",
                    format!(
                        "evidence slot `{}` requires a daemon-observed workspace revision",
                        slot.id
                    ),
                )
            })?;
            signals
                .iter()
                .find(|candidate| candidate.signal.workspace_revision.is_some())
                .filter(|candidate| candidate.signal.workspace_revision.as_ref() == Some(revision))
        }
        SubjectSelector::Freeform => None,
    };
    match slot.subject_selector {
        SubjectSelector::Command
            if current.is_none_or(|candidate| {
                candidate.signal.effect_id != loaded.signal.effect_id
                    || candidate.signal.subject_digest != loaded.signal.subject_digest
            }) =>
        {
            Err(graph_evidence_error(
                ErrorCode::RevisionConflict,
                "stale_evidence_subject",
                format!(
                    "evidence slot `{}` does not reference the newest daemon-observed command subject",
                    slot.id
                ),
            ))
        }
        SubjectSelector::WorkspaceRevision if current.is_none() => Err(graph_evidence_error(
            ErrorCode::RevisionConflict,
            "stale_evidence_subject",
            format!(
                "evidence slot `{}` does not match the current daemon-observed workspace revision",
                slot.id
            ),
        )),
        SubjectSelector::Freeform => Err(graph_evidence_error(
            ErrorCode::InvalidArgument,
            "wrong_evidence_authority",
            format!(
                "daemon-verified evidence slot `{}` cannot use a freeform subject",
                slot.id
            ),
        )),
        _ => Ok(()),
    }
}

#[allow(clippy::result_large_err)]
fn graph_attempt_opened_seq(
    transaction: &Transaction<'_>,
    session_id: &SessionId,
    graph_id: &GraphId,
    node: GraphNodeName,
    attempt: u32,
) -> StoreResult<u64> {
    for envelope in load_graph_reduction_envelopes(transaction, session_id)?
        .into_iter()
        .rev()
    {
        let Ok(EventPayload::GraphAttemptOpened(opened)) =
            serde_json::from_value::<EventPayload>(envelope.payload)
        else {
            continue;
        };
        if opened.graph_id == *graph_id && opened.node == node && opened.attempt == attempt {
            return Ok(envelope.seq);
        }
    }
    Err(corrupt(format!(
        "active graph {graph_id} has no opening fact for {} attempt {attempt}",
        node.label()
    )))
}

#[allow(clippy::result_large_err)]
fn process_signals_since(
    transaction: &Transaction<'_>,
    session_id: &SessionId,
    first_seq: u64,
    branch_id: Option<&BranchId>,
) -> StoreResult<Vec<LoadedProcessSignal>> {
    let mut statement = transaction
        .prepare_cached(
            "SELECT envelope_json FROM events
             WHERE session_id = ?1 AND seq >= ?2
               AND (
                   payload_kind = 'process_signal_recorded'
                   OR (
                       payload_kind IS NULL
                       AND instr(envelope_json, '\"type\":\"process_signal_recorded\"') > 0
                   )
               )
             ORDER BY seq DESC",
        )
        .map_err(map_sqlite_error)?;
    let first_seq = to_sqlite_integer(first_seq)?;
    let mut rows = statement
        .query(params![session_id.as_str(), first_seq])
        .map_err(map_sqlite_error)?;
    let mut signals = Vec::new();
    while let Some(row) = rows.next().map_err(map_sqlite_error)? {
        let envelope = decode_envelope_column(row, 0).map_err(|error| {
            corrupt(format!(
                "invalid process-signal envelope in session {session_id}: {error}"
            ))
        })?;
        if envelope.branch_id.as_ref() != branch_id {
            continue;
        }
        let Ok(EventPayload::ProcessSignalRecorded(signal)) =
            serde_json::from_value::<EventPayload>(envelope.payload)
        else {
            continue;
        };
        signals.push(LoadedProcessSignal {
            signal,
            seq: envelope.seq,
            branch_id: envelope.branch_id,
        });
    }
    Ok(signals)
}

fn process_signal_base_matches(
    left: &ProcessSignalRecorded,
    right: &ProcessSignalRecorded,
) -> bool {
    left.run_id == right.run_id
        && left.call_id == right.call_id
        && left.effect_id == right.effect_id
        && left.command_arg_digest == right.command_arg_digest
        && left.exit_code == right.exit_code
        && left.transcript_digest == right.transcript_digest
        && left.artifact == right.artifact
}

#[allow(clippy::result_large_err)]
fn current_workspace_revision(
    transaction: &Transaction<'_>,
    session_id: &SessionId,
) -> StoreResult<WorkspaceRevision> {
    workspace_revision_at_or_before(transaction, session_id, i64::MAX as u64)
}

#[allow(clippy::result_large_err)]
fn workspace_revision_at_or_before(
    transaction: &Transaction<'_>,
    session_id: &SessionId,
    through_seq: u64,
) -> StoreResult<WorkspaceRevision> {
    let mut statement = transaction
        .prepare_cached(
            "SELECT envelope_json FROM events
             WHERE session_id = ?1 AND seq <= ?2
               AND (
                   payload_kind IN ('effect', 'task_completed')
                   OR (
                       payload_kind IS NULL
                       AND instr(envelope_json, '\"workspace_mutation\"') > 0
                   )
               )
             ORDER BY seq DESC",
        )
        .map_err(map_sqlite_error)?;
    let mut rows = statement
        .query(params![
            session_id.as_str(),
            to_sqlite_integer(through_seq)?
        ])
        .map_err(map_sqlite_error)?;
    while let Some(row) = rows.next().map_err(map_sqlite_error)? {
        let envelope = decode_envelope_column(row, 0).map_err(|error| {
            corrupt(format!(
                "invalid workspace-mutation envelope in session {session_id}: {error}"
            ))
        })?;
        let mutation = match envelope
            .payload
            .get("type")
            .and_then(serde_json::Value::as_str)
        {
            Some("effect") => match decode_payload::<EventPayload>(&envelope.payload) {
                Ok(EventPayload::Effect(EffectPhase::Outcome {
                    workspace_mutation: Some(mutation),
                    ..
                })) => Some(mutation),
                _ => None,
            },
            Some("task_completed") => decode_payload::<TaskEventPayload>(&envelope.payload)
                .ok()
                .and_then(|event| match event {
                    TaskEventPayload::TaskCompleted(completed) => completed.workspace_mutation,
                    TaskEventPayload::TaskStarted(_) => None,
                }),
            _ => None,
        };
        let Some(mutation) = mutation else {
            continue;
        };
        return mutation.workspace_revision.ok_or_else(|| {
            corrupt(format!(
                "workspace mutation in session {session_id}, seq {} has no stamped revision",
                envelope.seq
            ))
        });
    }
    Ok(workspace_revision_for_seq(0))
}

#[allow(clippy::result_large_err)]
fn process_effect_outcome_seq(
    transaction: &Transaction<'_>,
    session_id: &SessionId,
    run_id: &RunId,
    effect_id: &EffectId,
) -> StoreResult<u64> {
    let mut statement = transaction
        .prepare_cached(
            "SELECT envelope_json FROM events
             WHERE session_id = ?1
               AND (
                   payload_kind = 'effect'
                   OR (
                       payload_kind IS NULL
                       AND instr(envelope_json, '\"type\":\"effect\"') > 0
                   )
               )
             ORDER BY seq ASC",
        )
        .map_err(map_sqlite_error)?;
    let mut rows = statement
        .query([session_id.as_str()])
        .map_err(map_sqlite_error)?;
    while let Some(row) = rows.next().map_err(map_sqlite_error)? {
        let envelope = decode_envelope_column(row, 0).map_err(|error| {
            corrupt(format!(
                "invalid process-effect envelope in session {session_id}: {error}"
            ))
        })?;
        let Ok(EventPayload::Effect(EffectPhase::Outcome { effect, .. })) =
            serde_json::from_value::<EventPayload>(envelope.payload)
        else {
            continue;
        };
        if effect == *effect_id {
            if envelope.run_id.as_ref() != Some(run_id) {
                return Err(graph_evidence_error(
                    ErrorCode::InvalidArgument,
                    "mismatched_signal_provenance",
                    "process signal does not match its durable effect outcome run",
                ));
            }
            return Ok(envelope.seq);
        }
    }
    Err(graph_evidence_error(
        ErrorCode::InvalidArgument,
        "mismatched_signal_provenance",
        "process signal is missing its durable terminal effect outcome",
    ))
}

#[allow(clippy::result_large_err)]
fn validate_process_signal_provenance(
    transaction: &Transaction<'_>,
    session_id: &SessionId,
    signal: &ProcessSignalRecorded,
) -> StoreResult<()> {
    if signal.call_id.trim().is_empty()
        || signal.command_arg_digest.trim().is_empty()
        || signal.transcript_digest.trim().is_empty()
        || signal.subject_digest.trim().is_empty()
    {
        return Err(graph_evidence_error(
            ErrorCode::InvalidArgument,
            "mismatched_signal_provenance",
            "process signal contains empty provenance fields",
        ));
    }
    let expected_subject = process_signal_subject_digest(
        &signal.command_arg_digest,
        &signal.transcript_digest,
        signal.workspace_revision.as_ref(),
    );
    if expected_subject != signal.subject_digest {
        return Err(graph_evidence_error(
            ErrorCode::RevisionConflict,
            "stale_evidence_subject",
            "process signal subject digest does not match its recorded command and transcript",
        ));
    }
    let mut statement = transaction
        .prepare_cached(
            "SELECT envelope_json FROM events
             WHERE session_id = ?1
               AND (
                   payload_kind = 'effect'
                   OR (
                       payload_kind IS NULL
                       AND instr(envelope_json, '\"type\":\"effect\"') > 0
                   )
               )
             ORDER BY seq ASC",
        )
        .map_err(map_sqlite_error)?;
    let mut rows = statement
        .query([session_id.as_str()])
        .map_err(map_sqlite_error)?;
    let mut matched_intent = false;
    let mut matched_terminal = false;
    while let Some(row) = rows.next().map_err(map_sqlite_error)? {
        let envelope = decode_envelope_column(row, 0).map_err(|error| {
            corrupt(format!(
                "invalid effect envelope in session {session_id}: {error}"
            ))
        })?;
        let Ok(EventPayload::Effect(phase)) =
            serde_json::from_value::<EventPayload>(envelope.payload)
        else {
            continue;
        };
        match phase {
            EffectPhase::Intent(intent) if intent.effect == signal.effect_id => {
                if envelope.run_id.as_ref() != Some(&signal.run_id)
                    || intent.class != EffectClass::ProcessExec
                    || intent.args_digest != signal.command_arg_digest
                {
                    return Err(graph_evidence_error(
                        ErrorCode::InvalidArgument,
                        "mismatched_signal_provenance",
                        "process signal does not match its durable effect intent",
                    ));
                }
                matched_intent = true;
            }
            EffectPhase::Outcome {
                effect, outcome, ..
            } if effect == signal.effect_id => {
                if envelope.run_id.as_ref() != Some(&signal.run_id)
                    || outcome == EffectOutcome::Unknown
                {
                    return Err(graph_evidence_error(
                        ErrorCode::InvalidArgument,
                        "mismatched_signal_provenance",
                        "process signal does not match a known terminal effect outcome",
                    ));
                }
                matched_terminal = true;
            }
            _ => {}
        }
    }
    if !matched_intent || !matched_terminal {
        return Err(graph_evidence_error(
            ErrorCode::InvalidArgument,
            "mismatched_signal_provenance",
            "process signal is missing its durable process intent or terminal outcome",
        ));
    }
    Ok(())
}

fn graph_confirm_menu(status: &GraphStatus, node: &GraphNodeName, attempt: u32) -> Menu {
    graph_confirm_menu_for(&status.graph_id, &status.template, node, attempt)
}

fn graph_confirm_menu_for(
    graph_id: &GraphId,
    template: &str,
    node: &GraphNodeName,
    attempt: u32,
) -> Menu {
    let legacy_ship_loop_id =
        template == haider_protocol::graph::SHIP_LOOP_TEMPLATE && node.as_str() == "SHIP";
    let id = if legacy_ship_loop_id {
        format!("graph-confirm-{graph_id}-{attempt}")
    } else {
        format!("graph-confirm-{graph_id}-{}-{attempt}", node.as_str())
    };
    Menu {
        id: MenuId::new(id),
        kind: MenuKind::GraphHumanConfirm {
            graph_id: graph_id.clone(),
            node: node.clone(),
            attempt,
        },
        title: format!("Confirm {}?", node.label()),
        body: vec![format!(
            "All dependencies for {} are green in attempt {attempt}.",
            node.label()
        )],
        options: vec![
            haider_protocol::menu::MenuOption {
                key: "confirm".into(),
                label: "Confirm".into(),
                detail: Some(format!("Satisfy the {} gate.", node.label())),
                decision: None,
            },
            haider_protocol::menu::MenuOption {
                key: "hold".into(),
                label: "Hold".into(),
                detail: Some("Park the graph for explicit re-pin or abandon.".into()),
                decision: None,
            },
        ],
        blocking: false,
        scope: haider_protocol::menu::MenuScope::Session,
        origin: "convergence-graph".into(),
        ttl_ms: None,
        timeout_option: None,
    }
}

fn graph_finalization_state_digest(status: &GraphStatus) -> StoreResult<String> {
    let mut obligation = status.clone();
    // Guardrail menus are presentation/wait state, not graph progress. If
    // included, opening the confirmation would make its own coordinates stale
    // before either valid answer could commit.
    obligation.pending_menu = None;
    obligation.pending_menus.clear();
    let encoded = serde_json::to_vec(&obligation).map_err(|error| {
        store_error(
            ErrorCode::Internal,
            format!("cannot encode graph finalization state: {error}"),
            false,
        )
    })?;
    Ok(blake3::hash(&encoded).to_hex().to_string())
}

fn graph_abandon_confirm_menu_id(
    session_id: &SessionId,
    graph_id: &GraphId,
    run_id: &RunId,
    state_digest: &str,
    ordinal: usize,
) -> MenuId {
    let mut hasher = blake3::Hasher::new();
    let ordinal = ordinal.to_string();
    for value in [
        session_id.as_str(),
        graph_id.as_str(),
        run_id.as_str(),
        state_digest,
        &ordinal,
    ] {
        hasher.update(&(value.len() as u64).to_be_bytes());
        hasher.update(value.as_bytes());
    }
    MenuId::new(format!(
        "graph-abandon-confirm-{}",
        hasher.finalize().to_hex()
    ))
}

fn graph_abandon_confirm_menu(
    id: MenuId,
    graph_id: GraphId,
    run_id: RunId,
    state_digest: String,
) -> Menu {
    Menu {
        id,
        kind: MenuKind::GraphAbandonConfirm {
            graph_id,
            run_id,
            state_digest,
        },
        title: "Workflow is unfinished".into(),
        body: vec![
            "The final response left required workflow obligations unmet.".into(),
            "Continue working, or explicitly abandon the workflow and finish.".into(),
        ],
        options: vec![
            haider_protocol::menu::MenuOption {
                key: "continue-work".into(),
                label: "Continue work".into(),
                detail: Some(
                    "Resume the current run and satisfy the remaining obligations.".into(),
                ),
                decision: None,
            },
            haider_protocol::menu::MenuOption {
                key: "abandon-and-finish".into(),
                label: "Abandon and finish".into(),
                detail: Some(
                    "Durably abandon this workflow, then accept the final response.".into(),
                ),
                decision: None,
            },
        ],
        blocking: true,
        scope: haider_protocol::menu::MenuScope::Session,
        origin: "convergence-graph-finalization".into(),
        ttl_ms: None,
        timeout_option: None,
    }
}

fn graph_finalization_envelope(
    command: &GraphFinalizationCommand,
    state_digest: &str,
    kind: &str,
    payload: EventPayload,
) -> StoreResult<RawEnvelope> {
    let mut hasher = blake3::Hasher::new();
    for value in [
        command.session_id.as_str(),
        command.run_id.as_str(),
        state_digest,
        kind,
    ] {
        hasher.update(&(value.len() as u64).to_be_bytes());
        hasher.update(value.as_bytes());
    }
    unstamped_command_envelope(
        EventId::new(format!("graph-finalization-{}", hasher.finalize().to_hex())),
        &command.session_id,
        command.branch_id.clone(),
        Some(command.run_id.clone()),
        command.device_id.clone(),
        command.worker_generation,
        payload,
        PromptRender::Omit,
    )
}

const RUN_HEADS_PROJECTION: &str = "run_heads_v1";
const RUN_HEADS_TIMELINE: &str = "session";
const RUN_HEADS_PAYLOAD_VERSION: u32 = 1;

type DurableRunHead = (RunState, u64, Option<BranchId>);

#[derive(Clone, Debug, PartialEq)]
struct ProjectedRunHead {
    state: RunState,
    state_seq: Option<u64>,
    accepted_seq: Option<u64>,
    branch_id: Option<BranchId>,
    prompt_run_id: Option<RunId>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct RunHeadsCheckpointPayload {
    version: u32,
    heads: Vec<RunHeadCheckpointRow>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct RunHeadCheckpointRow {
    run_id: String,
    state: RunState,
    state_seq: Option<u64>,
    accepted_seq: Option<u64>,
    branch_id: Option<String>,
    prompt_run_id: Option<String>,
}

struct RunHeadProjectionMeta {
    through_seq: u64,
    run_count: u64,
    nonterminal_count: u64,
}

struct StoredRunHead {
    run_id: String,
    state_json: String,
    state_seq: Option<i64>,
    terminal: i64,
    accepted_seq: Option<i64>,
    branch_id: Option<String>,
    prompt_run_id: Option<String>,
    checksum: Vec<u8>,
}

fn map_run_head_row_error(error: SqliteError) -> HaiderError {
    if matches!(
        &error,
        SqliteError::FromSqlConversionFailure(..)
            | SqliteError::IntegralValueOutOfRange(..)
            | SqliteError::InvalidColumnType(..)
    ) {
        return corrupt(format!("run-head projection row is malformed: {error}"));
    }
    map_sqlite_error(error)
}

fn map_projection_checkpoint_row_error(
    error: SqliteError,
    session_id: &SessionId,
    projection: &str,
    timeline_key: &str,
) -> HaiderError {
    if matches!(
        &error,
        SqliteError::FromSqlConversionFailure(..)
            | SqliteError::IntegralValueOutOfRange(..)
            | SqliteError::InvalidColumnType(..)
    ) {
        return corrupt(format!(
            "projection checkpoint {projection}/{timeline_key} for session {session_id} has malformed storage: {error}"
        ));
    }
    map_sqlite_error(error)
}

fn latest_run_states(
    connection: &Connection,
    session_id: &SessionId,
) -> StoreResult<HashMap<RunId, DurableRunHead>> {
    Ok(load_projected_run_heads(connection, session_id)?
        .into_iter()
        .filter_map(|(run_id, head)| {
            head.state_seq
                .map(|state_seq| (run_id, (head.state, state_seq, head.branch_id)))
        })
        .collect())
}

fn journal_latest_seq(connection: &Connection, session_id: &SessionId) -> StoreResult<u64> {
    let latest = connection
        .query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM events WHERE session_id = ?1",
            [session_id.as_str()],
            |row| row.get::<_, i64>(0),
        )
        .map_err(map_sqlite_error)?;
    u64::try_from(latest).map_err(|_| corrupt("database contains a negative event sequence"))
}

fn run_head_projection_meta(
    connection: &Connection,
    session_id: &SessionId,
) -> StoreResult<Option<RunHeadProjectionMeta>> {
    let row = connection
        .query_row(
            "SELECT through_seq, run_count, nonterminal_count
             FROM run_head_sessions WHERE session_id = ?1",
            [session_id.as_str()],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()
        .map_err(map_run_head_row_error)?;
    row.map(|(through_seq, run_count, nonterminal_count)| {
        Ok(RunHeadProjectionMeta {
            through_seq: u64::try_from(through_seq)
                .map_err(|_| corrupt("run-head projection has a negative journal watermark"))?,
            run_count: u64::try_from(run_count)
                .map_err(|_| corrupt("run-head projection has a negative row count"))?,
            nonterminal_count: u64::try_from(nonterminal_count)
                .map_err(|_| corrupt("run-head projection has a negative live-run count"))?,
        })
    })
    .transpose()
}

fn projected_run_head_counts(heads: &HashMap<RunId, ProjectedRunHead>) -> StoreResult<(u64, u64)> {
    let run_count =
        u64::try_from(heads.len()).map_err(|_| corrupt("too many durable run heads"))?;
    let nonterminal_count = u64::try_from(
        heads
            .values()
            .filter(|head| head.state_seq.is_some() && !head.state.is_terminal())
            .count(),
    )
    .map_err(|_| corrupt("too many nonterminal durable run heads"))?;
    Ok((run_count, nonterminal_count))
}

fn ensure_run_head_projection(
    connection: &Connection,
    session_id: &SessionId,
) -> StoreResult<RunHeadProjectionMeta> {
    let journal_head = journal_latest_seq(connection, session_id)?;
    let metadata = match run_head_projection_meta(connection, session_id) {
        Ok(metadata) => metadata,
        Err(error) if error.code == ErrorCode::StoreCorrupt => None,
        Err(error) => return Err(error),
    };
    if let Some(metadata) = metadata
        && metadata.through_seq == journal_head
    {
        return Ok(metadata);
    }
    rebuild_run_head_projection(connection, session_id)?;
    run_head_projection_meta(connection, session_id)?.ok_or_else(|| {
        corrupt(format!(
            "run-head projection rebuild omitted session {session_id}"
        ))
    })
}

#[allow(clippy::too_many_arguments)]
fn run_head_checksum(
    session_id: &SessionId,
    run_id: &str,
    state_json: &str,
    state_seq: Option<u64>,
    terminal: bool,
    accepted_seq: Option<u64>,
    branch_id: Option<&str>,
    prompt_run_id: Option<&str>,
) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"haider-run-head-v1\0");
    for value in [
        Some(session_id.as_str()),
        Some(run_id),
        Some(state_json),
        branch_id,
        prompt_run_id,
    ] {
        match value {
            Some(value) => {
                hasher.update(&[1]);
                hasher.update(&u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
                hasher.update(value.as_bytes());
            }
            None => {
                hasher.update(&[0]);
            }
        }
    }
    for value in [state_seq, accepted_seq] {
        match value {
            Some(value) => {
                hasher.update(&[1]);
                hasher.update(&value.to_be_bytes());
            }
            None => {
                hasher.update(&[0]);
            }
        }
    }
    hasher.update(&[u8::from(terminal)]);
    hasher.finalize()
}

fn decode_stored_run_head(
    session_id: &SessionId,
    stored: StoredRunHead,
) -> StoreResult<(RunId, ProjectedRunHead)> {
    let state = serde_json::from_str::<RunState>(&stored.state_json).map_err(|error| {
        corrupt(format!(
            "run-head projection for session {session_id}, run {} has invalid state: {error}",
            stored.run_id
        ))
    })?;
    let terminal = match stored.terminal {
        0 => false,
        1 => true,
        _ => {
            return Err(corrupt(format!(
                "run-head projection for session {session_id}, run {} has invalid terminal flag",
                stored.run_id
            )));
        }
    };
    if terminal != state.is_terminal() {
        return Err(corrupt(format!(
            "run-head projection for session {session_id}, run {} disagrees on terminal state",
            stored.run_id
        )));
    }
    let state_seq = stored
        .state_seq
        .map(|value| {
            u64::try_from(value)
                .ok()
                .filter(|value| *value > 0)
                .ok_or_else(|| corrupt("run-head projection has an invalid state sequence"))
        })
        .transpose()?;
    let accepted_seq = stored
        .accepted_seq
        .map(|value| {
            u64::try_from(value)
                .ok()
                .filter(|value| *value > 0)
                .ok_or_else(|| corrupt("run-head projection has an invalid accepted sequence"))
        })
        .transpose()?;
    let checksum = run_head_checksum(
        session_id,
        &stored.run_id,
        &stored.state_json,
        state_seq,
        terminal,
        accepted_seq,
        stored.branch_id.as_deref(),
        stored.prompt_run_id.as_deref(),
    );
    if stored.checksum.as_slice() != checksum.as_bytes() {
        return Err(corrupt(format!(
            "run-head projection for session {session_id}, run {} has an invalid checksum",
            stored.run_id
        )));
    }
    Ok((
        RunId::new(stored.run_id),
        ProjectedRunHead {
            state,
            state_seq,
            accepted_seq,
            branch_id: stored.branch_id.map(BranchId::new),
            prompt_run_id: stored.prompt_run_id.map(RunId::new),
        },
    ))
}

fn stored_run_head(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredRunHead> {
    Ok(StoredRunHead {
        run_id: row.get(0)?,
        state_json: row.get(1)?,
        state_seq: row.get(2)?,
        terminal: row.get(3)?,
        accepted_seq: row.get(4)?,
        branch_id: row.get(5)?,
        prompt_run_id: row.get(6)?,
        checksum: row.get(7)?,
    })
}

fn try_load_projected_run_heads(
    connection: &Connection,
    session_id: &SessionId,
) -> StoreResult<HashMap<RunId, ProjectedRunHead>> {
    let mut statement = connection
        .prepare_cached(
            "SELECT run_id, state_json, state_seq, terminal, accepted_seq,
                    branch_id, prompt_run_id, checksum
             FROM run_heads WHERE session_id = ?1 ORDER BY run_id",
        )
        .map_err(map_sqlite_error)?;
    let rows = statement
        .query_map([session_id.as_str()], stored_run_head)
        .map_err(map_sqlite_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(map_run_head_row_error)?;
    rows.into_iter()
        .map(|stored| decode_stored_run_head(session_id, stored))
        .collect()
}

fn load_projected_run_heads(
    connection: &Connection,
    session_id: &SessionId,
) -> StoreResult<HashMap<RunId, ProjectedRunHead>> {
    let metadata = ensure_run_head_projection(connection, session_id)?;
    match try_load_projected_run_heads(connection, session_id) {
        Ok(heads) => {
            if projected_run_head_counts(&heads)?
                == (metadata.run_count, metadata.nonterminal_count)
            {
                Ok(heads)
            } else {
                rebuild_run_head_projection(connection, session_id)?;
                try_load_projected_run_heads(connection, session_id)
            }
        }
        Err(error) if error.code == ErrorCode::StoreCorrupt => {
            rebuild_run_head_projection(connection, session_id)?;
            try_load_projected_run_heads(connection, session_id)
        }
        Err(error) => Err(error),
    }
}

fn try_load_projected_run_head(
    connection: &Connection,
    session_id: &SessionId,
    run_id: &RunId,
) -> StoreResult<Option<ProjectedRunHead>> {
    connection
        .query_row(
            "SELECT run_id, state_json, state_seq, terminal, accepted_seq,
                    branch_id, prompt_run_id, checksum
             FROM run_heads WHERE session_id = ?1 AND run_id = ?2",
            params![session_id.as_str(), run_id.as_str()],
            stored_run_head,
        )
        .optional()
        .map_err(map_run_head_row_error)?
        .map(|stored| decode_stored_run_head(session_id, stored).map(|(_, head)| head))
        .transpose()
}

fn projected_run_head(
    connection: &Connection,
    session_id: &SessionId,
    run_id: &RunId,
) -> StoreResult<Option<ProjectedRunHead>> {
    let metadata = ensure_run_head_projection(connection, session_id)?;
    match try_load_projected_run_head(connection, session_id, run_id) {
        Ok(Some(head)) => Ok(Some(head)),
        Ok(None) if metadata.run_count == 0 => Ok(None),
        Ok(None) => {
            rebuild_run_head_projection(connection, session_id)?;
            try_load_projected_run_head(connection, session_id, run_id)
        }
        Err(error) if error.code == ErrorCode::StoreCorrupt => {
            rebuild_run_head_projection(connection, session_id)?;
            try_load_projected_run_head(connection, session_id, run_id)
        }
        Err(error) => Err(error),
    }
}

fn latest_run_state(
    connection: &Connection,
    session_id: &SessionId,
    run_id: &RunId,
) -> StoreResult<Option<DurableRunHead>> {
    Ok(
        projected_run_head(connection, session_id, run_id)?.and_then(|head| {
            head.state_seq
                .map(|state_seq| (head.state, state_seq, head.branch_id))
        }),
    )
}

fn try_latest_run_states_for(
    connection: &Connection,
    session_id: &SessionId,
    run_ids: &HashSet<RunId>,
) -> StoreResult<(HashMap<RunId, DurableRunHead>, bool)> {
    let mut states = HashMap::new();
    let mut missing = false;
    for run_id in run_ids {
        let Some(head) = try_load_projected_run_head(connection, session_id, run_id)? else {
            missing = true;
            continue;
        };
        if let Some(state_seq) = head.state_seq {
            states.insert(run_id.clone(), (head.state, state_seq, head.branch_id));
        }
    }
    Ok((states, missing))
}

fn latest_run_states_for<'a>(
    connection: &Connection,
    session_id: &SessionId,
    run_ids: impl IntoIterator<Item = &'a RunId>,
    projection_run_count: u64,
) -> StoreResult<HashMap<RunId, DurableRunHead>> {
    let run_ids = run_ids.into_iter().cloned().collect::<HashSet<_>>();
    match try_latest_run_states_for(connection, session_id, &run_ids) {
        Ok((states, false)) => Ok(states),
        Ok((states, true)) if projection_run_count == 0 => Ok(states),
        Ok(_) => {
            rebuild_run_head_projection(connection, session_id)?;
            try_latest_run_states_for(connection, session_id, &run_ids).map(|(states, _)| states)
        }
        Err(error) if error.code == ErrorCode::StoreCorrupt => {
            rebuild_run_head_projection(connection, session_id)?;
            try_latest_run_states_for(connection, session_id, &run_ids).map(|(states, _)| states)
        }
        Err(error) => Err(error),
    }
}

fn try_load_one_nonterminal_run(
    connection: &Connection,
    session_id: &SessionId,
) -> StoreResult<Option<(RunId, ProjectedRunHead)>> {
    connection
        .query_row(
            "SELECT run_id, state_json, state_seq, terminal, accepted_seq,
                    branch_id, prompt_run_id, checksum
             FROM run_heads
             WHERE session_id = ?1 AND terminal = 0 AND state_seq IS NOT NULL
             ORDER BY state_seq DESC LIMIT 1",
            [session_id.as_str()],
            stored_run_head,
        )
        .optional()
        .map_err(map_run_head_row_error)?
        .map(|stored| decode_stored_run_head(session_id, stored))
        .transpose()
}

fn has_nonterminal_run(connection: &Connection, session_id: &SessionId) -> StoreResult<bool> {
    let metadata = ensure_run_head_projection(connection, session_id)?;
    let loaded = try_load_one_nonterminal_run(connection, session_id);
    match loaded {
        Ok(head) if head.is_some() == (metadata.nonterminal_count > 0) => Ok(head.is_some()),
        Ok(_) => {
            rebuild_run_head_projection(connection, session_id)?;
            Ok(try_load_one_nonterminal_run(connection, session_id)?.is_some())
        }
        Err(error) if error.code == ErrorCode::StoreCorrupt => {
            rebuild_run_head_projection(connection, session_id)?;
            Ok(try_load_one_nonterminal_run(connection, session_id)?.is_some())
        }
        Err(error) => Err(error),
    }
}

fn apply_run_head_envelope(
    heads: &mut HashMap<RunId, ProjectedRunHead>,
    envelope: &RawEnvelope,
) -> StoreResult<()> {
    let Some(run_id) = envelope.run_id.clone() else {
        return Ok(());
    };
    let Some(kind) = envelope
        .payload
        .get("type")
        .and_then(serde_json::Value::as_str)
    else {
        return Ok(());
    };
    if kind == "run_retried" {
        let Ok(RunRetryEventPayload::RunRetried { prompt_run_id, .. }) =
            RunRetryEventPayload::from_payload_value(envelope.payload.clone())
        else {
            return Ok(());
        };
        let head = heads
            .entry(run_id.clone())
            .or_insert_with(|| ProjectedRunHead {
                state: RunState::Queued,
                state_seq: None,
                accepted_seq: None,
                branch_id: envelope.branch_id.clone(),
                prompt_run_id: None,
            });
        if head.branch_id != envelope.branch_id {
            return Err(corrupt(format!(
                "run {run_id} crosses branch scopes in durable history"
            )));
        }
        head.accepted_seq = Some(envelope.seq);
        head.prompt_run_id = Some(prompt_run_id);
        return Ok(());
    }
    if !matches!(kind, "run_state" | "user_message") {
        return Ok(());
    }
    let Ok(payload) = serde_json::from_value::<EventPayload>(envelope.payload.clone()) else {
        return Ok(());
    };
    match payload {
        EventPayload::RunState(state) => {
            let head = heads
                .entry(run_id.clone())
                .or_insert_with(|| ProjectedRunHead {
                    state: state.clone(),
                    state_seq: None,
                    accepted_seq: None,
                    branch_id: envelope.branch_id.clone(),
                    prompt_run_id: None,
                });
            if head.branch_id != envelope.branch_id {
                return Err(corrupt(format!(
                    "run {run_id} crosses branch scopes in durable history"
                )));
            }
            head.state = state;
            head.state_seq = Some(envelope.seq);
        }
        EventPayload::UserMessage { .. } => {
            let head = heads
                .entry(run_id.clone())
                .or_insert_with(|| ProjectedRunHead {
                    state: RunState::Queued,
                    state_seq: None,
                    accepted_seq: None,
                    branch_id: envelope.branch_id.clone(),
                    prompt_run_id: None,
                });
            if head.branch_id != envelope.branch_id {
                return Err(corrupt(format!(
                    "run {run_id} crosses branch scopes in durable history"
                )));
            }
            head.accepted_seq = Some(envelope.seq);
        }
        _ => {}
    }
    Ok(())
}

fn write_projected_run_head(
    connection: &Connection,
    session_id: &SessionId,
    run_id: &RunId,
    head: &ProjectedRunHead,
) -> StoreResult<()> {
    let state_json = serde_json::to_string(&head.state).map_err(|error| {
        corrupt(format!(
            "cannot serialize run-head state for session {session_id}, run {run_id}: {error}"
        ))
    })?;
    let terminal = head.state.is_terminal();
    let checksum = run_head_checksum(
        session_id,
        run_id.as_str(),
        &state_json,
        head.state_seq,
        terminal,
        head.accepted_seq,
        head.branch_id.as_ref().map(BranchId::as_str),
        head.prompt_run_id.as_ref().map(RunId::as_str),
    );
    connection
        .execute(
            "INSERT INTO run_heads(
                 session_id, run_id, state_json, state_seq, terminal,
                 accepted_seq, branch_id, prompt_run_id, checksum
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(session_id, run_id) DO UPDATE SET
                 state_json = excluded.state_json,
                 state_seq = excluded.state_seq,
                 terminal = excluded.terminal,
                 accepted_seq = excluded.accepted_seq,
                 branch_id = excluded.branch_id,
                 prompt_run_id = excluded.prompt_run_id,
                 checksum = excluded.checksum",
            params![
                session_id.as_str(),
                run_id.as_str(),
                state_json,
                head.state_seq.map(to_sqlite_integer).transpose()?,
                i64::from(u8::from(terminal)),
                head.accepted_seq.map(to_sqlite_integer).transpose()?,
                head.branch_id.as_ref().map(BranchId::as_str),
                head.prompt_run_id.as_ref().map(RunId::as_str),
                checksum.as_bytes().as_slice(),
            ],
        )
        .map_err(map_sqlite_error)?;
    Ok(())
}

fn replace_run_head_projection(
    connection: &Connection,
    session_id: &SessionId,
    through_seq: u64,
    heads: &HashMap<RunId, ProjectedRunHead>,
) -> StoreResult<()> {
    connection
        .execute(
            "DELETE FROM run_heads WHERE session_id = ?1",
            [session_id.as_str()],
        )
        .map_err(map_sqlite_error)?;
    for (run_id, head) in heads {
        write_projected_run_head(connection, session_id, run_id, head)?;
    }
    let (run_count, nonterminal_count) = projected_run_head_counts(heads)?;
    connection
        .execute(
            "INSERT INTO run_head_sessions(
                 session_id, through_seq, run_count, nonterminal_count
             ) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(session_id) DO UPDATE SET
                 through_seq = excluded.through_seq,
                 run_count = excluded.run_count,
                 nonterminal_count = excluded.nonterminal_count",
            params![
                session_id.as_str(),
                to_sqlite_integer(through_seq)?,
                to_sqlite_integer(run_count)?,
                to_sqlite_integer(nonterminal_count)?,
            ],
        )
        .map_err(map_sqlite_error)?;
    Ok(())
}

fn rebuild_run_head_projection(connection: &Connection, session_id: &SessionId) -> StoreResult<()> {
    let derived = (|| {
        let mut statement = connection
            .prepare_cached(
                "SELECT seq, envelope_json, event_id, committed_at_ms
                 FROM events WHERE session_id = ?1 ORDER BY seq ASC",
            )
            .map_err(map_sqlite_error)?;
        let mut rows = statement
            .query([session_id.as_str()])
            .map_err(map_sqlite_error)?;
        let mut through_seq = 0;
        let mut heads = HashMap::new();
        while let Some(row) = rows.next().map_err(map_sqlite_error)? {
            let stored_seq = row.get::<_, i64>(0).map_err(map_sqlite_error)?;
            let stored_event_id = row.get::<_, String>(2).map_err(map_sqlite_error)?;
            let stored_committed_at_ms = row.get::<_, i64>(3).map_err(map_sqlite_error)?;
            let envelope = decode_envelope_column(row, 1).map_err(|error| {
                corrupt(format!(
                    "invalid envelope for run-head rebuild in session {session_id}, seq {stored_seq}: {error}"
                ))
            })?;
            validate_stored_envelope(
                session_id,
                stored_seq,
                &stored_event_id,
                stored_committed_at_ms,
                &envelope,
            )?;
            through_seq = envelope.seq;
            apply_run_head_envelope(&mut heads, &envelope)?;
        }
        Ok((through_seq, heads))
    })()?;

    // Append, group-commit, fork, and backfill callers already own a rollback
    // boundary. Autocommit read-side repair needs its own atomic savepoint.
    if !connection.is_autocommit() {
        return replace_run_head_projection(connection, session_id, derived.0, &derived.1);
    }
    connection
        .execute_batch("SAVEPOINT rebuild_run_heads")
        .map_err(map_sqlite_error)?;
    match replace_run_head_projection(connection, session_id, derived.0, &derived.1) {
        Ok(()) => connection
            .execute_batch("RELEASE rebuild_run_heads")
            .map_err(map_sqlite_error),
        Err(error) => {
            let _ = connection
                .execute_batch("ROLLBACK TO rebuild_run_heads; RELEASE rebuild_run_heads");
            Err(error)
        }
    }
}

fn update_run_head_projection_after_append(
    connection: &Connection,
    session_id: &SessionId,
    envelopes: &[RawEnvelope],
) -> StoreResult<()> {
    let Some(last) = envelopes.last() else {
        return Ok(());
    };
    let metadata = run_head_projection_meta(connection, session_id)?;
    let expected_previous = envelopes
        .first()
        .and_then(|envelope| envelope.seq.checked_sub(1));
    let Some(metadata) =
        metadata.filter(|metadata| Some(metadata.through_seq) == expected_previous)
    else {
        return rebuild_run_head_projection(connection, session_id);
    };
    let mut original = HashMap::<RunId, Option<ProjectedRunHead>>::new();
    let mut changed = HashMap::<RunId, ProjectedRunHead>::new();
    for envelope in envelopes {
        let Some(run_id) = envelope.run_id.as_ref() else {
            continue;
        };
        let changes_projection = matches!(
            envelope
                .payload
                .get("type")
                .and_then(serde_json::Value::as_str),
            Some("run_retried" | "run_state" | "user_message")
        );
        if !changes_projection {
            continue;
        }
        if !original.contains_key(run_id) {
            let head = match try_load_projected_run_head(connection, session_id, run_id) {
                Ok(head) => head,
                Err(error) if error.code == ErrorCode::StoreCorrupt => {
                    return rebuild_run_head_projection(connection, session_id);
                }
                Err(error) => return Err(error),
            };
            original.insert(run_id.clone(), head.clone());
            if let Some(head) = head {
                changed.insert(run_id.clone(), head);
            }
        }
        apply_run_head_envelope(&mut changed, envelope)?;
    }

    let mut run_count = metadata.run_count;
    let mut nonterminal_count = metadata.nonterminal_count;
    for (run_id, head) in &changed {
        let previous = original.get(run_id).and_then(Option::as_ref);
        if previous.is_none() {
            run_count = run_count
                .checked_add(1)
                .ok_or_else(|| corrupt("run-head row count is exhausted"))?;
        }
        if previous.is_some_and(|head| head.state_seq.is_some() && !head.state.is_terminal()) {
            nonterminal_count = nonterminal_count
                .checked_sub(1)
                .ok_or_else(|| corrupt("run-head live count is inconsistent"))?;
        }
        if head.state_seq.is_some() && !head.state.is_terminal() {
            nonterminal_count = nonterminal_count
                .checked_add(1)
                .ok_or_else(|| corrupt("run-head live count is exhausted"))?;
        }
        write_projected_run_head(connection, session_id, run_id, head)?;
    }
    connection
        .execute(
            "UPDATE run_head_sessions
             SET through_seq = ?2, run_count = ?3, nonterminal_count = ?4
             WHERE session_id = ?1",
            params![
                session_id.as_str(),
                to_sqlite_integer(last.seq)?,
                to_sqlite_integer(run_count)?,
                to_sqlite_integer(nonterminal_count)?,
            ],
        )
        .map_err(map_sqlite_error)?;
    Ok(())
}

fn backfill_run_head_projections(connection: &mut Connection) -> StoreResult<()> {
    let session_ids = {
        let mut statement = connection
            .prepare(
                "SELECT session.id
                 FROM sessions AS session
                 LEFT JOIN run_head_sessions AS projection
                   ON projection.session_id = session.id
                 WHERE projection.session_id IS NULL
                    OR projection.through_seq != COALESCE(
                        (SELECT MAX(event.seq) FROM events AS event
                         WHERE event.session_id = session.id),
                        0
                    )
                 ORDER BY session.id",
            )
            .map_err(map_sqlite_error)?;
        let session_ids = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(map_sqlite_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_sqlite_error)?;
        drop(statement);
        session_ids
    };
    for session_id in session_ids {
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        rebuild_run_head_projection(&transaction, &SessionId::new(session_id))?;
        transaction.commit().map_err(map_sqlite_error)?;
    }
    Ok(())
}

fn run_heads_projection_checkpoint(
    connection: &Connection,
    session_id: &SessionId,
) -> StoreResult<SessionProjectionCheckpoint> {
    require_session(connection, session_id)?;
    let heads = load_projected_run_heads(connection, session_id)?;
    let through_seq = journal_latest_seq(connection, session_id)?;
    if through_seq == 0 {
        return Err(corrupt(format!(
            "session {session_id} has no journal boundary for its run-head projection"
        )));
    }
    let boundary_event_id = connection
        .query_row(
            "SELECT event_id FROM events WHERE session_id = ?1 AND seq = ?2",
            params![session_id.as_str(), to_sqlite_integer(through_seq)?],
            |row| row.get::<_, String>(0),
        )
        .map_err(map_sqlite_error)?;
    let mut rows = heads
        .into_iter()
        .map(|(run_id, head)| RunHeadCheckpointRow {
            run_id: run_id.as_str().to_owned(),
            state: head.state,
            state_seq: head.state_seq,
            accepted_seq: head.accepted_seq,
            branch_id: head
                .branch_id
                .map(|branch_id| branch_id.as_str().to_owned()),
            prompt_run_id: head
                .prompt_run_id
                .map(|prompt_run_id| prompt_run_id.as_str().to_owned()),
        })
        .collect::<Vec<_>>();
    rows.sort_by(|left, right| {
        left.accepted_seq
            .unwrap_or(u64::MAX)
            .cmp(&right.accepted_seq.unwrap_or(u64::MAX))
            .then_with(|| left.run_id.cmp(&right.run_id))
    });
    let payload = rmp_serde::to_vec_named(&RunHeadsCheckpointPayload {
        version: RUN_HEADS_PAYLOAD_VERSION,
        heads: rows,
    })
    .map_err(|error| {
        corrupt(format!(
            "cannot encode run-head projection for session {session_id}: {error}"
        ))
    })?;
    Ok(SessionProjectionCheckpoint {
        session_id: session_id.clone(),
        projection: RUN_HEADS_PROJECTION.to_owned(),
        timeline_key: RUN_HEADS_TIMELINE.to_owned(),
        through_seq,
        boundary_event_id: EventId::new(boundary_event_id),
        payload,
    })
}

#[derive(Clone)]
struct QueuedEntry {
    row: QueueRow,
    run_id: RunId,
    branch_id: Option<BranchId>,
    accepted_seq: u64,
}

fn queue_entries(
    connection: &Connection,
    session_id: &SessionId,
) -> StoreResult<(u64, Vec<QueuedEntry>)> {
    let states = latest_run_states(connection, session_id)?;
    let mut statement = connection
        .prepare_cached(
            "SELECT seq, envelope_json
             FROM events INDEXED BY events_payload_kind_session_seq
             WHERE session_id = ?1
               AND (
                   payload_kind IN (?2, ?3)
                   OR payload_kind IS NULL
               )
             ORDER BY seq ASC",
        )
        .map_err(map_sqlite_error)?;
    let mut rows = statement
        .query(params![
            session_id.as_str(),
            QUEUE_REDUCER_PAYLOAD_KINDS[0],
            QUEUE_REDUCER_PAYLOAD_KINDS[1]
        ])
        .map_err(map_sqlite_error)?;
    let mut revision = 0_u64;
    let mut messages = HashMap::<RunId, QueuedEntry>::new();
    let mut held_ids = HashSet::<EventId>::new();
    while let Some(row) = rows.next().map_err(map_sqlite_error)? {
        let seq = sql_u64(row.get(0).map_err(map_sqlite_error)?).map_err(map_sqlite_error)?;
        let envelope = decode_envelope_column(row, 1).map_err(|error| {
            corrupt(format!(
                "invalid queue envelope for session {session_id}, seq {seq}: {error}"
            ))
        })?;
        match decode_payload::<EventPayload>(&envelope.payload) {
            Ok(EventPayload::UserMessage { text, mode, .. }) => {
                let Some(run_id) = envelope.run_id.clone() else {
                    continue;
                };
                if states
                    .get(&run_id)
                    .is_some_and(|(state, _, _)| *state == RunState::Queued)
                {
                    messages.entry(run_id.clone()).or_insert(QueuedEntry {
                        row: QueueRow {
                            id: envelope.event_id,
                            text,
                            mode,
                            ordinal: 0,
                            created_at_ms: envelope.committed_at_ms,
                        },
                        run_id,
                        branch_id: envelope.branch_id,
                        accepted_seq: seq,
                    });
                }
            }
            Ok(EventPayload::QueueChanged(delta)) => {
                revision = revision.max(delta.revision.max(seq));
                match delta.change {
                    QueueChange::Enqueued { row } => {
                        held_ids.insert(row.id);
                    }
                    QueueChange::Removed { id }
                    | QueueChange::PromotedSteer { id }
                    | QueueChange::Consumed { id } => {
                        held_ids.remove(&id);
                    }
                    QueueChange::Unknown => {}
                    _ => {}
                }
            }
            _ => {}
        }
    }
    let mut entries = messages
        .into_values()
        .filter(|entry| held_ids.contains(&entry.row.id))
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.accepted_seq);
    for (index, entry) in entries.iter_mut().enumerate() {
        entry.row.ordinal = u32::try_from(index)
            .ok()
            .and_then(|index| index.checked_add(1))
            .ok_or_else(|| {
                store_error(ErrorCode::Busy, "queue ordinal space is exhausted", true)
            })?;
    }
    Ok((revision, entries))
}

fn queue_revision_conflict(expected_revision: u64, current_revision: u64) -> HaiderError {
    let mut error = store_error(
        ErrorCode::RevisionConflict,
        format!(
            "queue revision {expected_revision} is stale; current revision is {current_revision}"
        ),
        true,
    );
    error.details = Some(serde_json::json!({
        "expected_revision": expected_revision,
        "current_revision": current_revision,
    }));
    error
}

fn queue_promote_target(
    connection: &Connection,
    command: &QueuePromoteCommand,
) -> StoreResult<(QueuedEntry, RunId, Option<BranchId>)> {
    let (current_revision, entries) = queue_entries(connection, &command.session_id)?;
    // MUTATION CHECK: this fence is shared by preview and commit. Removing it
    // lets a stale id reservation target a row after another mutation moved
    // the queue, and the stale-refusal pin must fail.
    if command.revision != current_revision {
        return Err(queue_revision_conflict(command.revision, current_revision));
    }
    let entry = entries
        .into_iter()
        .find(|entry| entry.row.id == command.id)
        .ok_or_else(|| {
            store_error(
                ErrorCode::InvalidArgument,
                format!("queue item {} is not held", command.id),
                false,
            )
        })?;
    let states = latest_run_states(connection, &command.session_id)?;
    let (active_run_id, active_branch_id) = states
        .iter()
        .filter(|(run_id, (state, _, _))| {
            **run_id != entry.run_id
                && !state.is_terminal()
                && !matches!(
                    state,
                    RunState::Queued
                        | RunState::Cancelling
                        | RunState::Compacting
                        | RunState::EffectOutcomeUnknown
                )
        })
        .max_by_key(|(_, (_, seq, _))| *seq)
        .map(|(run_id, (_, _, branch_id))| (run_id.clone(), branch_id.clone()))
        .ok_or_else(|| {
            store_error(
                ErrorCode::RunNotActive,
                "queue promotion requires an active turn",
                false,
            )
        })?;
    if command
        .expected_active_run_id
        .as_ref()
        .is_some_and(|expected| expected != &active_run_id)
    {
        return Err(store_error(
            ErrorCode::RunNotActive,
            "active turn changed before queue promotion committed",
            true,
        ));
    }
    Ok((entry, active_run_id, active_branch_id))
}

/// Resolves the exact main-timeline `Retrying` event currently named by the
/// run-state reduction. Returning `None` keeps branch/agent backoffs outside
/// the main-session `run.retry` command family.
fn main_timeline_retrying_event_id(
    connection: &Connection,
    session_id: &SessionId,
    run_id: &RunId,
    state_seq: u64,
) -> StoreResult<Option<EventId>> {
    let mut statement = connection
        .prepare_cached(
            "SELECT envelope_json FROM events
             WHERE session_id = ?1 AND seq = ?2",
        )
        .map_err(map_sqlite_error)?;
    let stored_seq = i64::try_from(state_seq)
        .map_err(|_| corrupt("event sequence exceeds SQLite INTEGER range"))?;
    let mut rows = statement
        .query(rusqlite::params![session_id.as_str(), stored_seq])
        .map_err(map_sqlite_error)?;
    let Some(row) = rows.next().map_err(map_sqlite_error)? else {
        return Ok(None);
    };
    let envelope = decode_envelope_column(row, 0).map_err(|error| {
        corrupt(format!(
            "invalid envelope JSON for session {session_id}, seq {state_seq}: {error}"
        ))
    })?;
    let main_timeline = envelope.branch_id.is_none() && envelope.agent_id.is_none();
    let exact_run = envelope.run_id.as_ref() == Some(run_id);
    let retrying = serde_json::from_value::<EventPayload>(envelope.payload)
        .is_ok_and(|payload| matches!(payload, EventPayload::RunState(RunState::Retrying { .. })));
    Ok((main_timeline && exact_run && retrying).then_some(envelope.event_id))
}

/// Finds the immutable user/prompt source for one main-timeline run. Ordinary
/// runs own their `UserMessage`; fresh manual-retry runs own a `RunRetried`
/// fact that points back to the original prompt ancestry.
fn main_timeline_run_prompt_source(
    connection: &Connection,
    session_id: &SessionId,
    target_run_id: &RunId,
) -> StoreResult<Option<(RunId, u64)>> {
    let mut statement = connection
        .prepare_cached(
            "SELECT seq, envelope_json
             FROM events INDEXED BY events_payload_kind_session_seq
             WHERE session_id = ?1
               AND (payload_kind IN (?2, ?3) OR payload_kind IS NULL)
             ORDER BY seq ASC",
        )
        .map_err(map_sqlite_error)?;
    let mut rows = statement
        .query(params![
            session_id.as_str(),
            RUN_PROMPT_SOURCE_PAYLOAD_KINDS[0],
            RUN_PROMPT_SOURCE_PAYLOAD_KINDS[1]
        ])
        .map_err(map_sqlite_error)?;
    let mut source = None;
    while let Some(row) = rows.next().map_err(map_sqlite_error)? {
        let seq = sql_u64(row.get(0).map_err(map_sqlite_error)?).map_err(map_sqlite_error)?;
        let envelope = decode_envelope_column(row, 1).map_err(|error| {
            corrupt(format!(
                "invalid envelope JSON for session {session_id}, seq {seq}: {error}"
            ))
        })?;
        if envelope.run_id.as_ref() != Some(target_run_id)
            || envelope.branch_id.is_some()
            || envelope.agent_id.is_some()
        {
            continue;
        }
        let payload = &envelope.payload;
        // `NULL` legacy rows are deliberately over-selected. Keep the exact
        // type fence before decoding them into either supported union.
        if !matches!(
            payload.get("type").and_then(serde_json::Value::as_str),
            Some("user_message" | "run_retried")
        ) {
            continue;
        }
        match payload.get("type").and_then(serde_json::Value::as_str) {
            Some("user_message")
                if decode_payload::<EventPayload>(payload)
                    .is_ok_and(|payload| matches!(payload, EventPayload::UserMessage { .. })) =>
            {
                source = Some((target_run_id.clone(), seq));
            }
            Some("run_retried") => {
                if let Ok(RunRetryEventPayload::RunRetried {
                    prompt_run_id,
                    user_seq,
                    ..
                }) = decode_payload::<RunRetryEventPayload>(payload)
                {
                    source = Some((prompt_run_id, user_seq));
                }
            }
            _ => {}
        }
    }
    Ok(source)
}

/// Returns the latest main-timeline user coordinate only when that exact
/// run owns both the latest terminal `Errored` state and a durable
/// `RunFailed` cause. A later successful/cancelled user turn therefore
/// makes an older failure ineligible for manual retry.
fn latest_main_timeline_failed_turn(
    connection: &Connection,
    session_id: &SessionId,
) -> StoreResult<Option<(RunId, RunId, u64)>> {
    let states = latest_run_states(connection, session_id)?;
    let mut statement = connection
        .prepare_cached(
            "SELECT seq, envelope_json
             FROM events INDEXED BY events_payload_kind_session_seq
             WHERE session_id = ?1
               AND (payload_kind IN (?2, ?3, ?4) OR payload_kind IS NULL)
             ORDER BY seq ASC",
        )
        .map_err(map_sqlite_error)?;
    let mut rows = statement
        .query(params![
            session_id.as_str(),
            FAILED_TURN_REDUCER_PAYLOAD_KINDS[0],
            FAILED_TURN_REDUCER_PAYLOAD_KINDS[1],
            FAILED_TURN_REDUCER_PAYLOAD_KINDS[2]
        ])
        .map_err(map_sqlite_error)?;
    let mut latest_user = None::<(RunId, u64)>;
    let mut retries = Vec::<(RunId, RunId, u64)>::new();
    let mut failed = HashSet::<RunId>::new();
    while let Some(row) = rows.next().map_err(map_sqlite_error)? {
        let seq = sql_u64(row.get(0).map_err(map_sqlite_error)?).map_err(map_sqlite_error)?;
        let envelope = decode_envelope_column(row, 1).map_err(|error| {
            corrupt(format!(
                "invalid envelope JSON for session {session_id}, seq {seq}: {error}"
            ))
        })?;
        let Some(run_id) = envelope.run_id else {
            continue;
        };
        let main_timeline = envelope.branch_id.is_none() && envelope.agent_id.is_none();
        let payload = &envelope.payload;
        // `NULL` legacy rows are deliberately over-selected. Keep the exact
        // type fence before decoding them into either supported union.
        if !matches!(
            payload.get("type").and_then(serde_json::Value::as_str),
            Some("user_message" | "run_failed" | "run_retried")
        ) {
            continue;
        }
        match payload.get("type").and_then(serde_json::Value::as_str) {
            Some("user_message")
                if main_timeline
                    && decode_payload::<EventPayload>(payload).is_ok_and(|payload| {
                        matches!(payload, EventPayload::UserMessage { .. })
                    }) =>
            {
                latest_user = Some((run_id, seq));
            }
            Some("run_failed")
                if main_timeline
                    && decode_payload::<EventPayload>(payload)
                        .is_ok_and(|payload| matches!(payload, EventPayload::RunFailed { .. })) =>
            {
                failed.insert(run_id);
            }
            Some("run_retried") if main_timeline => {
                if let Ok(RunRetryEventPayload::RunRetried {
                    prompt_run_id,
                    user_seq,
                    ..
                }) = decode_payload::<RunRetryEventPayload>(payload)
                {
                    retries.push((run_id, prompt_run_id, user_seq));
                }
            }
            _ => {}
        }
    }
    let Some((user_run_id, user_seq)) = latest_user else {
        return Ok(None);
    };
    let (run_id, prompt_run_id) = retries
        .into_iter()
        .rfind(|(_, _, retried_user_seq)| *retried_user_seq == user_seq)
        .map_or_else(
            || (user_run_id.clone(), user_run_id),
            |(retry_run_id, prompt_run_id, _)| (retry_run_id, prompt_run_id),
        );
    let eligible = states.get(&run_id).is_some_and(|(state, _, branch_id)| {
        *state == RunState::Errored && branch_id.is_none() && failed.contains(&run_id)
    });
    Ok(eligible.then_some((run_id, prompt_run_id, user_seq)))
}

fn latest_tree_head(
    connection: &Connection,
    session_id: &SessionId,
    branch_id: Option<&haider_protocol::ids::BranchId>,
    agent_id: Option<&AgentId>,
) -> StoreResult<Option<NodeId>> {
    let mut statement = connection
        .prepare_cached(
            "SELECT seq, envelope_json
             FROM events INDEXED BY events_payload_kind_session_seq
             WHERE session_id = ?1
               AND (payload_kind = ?2 OR payload_kind IS NULL)
             ORDER BY seq DESC",
        )
        .map_err(map_sqlite_error)?;
    let mut rows = statement
        .query(params![
            session_id.as_str(),
            TREE_HEAD_REDUCER_PAYLOAD_KINDS[0]
        ])
        .map_err(map_sqlite_error)?;
    while let Some(row) = rows.next().map_err(map_sqlite_error)? {
        let seq: i64 = row.get(0).map_err(map_sqlite_error)?;
        let envelope = decode_envelope_column(row, 1).map_err(|error| {
            corrupt(format!(
                "invalid envelope JSON for session {session_id}, seq {seq}: {error}"
            ))
        })?;
        if envelope.branch_id.as_ref() != branch_id || envelope.agent_id.as_ref() != agent_id {
            continue;
        }
        if let Ok(EventPayload::NodeCommitted(node)) =
            serde_json::from_value::<EventPayload>(envelope.payload)
        {
            return Ok(Some(node.node));
        }
    }
    Ok(None)
}

#[allow(clippy::too_many_arguments)]
fn unstamped_command_envelope(
    event_id: EventId,
    session_id: &SessionId,
    branch_id: Option<BranchId>,
    run_id: Option<RunId>,
    device_id: DeviceId,
    worker_generation: u64,
    payload: EventPayload,
    prompt: PromptRender,
) -> StoreResult<RawEnvelope> {
    let payload = serde_json::to_value(payload).map_err(|error| {
        store_error(
            ErrorCode::InvalidArgument,
            format!("cannot serialize command envelope payload: {error}"),
            false,
        )
    })?;
    unstamped_raw_command_envelope(
        event_id,
        session_id,
        branch_id,
        run_id,
        device_id,
        worker_generation,
        payload,
        prompt,
    )
}

#[allow(clippy::too_many_arguments)]
fn unstamped_raw_command_envelope(
    event_id: EventId,
    session_id: &SessionId,
    branch_id: Option<BranchId>,
    run_id: Option<RunId>,
    device_id: DeviceId,
    worker_generation: u64,
    payload: serde_json::Value,
    prompt: PromptRender,
) -> StoreResult<RawEnvelope> {
    Ok(EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id,
        seq: 0,
        session_id: session_id.clone(),
        branch_id,
        run_id,
        agent_id: None,
        device_id,
        authority_epoch: 0,
        worker_generation,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt,
        },
        payload,
    })
}

fn workflow_graph_journal_event(
    payload: &serde_json::Value,
) -> StoreResult<Option<WorkflowGraphJournalEvent>> {
    WorkflowGraphJournalEvent::from_payload_value(payload).map_err(|error| {
        corrupt(format!(
            "malformed workflow activation journal event: {error}"
        ))
    })
}

fn workflow_event_graph_id(event: &WorkflowGraphJournalEvent) -> &GraphId {
    match event {
        WorkflowGraphJournalEvent::WorkflowGraphStarted(started) => &started.graph_id,
        WorkflowGraphJournalEvent::WorkflowNodeActivated(activated) => &activated.graph_id,
        WorkflowGraphJournalEvent::WorkflowNodeCompleted(completed) => &completed.graph_id,
        WorkflowGraphJournalEvent::WorkflowNodeRejected(rejected) => &rejected.graph_id,
    }
}

fn load_workflow_graph_state(
    connection: &Connection,
    session_id: &SessionId,
    graph_id: Option<&GraphId>,
) -> StoreResult<Option<WorkflowGraphState>> {
    let selected = match graph_id {
        Some(graph_id) => connection
            .query_row(
                "SELECT graph_id, through_seq, phase, input_ready, state_json
                 FROM workflow_graph_instances
                 WHERE session_id = ?1 AND graph_id = ?2",
                params![session_id.as_str(), graph_id.as_str()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                },
            )
            .optional()
            .map_err(map_sqlite_error)?,
        None => connection
            .query_row(
                "SELECT graph_id, through_seq, phase, input_ready, state_json
                 FROM workflow_graph_instances
                 WHERE session_id = ?1 ORDER BY through_seq DESC, graph_id ASC LIMIT 1",
                [session_id.as_str()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                },
            )
            .optional()
            .map_err(map_sqlite_error)?,
    };
    let Some((stored_graph_id, stored_through_cursor, stored_phase, stored_input_ready, json)) =
        selected
    else {
        return Ok(None);
    };
    let state: WorkflowGraphState = serde_json::from_str(&json)
        .map_err(|error| corrupt(format!("workflow graph projection is invalid: {error}")))?;
    let stored_through_cursor = u64::try_from(stored_through_cursor)
        .map_err(|_| corrupt("workflow graph projection has a negative cursor"))?;
    state.validate_projection().map_err(|error| {
        corrupt(format!(
            "workflow graph projection is inconsistent: {error}"
        ))
    })?;
    if stored_graph_id != state.graph_id.as_str()
        || stored_through_cursor != state.through_cursor
        || stored_phase != workflow_graph_phase_sql(state.phase)
        || stored_input_ready != i64::from(state.seed.is_some())
    {
        return Err(corrupt(
            "workflow graph projection index disagrees with its typed state",
        ));
    }
    let mut statement = connection
        .prepare_cached(
            "SELECT node, phase, iteration, updated_seq, state_json
             FROM workflow_node_states
             WHERE session_id = ?1 AND graph_id = ?2 ORDER BY node ASC",
        )
        .map_err(map_sqlite_error)?;
    let rows = statement
        .query_map(
            params![session_id.as_str(), state.graph_id.as_str()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                ))
            },
        )
        .map_err(map_sqlite_error)?;
    let mut by_node = HashMap::new();
    for row in rows {
        let (stored_node, stored_phase, stored_iteration, stored_cursor, json) =
            row.map_err(map_sqlite_error)?;
        let node: haider_protocol::graph::WorkflowNodeState = serde_json::from_str(&json)
            .map_err(|error| corrupt(format!("workflow node projection is invalid: {error}")))?;
        let stored_iteration = u32::try_from(stored_iteration)
            .map_err(|_| corrupt("workflow node projection has an invalid iteration"))?;
        let stored_cursor = u64::try_from(stored_cursor)
            .map_err(|_| corrupt("workflow node projection has a negative cursor"))?;
        if stored_node != node.node.as_str()
            || stored_phase != workflow_node_phase_sql(node.phase)
            || stored_iteration != node.iteration
            || stored_cursor != node.updated_cursor
        {
            return Err(corrupt(
                "workflow node projection index disagrees with its typed state",
            ));
        }
        if by_node.insert(node.node.clone(), node).is_some() {
            return Err(corrupt(
                "workflow node projection contains a duplicate node",
            ));
        }
    }
    if by_node.len() != state.ast.nodes.len() {
        return Err(corrupt(
            "workflow node projection does not cover the activation AST",
        ));
    }
    let indexed_nodes = state
        .ast
        .nodes
        .iter()
        .map(|spec| {
            by_node
                .remove(&spec.node)
                .ok_or_else(|| corrupt("workflow node projection omitted an AST node"))
        })
        .collect::<StoreResult<Vec<_>>>()?;
    if indexed_nodes != state.nodes {
        return Err(corrupt(
            "workflow graph and node projection snapshots disagree",
        ));
    }
    Ok(Some(state))
}

fn load_seedless_active_workflow_graph_state(
    connection: &Connection,
    session_id: &SessionId,
) -> StoreResult<Option<WorkflowGraphState>> {
    let graph_id = connection
        .query_row(
            "SELECT graph_id FROM workflow_graph_instances
             WHERE session_id = ?1 AND phase = 'active' AND input_ready = 0
             ORDER BY through_seq DESC, graph_id ASC LIMIT 1",
            [session_id.as_str()],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(map_sqlite_error)?
        .map(GraphId::new);
    graph_id
        .as_ref()
        .map(|graph_id| load_workflow_graph_state(connection, session_id, Some(graph_id)))
        .transpose()
        .map(Option::flatten)
}

fn workflow_graph_phase_sql(phase: WorkflowGraphPhase) -> &'static str {
    match phase {
        WorkflowGraphPhase::Active => "active",
        WorkflowGraphPhase::Completed => "completed",
        WorkflowGraphPhase::Rejected => "rejected",
    }
}

fn workflow_node_phase_sql(phase: WorkflowNodePhase) -> &'static str {
    match phase {
        WorkflowNodePhase::Waiting => "waiting",
        WorkflowNodePhase::Activated => "activated",
        WorkflowNodePhase::Completed => "completed",
        WorkflowNodePhase::Rejected => "rejected",
    }
}

fn persist_workflow_graph_state(
    transaction: &Connection,
    session_id: &SessionId,
    state: &WorkflowGraphState,
) -> StoreResult<()> {
    let state_json = serde_json::to_string(state).map_err(|error| {
        store_error(
            ErrorCode::InvalidArgument,
            format!("cannot encode workflow graph projection: {error}"),
            false,
        )
    })?;
    transaction
        .execute(
            "INSERT INTO workflow_graph_instances(
                 session_id, graph_id, through_seq, phase, input_ready, state_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(session_id, graph_id) DO UPDATE SET
                 through_seq = excluded.through_seq,
                 phase = excluded.phase,
                 input_ready = excluded.input_ready,
                 state_json = excluded.state_json
             WHERE excluded.through_seq > workflow_graph_instances.through_seq",
            params![
                session_id.as_str(),
                state.graph_id.as_str(),
                to_sqlite_integer(state.through_cursor)?,
                workflow_graph_phase_sql(state.phase),
                state.seed.is_some(),
                state_json,
            ],
        )
        .map_err(map_sqlite_error)?;
    let mut upsert = transaction
        .prepare_cached(
            "INSERT INTO workflow_node_states(
                 session_id, graph_id, node, phase, iteration, updated_seq, state_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(session_id, graph_id, node) DO UPDATE SET
                 phase = excluded.phase,
                 iteration = excluded.iteration,
                 updated_seq = excluded.updated_seq,
                 state_json = excluded.state_json
             WHERE excluded.updated_seq >= workflow_node_states.updated_seq",
        )
        .map_err(map_sqlite_error)?;
    // The graph snapshot advances every fact, but only nodes stamped at this
    // cursor changed. Start events stamp all nodes; ordinary transitions
    // update one node, while back edges also stamp the reset descendants.
    for node in state
        .nodes
        .iter()
        .filter(|node| node.updated_cursor == state.through_cursor)
    {
        let node_json = serde_json::to_string(node).map_err(|error| {
            store_error(
                ErrorCode::InvalidArgument,
                format!("cannot encode workflow node projection: {error}"),
                false,
            )
        })?;
        upsert
            .execute(params![
                session_id.as_str(),
                state.graph_id.as_str(),
                node.node.as_str(),
                workflow_node_phase_sql(node.phase),
                i64::from(node.iteration),
                to_sqlite_integer(node.updated_cursor)?,
                node_json,
            ])
            .map_err(map_sqlite_error)?;
    }
    Ok(())
}

fn update_workflow_graph_projection_after_append(
    transaction: &Connection,
    session_id: &SessionId,
    envelopes: &[RawEnvelope],
) -> StoreResult<()> {
    let mut states = HashMap::<GraphId, WorkflowGraphState>::new();
    for envelope in envelopes {
        let Some(event) = workflow_graph_journal_event(&envelope.payload)? else {
            continue;
        };
        let graph_id = workflow_event_graph_id(&event).clone();
        if let WorkflowGraphJournalEvent::WorkflowGraphStarted(started) = event {
            if load_workflow_graph_state(transaction, session_id, Some(&graph_id))?.is_some() {
                return Err(corrupt(format!(
                    "workflow graph {graph_id} was durably started more than once"
                )));
            }
            let state = WorkflowGraphState::from_started(envelope.seq, *started)
                .map_err(|error| corrupt(format!("cannot reduce workflow graph start: {error}")))?;
            persist_workflow_graph_state(transaction, session_id, &state)?;
            states.insert(graph_id, state);
            continue;
        }
        if !states.contains_key(&graph_id) {
            let state = load_workflow_graph_state(transaction, session_id, Some(&graph_id))?
                .ok_or_else(|| {
                    corrupt(format!(
                        "workflow activation fact for {graph_id} has no indexed start"
                    ))
                })?;
            states.insert(graph_id.clone(), state);
        }
        let state = states
            .get_mut(&graph_id)
            .ok_or_else(|| corrupt("workflow graph projection cache lost its state"))?;
        state.apply(envelope.seq, &event).map_err(|error| {
            corrupt(format!(
                "cannot reduce workflow activation at {}:{}: {error}",
                session_id, envelope.seq
            ))
        })?;
        persist_workflow_graph_state(transaction, session_id, state)?;
    }
    Ok(())
}

fn load_loom_workflow_revision_tx(
    transaction: &Connection,
    id: &str,
    template_digest: &str,
) -> StoreResult<Option<LoomWorkflow>> {
    let mut statement = transaction
        .prepare_cached(
            "SELECT record_json FROM loom_workflow_revisions
             WHERE id = ?1 ORDER BY rev DESC",
        )
        .map_err(map_sqlite_error)?;
    let rows = statement
        .query_map([id], |row| row.get::<_, String>(0))
        .map_err(map_sqlite_error)?;
    for row in rows {
        let json = row.map_err(map_sqlite_error)?;
        let workflow: LoomWorkflow = serde_json::from_str(&json)
            .map_err(|_| corrupt("loom workflow revision is not decodable"))?;
        if workflow.id == id && graph_template_digest(&workflow.template) == template_digest {
            return Ok(Some(workflow));
        }
    }
    Ok(None)
}

fn cas_evidence(
    cas: &FileCas,
    evidence_type: &str,
    value: &(impl serde::Serialize + ?Sized),
    parents: Vec<ArtifactRef>,
) -> StoreResult<InstructEvidenceRef> {
    let bytes = serde_json::to_vec(value).map_err(|error| {
        store_error(
            ErrorCode::InvalidArgument,
            format!("cannot encode InstructPipe evidence: {error}"),
            false,
        )
    })?;
    let byte_len = u64::try_from(bytes.len()).map_err(|_| {
        store_error(
            ErrorCode::InvalidArgument,
            "InstructPipe evidence length exceeds u64",
            false,
        )
    })?;
    let artifact = cas.put(&bytes)?;
    let evidence = InstructEvidenceRef::new(artifact, evidence_type, byte_len, parents);
    evidence.validate().map_err(|error| {
        store_error(
            ErrorCode::InvalidArgument,
            format!("cannot create InstructPipe evidence: {error}"),
            false,
        )
    })?;
    Ok(evidence)
}

fn append_planned_workflow_event(
    states: &mut HashMap<GraphId, WorkflowGraphState>,
    events: &mut Vec<(WorkflowGraphJournalEvent, EventId)>,
    virtual_cursor: u64,
    event: WorkflowGraphJournalEvent,
    causation_id: EventId,
) -> StoreResult<()> {
    match &event {
        WorkflowGraphJournalEvent::WorkflowGraphStarted(started) => {
            let state = WorkflowGraphState::from_started(virtual_cursor, started.as_ref().clone())
                .map_err(|error| corrupt(format!("cannot plan workflow graph start: {error}")))?;
            states.insert(started.graph_id.clone(), state);
        }
        _ => {
            let graph_id = workflow_event_graph_id(&event).clone();
            let state = states.get_mut(&graph_id).ok_or_else(|| {
                corrupt(format!(
                    "cannot plan workflow event without graph {graph_id}"
                ))
            })?;
            state.apply(virtual_cursor, &event).map_err(|error| {
                corrupt(format!("cannot plan workflow activation fact: {error}"))
            })?;
        }
    }
    events.push((event, causation_id));
    Ok(())
}

fn workflow_output_evidence(
    cas: &FileCas,
    state: &WorkflowGraphState,
    node: &GraphNodeName,
    source: &impl serde::Serialize,
) -> StoreResult<InstructEvidenceRef> {
    let spec = state
        .ast
        .nodes
        .iter()
        .find(|candidate| candidate.node.eq(node))
        .ok_or_else(|| corrupt("workflow output node is absent from its AST"))?;
    let parents = state
        .node(node)
        .ok_or_else(|| corrupt("workflow output node has no projected state"))?
        .inputs
        .iter()
        .map(|input| input.evidence.artifact.clone())
        .collect();
    cas_evidence(cas, &spec.output_type, source, parents)
}

fn forward_workflow_inputs(
    state: &WorkflowGraphState,
    node: &GraphNodeName,
    graph_seed: Option<&InstructEvidenceRef>,
) -> Option<Vec<WorkflowNodeInput>> {
    let spec = state.ast.nodes.iter().find(|spec| spec.node.eq(node))?;
    let mut inputs = Vec::with_capacity(spec.join.initial_all.len());
    for edge_id in &spec.join.initial_all {
        let edge = state.ast.edges.iter().find(|edge| edge.id == *edge_id)?;
        let evidence = match edge.kind {
            WorkflowEdgeKind::GraphInput => graph_seed.or(state.seed.as_ref())?.clone(),
            WorkflowEdgeKind::Forward => {
                let source = edge.from.as_ref()?;
                state
                    .node(source)?
                    .outputs
                    .iter()
                    .find(|output| output.evidence_type == edge.evidence_type)?
                    .clone()
            }
            WorkflowEdgeKind::Back => return None,
        };
        inputs.push(WorkflowNodeInput {
            edge_id: *edge_id,
            evidence,
        });
    }
    Some(inputs)
}

fn back_workflow_input(
    state: &WorkflowGraphState,
    node: &GraphNodeName,
) -> Option<WorkflowNodeInput> {
    let spec = state.ast.nodes.iter().find(|spec| spec.node.eq(node))?;
    for edge_id in &spec.join.reactivate_any {
        let edge = state.ast.edges.iter().find(|edge| edge.id == *edge_id)?;
        let source = edge.from.as_ref()?;
        let evidence = state.node(source)?.rejection.as_ref()?.evidence.clone()?;
        if evidence.evidence_type == edge.evidence_type {
            return Some(WorkflowNodeInput {
                edge_id: *edge_id,
                evidence,
            });
        }
    }
    None
}

fn active_back_source(
    state: &WorkflowGraphState,
    target: &GraphNodeName,
) -> Option<(GraphNodeName, u32, String)> {
    state
        .ast
        .edges
        .iter()
        .filter(|edge| edge.kind == WorkflowEdgeKind::Back && edge.to.eq(target))
        .find_map(|edge| {
            let source = edge.from.as_ref()?;
            let node = state.node(source)?;
            (node.phase == WorkflowNodePhase::Activated)
                .then(|| (source.clone(), node.iteration, edge.evidence_type.clone()))
        })
}

struct WorkflowRejectionInput<'a, S: ?Sized> {
    state: &'a WorkflowGraphState,
    node: &'a GraphNodeName,
    iteration: u32,
    code: WorkflowNodeRejectCode,
    message: String,
    evidence_type: Option<&'a str>,
    source: &'a S,
}

fn workflow_rejection<S: serde::Serialize + ?Sized>(
    cas: &FileCas,
    input: WorkflowRejectionInput<'_, S>,
) -> StoreResult<WorkflowNodeRejected> {
    let WorkflowRejectionInput {
        state,
        node,
        iteration,
        code,
        message,
        evidence_type,
        source,
    } = input;
    let spec = state
        .ast
        .nodes
        .iter()
        .find(|candidate| candidate.node.eq(node))
        .ok_or_else(|| corrupt("workflow rejection node is absent from its AST"))?;
    let parents = state.node(node).map_or_else(Vec::new, |state| {
        state
            .inputs
            .iter()
            .map(|input| input.evidence.artifact.clone())
            .collect()
    });
    let evidence = evidence_type
        .map(|evidence_type| cas_evidence(cas, evidence_type, source, parents))
        .transpose()?;
    Ok(WorkflowNodeRejected {
        graph_id: state.graph_id.clone(),
        node: node.clone(),
        iteration,
        code,
        message,
        evidence,
        convergence_gate: spec.convergence_gate,
    })
}

fn planned_workflow_cursor(latest: u64, base_len: u64, planned_len: usize) -> StoreResult<u64> {
    let runtime_offset = u64::try_from(planned_len)
        .map_err(|_| corrupt("workflow activation batch is too large"))?
        .checked_add(1)
        .ok_or_else(|| corrupt("workflow graph cursor space is exhausted"))?;
    latest
        .checked_add(base_len)
        .and_then(|cursor| cursor.checked_add(runtime_offset))
        .ok_or_else(|| corrupt("workflow graph cursor space is exhausted"))
}

fn augment_workflow_graph_envelopes(
    transaction: &Connection,
    cas: &FileCas,
    session_id: &SessionId,
    envelopes: &mut Vec<RawEnvelope>,
) -> StoreResult<()> {
    if envelopes.is_empty() {
        return Ok(());
    }
    let latest = latest_seq_in_connection(transaction, session_id)?;
    let base_len = u64::try_from(envelopes.len())
        .map_err(|_| corrupt("workflow graph base batch is too large"))?;
    let mut states = HashMap::<GraphId, WorkflowGraphState>::new();
    let mut graph_order = Vec::<GraphId>::new();
    let mut todo_inputs = HashMap::<GraphId, TodoGraphAttached>::new();
    // Graph ids are opaque. Exact evidence lookup is hash-based; the only
    // ordered selection below compares the stable numeric attempt.
    let mut evidence = HashMap::<(GraphId, GraphNodeName, u32), EvidenceRecorded>::new();
    let mut planned = Vec::<(WorkflowGraphJournalEvent, EventId)>::new();
    let base_payloads = envelopes
        .iter()
        .filter_map(|envelope| {
            serde_json::from_value::<EventPayload>(envelope.payload.clone())
                .ok()
                .map(|payload| (envelope.event_id.clone(), payload))
        })
        .collect::<Vec<_>>();
    for (causation_id, payload) in &base_payloads {
        match payload {
            EventPayload::TodoGraphAttached(attached) => {
                todo_inputs.insert(attached.child_graph_id.clone(), attached.clone());
            }
            EventPayload::GraphPinned(pinned) => {
                let Some(workflow) =
                    load_loom_workflow_revision_tx(transaction, &pinned.template, &pinned.digest)?
                else {
                    continue;
                };
                let ast = workflow_activation_ast_from_loom(&workflow).map_err(|error| {
                    store_error(
                        ErrorCode::StoreCorrupt,
                        format!("cannot materialize workflow activation AST: {error}"),
                        false,
                    )
                })?;
                let seed = todo_inputs
                    .remove(&pinned.graph_id)
                    .map(|attached| {
                        cas_evidence(
                            cas,
                            &ast.input_type,
                            &EventPayload::TodoGraphAttached(attached),
                            Vec::new(),
                        )
                    })
                    .transpose()?;
                let started = WorkflowGraphStarted {
                    graph_id: pinned.graph_id.clone(),
                    ast_digest: workflow_activation_ast_digest(&ast),
                    ast,
                    seed,
                };
                let cursor = planned_workflow_cursor(latest, base_len, planned.len())?;
                append_planned_workflow_event(
                    &mut states,
                    &mut planned,
                    cursor,
                    WorkflowGraphJournalEvent::WorkflowGraphStarted(Box::new(started)),
                    causation_id.clone(),
                )?;
                graph_order.push(pinned.graph_id.clone());
            }
            EventPayload::UserMessage { .. } => {
                if !graph_order.iter().rev().any(|graph_id| {
                    states.get(graph_id).is_some_and(|state| {
                        state.phase == WorkflowGraphPhase::Active && state.seed.is_none()
                    })
                }) && let Some(state) =
                    load_seedless_active_workflow_graph_state(transaction, session_id)?
                    && state.phase == WorkflowGraphPhase::Active
                    && state.seed.is_none()
                    && !states.contains_key(&state.graph_id)
                {
                    graph_order.push(state.graph_id.clone());
                    states.insert(state.graph_id.clone(), state);
                }
                let Some(graph_id) = graph_order
                    .iter()
                    .rev()
                    .find(|graph_id| {
                        states.get(*graph_id).is_some_and(|state| {
                            state.phase == WorkflowGraphPhase::Active && state.seed.is_none()
                        })
                    })
                    .cloned()
                else {
                    continue;
                };
                let state = states
                    .get(&graph_id)
                    .ok_or_else(|| corrupt("workflow input graph vanished during planning"))?;
                let Some(node) = state
                    .ast
                    .edges
                    .iter()
                    .find(|edge| edge.kind == WorkflowEdgeKind::GraphInput)
                    .map(|edge| edge.to.clone())
                else {
                    return Err(corrupt("workflow activation AST has no graph input edge"));
                };
                let seed = cas_evidence(cas, &state.ast.input_type, payload, Vec::new())?;
                let Some(inputs) = forward_workflow_inputs(state, &node, Some(&seed)) else {
                    continue;
                };
                let mut activated = WorkflowNodeActivated {
                    graph_id: graph_id.clone(),
                    node,
                    iteration: 1,
                    activation_order: state.next_activation_order,
                    cause: WorkflowActivationCause::ForwardJoin,
                    inputs,
                    input_ledger_digest: String::new(),
                };
                activated.input_ledger_digest = workflow_input_ledger_digest(&activated.inputs);
                let cursor = planned_workflow_cursor(latest, base_len, planned.len())?;
                append_planned_workflow_event(
                    &mut states,
                    &mut planned,
                    cursor,
                    WorkflowGraphJournalEvent::WorkflowNodeActivated(activated),
                    causation_id.clone(),
                )?;
            }
            EventPayload::EvidenceRecorded(recorded)
                if states.contains_key(&recorded.graph_id)
                    || load_workflow_graph_state(
                        transaction,
                        session_id,
                        Some(&recorded.graph_id),
                    )?
                    .is_some() =>
            {
                evidence.insert(
                    (
                        recorded.graph_id.clone(),
                        recorded.node.clone(),
                        recorded.attempt,
                    ),
                    recorded.clone(),
                );
            }
            EventPayload::GraphGateSatisfied(satisfied) => {
                if !states.contains_key(&satisfied.graph_id)
                    && let Some(state) = load_workflow_graph_state(
                        transaction,
                        session_id,
                        Some(&satisfied.graph_id),
                    )?
                {
                    states.insert(satisfied.graph_id.clone(), state);
                }
                let Some(state) = states.get(&satisfied.graph_id) else {
                    continue;
                };
                let node_state = state.node(&satisfied.node).ok_or_else(|| {
                    corrupt("satisfied workflow node has no activation projection")
                })?;
                if node_state.phase != WorkflowNodePhase::Activated {
                    return Err(corrupt(
                        "Convergence Graph satisfied a workflow node that never activated",
                    ));
                }
                let source = evidence
                    .get(&(
                        satisfied.graph_id.clone(),
                        satisfied.node.clone(),
                        satisfied.attempt,
                    ))
                    .map_or_else(|| serde_json::to_value(satisfied), serde_json::to_value)
                    .map_err(|error| {
                        store_error(
                            ErrorCode::InvalidArgument,
                            format!("cannot encode workflow completion source: {error}"),
                            false,
                        )
                    })?;
                let output = workflow_output_evidence(cas, state, &satisfied.node, &source)?;
                let convergence = state
                    .ast
                    .nodes
                    .iter()
                    .find(|spec| spec.node == satisfied.node)
                    .is_some_and(|spec| spec.convergence_gate)
                    .then(|| haider_protocol::graph::WorkflowConvergenceStamp {
                        decision_digest: output.ledger_digest.clone(),
                    });
                let completed = WorkflowNodeCompleted {
                    graph_id: satisfied.graph_id.clone(),
                    node: satisfied.node.clone(),
                    iteration: node_state.iteration,
                    outputs: vec![output],
                    output_ledger_digest: String::new(),
                    convergence,
                };
                let mut completed = completed;
                completed.output_ledger_digest =
                    workflow_evidence_ledger_digest(&completed.outputs);
                let cursor = planned_workflow_cursor(latest, base_len, planned.len())?;
                append_planned_workflow_event(
                    &mut states,
                    &mut planned,
                    cursor,
                    WorkflowGraphJournalEvent::WorkflowNodeCompleted(completed),
                    causation_id.clone(),
                )?;
            }
            EventPayload::GraphAttemptOpened(opened) => {
                if !states.contains_key(&opened.graph_id)
                    && let Some(state) =
                        load_workflow_graph_state(transaction, session_id, Some(&opened.graph_id))?
                {
                    states.insert(opened.graph_id.clone(), state);
                }
                let Some(state) = states.get(&opened.graph_id) else {
                    continue;
                };
                if let Some((source, iteration, target_type)) =
                    active_back_source(state, &opened.node)
                {
                    let source_value = evidence
                        .iter()
                        .filter(|((graph_id, node, _), _)| {
                            graph_id == &opened.graph_id && node == &source
                        })
                        .max_by_key(|((_, _, attempt), _)| *attempt)
                        .map(|(_, recorded)| recorded)
                        .map_or_else(|| serde_json::to_value(opened), serde_json::to_value)
                        .map_err(|error| {
                            store_error(
                                ErrorCode::InvalidArgument,
                                format!("cannot encode workflow reject source: {error}"),
                                false,
                            )
                        })?;
                    let rejected = workflow_rejection(
                        cas,
                        WorkflowRejectionInput {
                            state,
                            node: &source,
                            iteration,
                            code: WorkflowNodeRejectCode::EvidenceRejected,
                            message: format!(
                                "node {source} rejected evidence and followed its back edge"
                            ),
                            evidence_type: Some(&target_type),
                            source: &source_value,
                        },
                    )?;
                    let cursor = planned_workflow_cursor(latest, base_len, planned.len())?;
                    append_planned_workflow_event(
                        &mut states,
                        &mut planned,
                        cursor,
                        WorkflowGraphJournalEvent::WorkflowNodeRejected(rejected),
                        causation_id.clone(),
                    )?;
                }
                let state = states
                    .get(&opened.graph_id)
                    .ok_or_else(|| corrupt("workflow graph state vanished during activation"))?;
                // A retry target can still have its original forward inputs
                // in the projection. The explicit rejected back edge has
                // precedence; choosing the stale forward join would attempt
                // to activate a completed/rejected generation as iteration 1.
                let (cause, inputs) = if let Some(input) = back_workflow_input(state, &opened.node)
                {
                    (WorkflowActivationCause::BackEdge, vec![input])
                } else if let Some(inputs) = forward_workflow_inputs(state, &opened.node, None) {
                    (WorkflowActivationCause::ForwardJoin, inputs)
                } else {
                    let node_state = state
                        .node(&opened.node)
                        .ok_or_else(|| corrupt("opened workflow node has no projected state"))?;
                    let awaits_external_input = state.seed.is_none()
                        && state
                            .ast
                            .nodes
                            .iter()
                            .find(|spec| spec.node == opened.node)
                            .is_some_and(|spec| {
                                spec.join.initial_all.iter().any(|edge_id| {
                                    state.ast.edges.iter().any(|edge| {
                                        edge.id == *edge_id
                                            && edge.kind == WorkflowEdgeKind::GraphInput
                                    })
                                })
                            });
                    if awaits_external_input {
                        continue;
                    }
                    let missing_iteration =
                        node_state.iteration.checked_add(1).ok_or_else(|| {
                            corrupt("workflow activation iteration space is exhausted")
                        })?;
                    let rejected = workflow_rejection(
                        cas,
                        WorkflowRejectionInput {
                            state,
                            node: &opened.node,
                            iteration: missing_iteration,
                            code: WorkflowNodeRejectCode::TypedInputMissing,
                            message: format!(
                                "node {} did not activate because its typed join inputs are missing",
                                opened.node
                            ),
                            evidence_type: None,
                            source: opened,
                        },
                    )?;
                    let cursor = planned_workflow_cursor(latest, base_len, planned.len())?;
                    append_planned_workflow_event(
                        &mut states,
                        &mut planned,
                        cursor,
                        WorkflowGraphJournalEvent::WorkflowNodeRejected(rejected),
                        causation_id.clone(),
                    )?;
                    continue;
                };
                let state = states
                    .get(&opened.graph_id)
                    .ok_or_else(|| corrupt("workflow graph state vanished before activation"))?;
                let node_state = state
                    .node(&opened.node)
                    .ok_or_else(|| corrupt("opened workflow node has no projected state"))?;
                let next_iteration = node_state
                    .iteration
                    .checked_add(1)
                    .ok_or_else(|| corrupt("workflow activation iteration space is exhausted"))?;
                if cause == WorkflowActivationCause::BackEdge
                    && state.back_edge_activations >= state.ast.max_back_edge_activations
                {
                    let rejected = workflow_rejection(
                        cas,
                        WorkflowRejectionInput {
                            state,
                            node: &opened.node,
                            iteration: next_iteration,
                            code: WorkflowNodeRejectCode::IterationGuard,
                            message: format!(
                                "node {} was not reactivated because the bounded back-edge guard of {} was exhausted",
                                opened.node, state.ast.max_back_edge_activations
                            ),
                            evidence_type: None,
                            source: opened,
                        },
                    )?;
                    let cursor = planned_workflow_cursor(latest, base_len, planned.len())?;
                    append_planned_workflow_event(
                        &mut states,
                        &mut planned,
                        cursor,
                        WorkflowGraphJournalEvent::WorkflowNodeRejected(rejected),
                        causation_id.clone(),
                    )?;
                    continue;
                }
                let mut activated = WorkflowNodeActivated {
                    graph_id: opened.graph_id.clone(),
                    node: opened.node.clone(),
                    iteration: next_iteration,
                    activation_order: state.next_activation_order,
                    cause,
                    inputs,
                    input_ledger_digest: String::new(),
                };
                activated.input_ledger_digest = workflow_input_ledger_digest(&activated.inputs);
                let cursor = planned_workflow_cursor(latest, base_len, planned.len())?;
                append_planned_workflow_event(
                    &mut states,
                    &mut planned,
                    cursor,
                    WorkflowGraphJournalEvent::WorkflowNodeActivated(activated),
                    causation_id.clone(),
                )?;
            }
            EventPayload::GraphBlocked(blocked) => {
                if !states.contains_key(&blocked.graph_id)
                    && let Some(state) =
                        load_workflow_graph_state(transaction, session_id, Some(&blocked.graph_id))?
                {
                    states.insert(blocked.graph_id.clone(), state);
                }
                let Some(state) = states.get(&blocked.graph_id) else {
                    continue;
                };
                let node_state = state
                    .node(&blocked.node)
                    .ok_or_else(|| corrupt("blocked workflow node has no projected state"))?;
                if node_state.phase != WorkflowNodePhase::Activated {
                    continue;
                }
                let code = if matches!(blocked.reason, GraphBlockReason::HumanHold) {
                    WorkflowNodeRejectCode::ConvergenceRejected
                } else if matches!(blocked.reason, GraphBlockReason::RoundsExhausted) {
                    WorkflowNodeRejectCode::IterationGuard
                } else {
                    WorkflowNodeRejectCode::EvidenceRejected
                };
                let rejection_evidence_type = state
                    .ast
                    .nodes
                    .iter()
                    .find(|spec| spec.node == blocked.node && spec.convergence_gate)
                    .map(|spec| spec.output_type.clone());
                let rejected = workflow_rejection(
                    cas,
                    WorkflowRejectionInput {
                        state,
                        node: &blocked.node,
                        iteration: node_state.iteration,
                        code,
                        message: format!(
                            "convergence graph rejected node {}: {:?}",
                            blocked.node, blocked.reason
                        ),
                        evidence_type: rejection_evidence_type.as_deref(),
                        source: blocked,
                    },
                )?;
                let cursor = planned_workflow_cursor(latest, base_len, planned.len())?;
                append_planned_workflow_event(
                    &mut states,
                    &mut planned,
                    cursor,
                    WorkflowGraphJournalEvent::WorkflowNodeRejected(rejected),
                    causation_id.clone(),
                )?;
            }
            EventPayload::GraphAbandoned(abandoned) => {
                plan_terminal_workflow_rejections(TerminalWorkflowRejectionInput {
                    transaction,
                    cas,
                    session_id,
                    states: &mut states,
                    planned: &mut planned,
                    causation_id,
                    latest,
                    base_len,
                    graph_id: &abandoned.graph_id,
                    code: WorkflowNodeRejectCode::Abandoned,
                    message: "workflow graph was abandoned",
                    source: abandoned,
                })?;
            }
            EventPayload::GraphSuperseded(superseded) => {
                plan_terminal_workflow_rejections(TerminalWorkflowRejectionInput {
                    transaction,
                    cas,
                    session_id,
                    states: &mut states,
                    planned: &mut planned,
                    causation_id,
                    latest,
                    base_len,
                    graph_id: &superseded.old,
                    code: WorkflowNodeRejectCode::Superseded,
                    message: "workflow graph was superseded",
                    source: superseded,
                })?;
            }
            _ => {}
        }
    }
    if planned.is_empty() {
        return Ok(());
    }
    let metadata = envelopes
        .first()
        .cloned()
        .ok_or_else(|| corrupt("workflow graph batch lost its metadata envelope"))?;
    for (index, (event, causation_id)) in planned.into_iter().enumerate() {
        let payload_json = event.to_payload_value().map_err(|error| {
            store_error(
                ErrorCode::InvalidArgument,
                format!("cannot encode workflow activation event: {error}"),
                false,
            )
        })?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"haider/workflow-activation-event/v1\0");
        hasher.update(session_id.as_str().as_bytes());
        let event_offset =
            u64::try_from(index).map_err(|_| corrupt("workflow activation batch is too large"))?;
        hasher.update(&event_offset.to_be_bytes());
        hasher.update(payload_json.to_string().as_bytes());
        envelopes.push(EventEnvelope {
            schema_version: SCHEMA_VERSION,
            event_id: EventId::new(format!(
                "workflow-activation-{}",
                hasher.finalize().to_hex()
            )),
            seq: 0,
            session_id: session_id.clone(),
            branch_id: None,
            run_id: metadata.run_id.clone(),
            agent_id: metadata.agent_id.clone(),
            device_id: metadata.device_id.clone(),
            authority_epoch: metadata.authority_epoch,
            worker_generation: metadata.worker_generation,
            causation_id: Some(causation_id),
            correlation_id: metadata.correlation_id.clone(),
            committed_at_ms: 0,
            render: RenderTargets {
                // L3 consumes the indexed state/watch surfaces. Keeping raw
                // runtime facts off the legacy live-view stream avoids
                // teaching old EventPayload renderers about this additive
                // journal family.
                ui: false,
                durable: true,
                prompt: PromptRender::Omit,
            },
            payload: payload_json,
        });
    }
    Ok(())
}

/// One-time v25 translation for pinned Loom graphs whose legacy journal was
/// committed before activation facts existed. The translation appends the
/// same typed facts used by live commands, then builds graph/node indexes in
/// that transaction. Ordinary reads never rescan the journal.
fn backfill_workflow_graph_journals(
    connection: &mut Connection,
    cas: &FileCas,
    worker_generation: u64,
) -> StoreResult<()> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sqlite_error)?;
    let version = transaction
        .query_row(
            "SELECT workflow_graph_backfill_version FROM profile_meta WHERE singleton = 1",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map_err(map_sqlite_error)?;
    let version = u32::try_from(version)
        .map_err(|_| corrupt("workflow graph backfill version is invalid"))?;
    if version >= 1 {
        transaction.commit().map_err(map_sqlite_error)?;
        return Ok(());
    }
    let session_ids = {
        let mut statement = transaction
            .prepare_cached(
                "SELECT DISTINCT session_id FROM events
                 WHERE payload_kind = 'graph_pinned' ORDER BY session_id ASC",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(map_sqlite_error)?;
        let mut session_ids = Vec::new();
        for row in rows {
            session_ids.push(SessionId::new(row.map_err(map_sqlite_error)?));
        }
        session_ids
    };
    let committed_at_ms = now_ms()?;
    for session_id in session_ids {
        let mut historical = load_workflow_backfill_envelopes(&transaction, &session_id)?;
        let historical_len = historical.len();
        augment_workflow_graph_envelopes(&transaction, cas, &session_id, &mut historical)?;
        if historical.len() == historical_len {
            continue;
        }
        let mut generated = historical.split_off(historical_len);
        for envelope in &mut generated {
            envelope.worker_generation = worker_generation;
            envelope.device_id = DeviceId::new("workflow-graph-backfill");
            envelope.render.ui = false;
        }
        append_transaction_envelopes(&transaction, &session_id, committed_at_ms, &mut generated)?;
    }
    let changed = transaction
        .execute(
            "UPDATE profile_meta SET workflow_graph_backfill_version = 1
             WHERE singleton = 1 AND workflow_graph_backfill_version < 1",
            [],
        )
        .map_err(map_sqlite_error)?;
    if changed != 1 {
        return Err(corrupt(
            "workflow graph backfill version did not advance exactly once",
        ));
    }
    transaction.commit().map_err(map_sqlite_error)
}

struct TerminalWorkflowRejectionInput<'a, S: ?Sized> {
    transaction: &'a Connection,
    cas: &'a FileCas,
    session_id: &'a SessionId,
    states: &'a mut HashMap<GraphId, WorkflowGraphState>,
    planned: &'a mut Vec<(WorkflowGraphJournalEvent, EventId)>,
    causation_id: &'a EventId,
    latest: u64,
    base_len: u64,
    graph_id: &'a GraphId,
    code: WorkflowNodeRejectCode,
    message: &'a str,
    source: &'a S,
}

fn plan_terminal_workflow_rejections<S: serde::Serialize + ?Sized>(
    input: TerminalWorkflowRejectionInput<'_, S>,
) -> StoreResult<()> {
    let TerminalWorkflowRejectionInput {
        transaction,
        cas,
        session_id,
        states,
        planned,
        causation_id,
        latest,
        base_len,
        graph_id,
        code,
        message,
        source,
    } = input;
    if !states.contains_key(graph_id)
        && let Some(state) = load_workflow_graph_state(transaction, session_id, Some(graph_id))?
    {
        states.insert(graph_id.clone(), state);
    }
    let Some(state) = states.get(graph_id) else {
        return Ok(());
    };
    let active = state
        .nodes
        .iter()
        .filter_map(|node| match node.phase {
            WorkflowNodePhase::Waiting => node
                .iteration
                .checked_add(1)
                .map(|iteration| (node.node.clone(), iteration)),
            WorkflowNodePhase::Activated => Some((node.node.clone(), node.iteration)),
            WorkflowNodePhase::Completed | WorkflowNodePhase::Rejected => None,
        })
        .collect::<Vec<_>>();
    for (node, iteration) in active {
        let state = states
            .get(graph_id)
            .ok_or_else(|| corrupt("workflow graph vanished during terminal rejection"))?;
        let rejected = workflow_rejection(
            cas,
            WorkflowRejectionInput {
                state,
                node: &node,
                iteration,
                code,
                message: message.to_owned(),
                evidence_type: None,
                source,
            },
        )?;
        let cursor = planned_workflow_cursor(latest, base_len, planned.len())?;
        append_planned_workflow_event(
            states,
            planned,
            cursor,
            WorkflowGraphJournalEvent::WorkflowNodeRejected(rejected),
            causation_id.clone(),
        )?;
    }
    Ok(())
}

fn append_transaction_envelopes(
    transaction: &Transaction<'_>,
    session_id: &SessionId,
    committed_at_ms: u64,
    envelopes: &mut [RawEnvelope],
) -> StoreResult<()> {
    ensure_run_head_projection(transaction, session_id)?;
    let latest: i64 = transaction
        .query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM events WHERE session_id = ?1",
            [session_id.as_str()],
            |row| row.get(0),
        )
        .map_err(map_sqlite_error)?;
    let first = u64::try_from(latest)
        .map_err(|_| corrupt("database contains a negative event sequence"))?
        .checked_add(1)
        .ok_or_else(|| corrupt("event sequence space is exhausted"))?;
    let mut insert = transaction
        .prepare_cached(
            "INSERT INTO events(
                session_id, seq, envelope_json, event_id, committed_at_ms, payload_kind
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .map_err(map_sqlite_error)?;
    for (offset, envelope) in envelopes.iter_mut().enumerate() {
        let offset = u64::try_from(offset).map_err(|_| corrupt("event batch is too large"))?;
        envelope.seq = first
            .checked_add(offset)
            .ok_or_else(|| corrupt("event sequence space is exhausted"))?;
        envelope.committed_at_ms = committed_at_ms;
        stamp_queue_delta(envelope)?;
        stamp_workspace_mutation(transaction, envelope)?;
        let bytes = encode_envelope(envelope).map_err(|error| {
            store_error(
                ErrorCode::InvalidArgument,
                format!("cannot serialize command envelope: {error}"),
                false,
            )
        })?;
        insert
            .execute(params![
                session_id.as_str(),
                to_sqlite_integer(envelope.seq)?,
                bytes,
                envelope.event_id.as_str(),
                to_sqlite_integer(committed_at_ms)?,
                payload_kind(envelope),
            ])
            .map_err(map_sqlite_error)?;
        enqueue_hook_dispatch(transaction, envelope)?;
    }
    drop(insert);
    update_run_head_projection_after_append(transaction, session_id, envelopes)?;
    update_workflow_graph_projection_after_append(transaction, session_id, envelopes)?;
    if envelopes
        .iter()
        .any(|envelope| graph_telemetry_event(&envelope.payload))
    {
        transaction
            .execute(
                "INSERT OR IGNORE INTO graph_telemetry_dirty(session_id) VALUES (?1)",
                [session_id.as_str()],
            )
            .map_err(map_sqlite_error)?;
    }
    update_branch_heads(transaction, envelopes)?;
    Ok(())
}

/// Claims (or re-encounters) the pending receipt row for one semantic
/// command inside the caller's open transaction — the shared first step of
/// every R2 command transaction. `INSERT OR IGNORE`: a fresh command claims
/// the row; an existing same-command pending row is a recovery artifact the
/// caller finishes (a committed row was already returned by the caller's
/// in-transaction receipt lookup; a different method/body was rejected by
/// that lookup).
fn claim_pending_receipt(
    transaction: &Transaction<'_>,
    command_id: &str,
    method: &str,
    request_digest: &str,
    request_json: &str,
    created_at_ms: u64,
) -> StoreResult<()> {
    if resolution_by_command(transaction, command_id)?.is_some() {
        return Err(store_error(
            ErrorCode::InvalidArgument,
            "command id was already used by a menu answer",
            false,
        ));
    }
    transaction
        .execute(
            "INSERT OR IGNORE INTO command_receipts(
                command_id, method, request_digest, request_json, state,
                session_id, run_id, accepted_seq, response_json,
                created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, 'pending',
                       NULL, NULL, NULL, NULL, ?5, ?5)",
            params![
                command_id,
                method,
                request_digest,
                request_json,
                to_sqlite_integer(created_at_ms)?,
            ],
        )
        .map_err(map_sqlite_error)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn finalize_command_receipt<T: serde::Serialize>(
    transaction: &Transaction<'_>,
    command_id: &str,
    session_id: &str,
    run_id: Option<&str>,
    accepted_seq: Option<u64>,
    response: &T,
    updated_at_ms: u64,
    description: &str,
) -> StoreResult<()> {
    let response_json = serde_json::to_string(response).map_err(|error| {
        store_error(
            ErrorCode::InvalidArgument,
            format!("cannot serialize {description} response: {error}"),
            false,
        )
    })?;
    let accepted_seq = accepted_seq.map(to_sqlite_integer).transpose()?;
    let updated = transaction
        .execute(
            "UPDATE command_receipts
             SET state = 'committed', session_id = ?2, run_id = ?3,
                 accepted_seq = ?4, response_json = ?5, updated_at_ms = ?6
             WHERE command_id = ?1 AND state = 'pending'",
            params![
                command_id,
                session_id,
                run_id,
                accepted_seq,
                response_json,
                to_sqlite_integer(updated_at_ms)?,
            ],
        )
        .map_err(map_sqlite_error)?;
    if updated == 1 {
        Ok(())
    } else {
        Err(corrupt(format!(
            "{description} command receipt was not pending at finalization"
        )))
    }
}

#[allow(clippy::too_many_arguments)]
fn finalize_management_command_receipt<T: serde::Serialize>(
    transaction: &Transaction<'_>,
    command_id: &str,
    method: &str,
    session_id: &str,
    run_id: Option<&str>,
    accepted_seq: Option<u64>,
    response: &T,
    updated_at_ms: u64,
    description: &str,
    advance_revision: bool,
) -> StoreResult<u64> {
    let (stored_method, state, final_revision): (String, String, Option<i64>) = transaction
        .query_row(
            "SELECT method, state, final_revision
             FROM command_receipts
             WHERE command_id = ?1",
            [command_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(map_sqlite_error)?;
    if stored_method != method || state != "pending" || final_revision.is_some() {
        return Err(corrupt(format!(
            "{description} command receipt was not an unrevisioned pending `{method}` receipt"
        )));
    }
    finalize_command_receipt(
        transaction,
        command_id,
        session_id,
        run_id,
        accepted_seq,
        response,
        updated_at_ms,
        description,
    )?;
    let revision = if advance_revision {
        next_management_revision_in_transaction(transaction)?
    } else {
        let current: i64 = transaction
            .query_row(
                "SELECT management_revision FROM profile_meta WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .map_err(map_sqlite_error)?;
        u64::try_from(current)
            .map_err(|_| corrupt("database contains a negative management revision"))?
    };
    let updated = transaction
        .execute(
            "UPDATE command_receipts
             SET final_revision = ?2
             WHERE command_id = ?1 AND method = ?3
               AND state = 'committed' AND final_revision IS NULL",
            params![command_id, to_sqlite_integer(revision)?, method],
        )
        .map_err(map_sqlite_error)?;
    if updated != 1 {
        return Err(corrupt(format!(
            "{description} receipt did not accept its final management revision"
        )));
    }
    Ok(revision)
}

fn next_management_revision_in_transaction(transaction: &Transaction<'_>) -> StoreResult<u64> {
    let current: i64 = transaction
        .query_row(
            "SELECT management_revision FROM profile_meta WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .map_err(map_sqlite_error)?;
    let next = current
        .checked_add(1)
        .ok_or_else(|| corrupt("management revision space is exhausted"))?;
    let updated = transaction
        .execute(
            "UPDATE profile_meta
             SET management_revision = ?1
             WHERE singleton = 1 AND management_revision = ?2",
            params![next, current],
        )
        .map_err(map_sqlite_error)?;
    if updated != 1 {
        return Err(corrupt(
            "profile metadata is missing its management revision singleton",
        ));
    }
    u64::try_from(next).map_err(|_| corrupt("database contains a negative management revision"))
}

/// The `failed` twin of [`finalize_command_receipt`] (additive; W3c2's
/// login command is the first writer of the schema's `failed` state): a
/// definitive non-retryable outcome recorded terminally, with the same
/// pending-only guard.
fn fail_command_receipt<T: serde::Serialize>(
    transaction: &Transaction<'_>,
    command_id: &str,
    failure: &T,
    updated_at_ms: u64,
    description: &str,
) -> StoreResult<()> {
    let response_json = serde_json::to_string(failure).map_err(|error| {
        store_error(
            ErrorCode::InvalidArgument,
            format!("cannot serialize {description} failure: {error}"),
            false,
        )
    })?;
    let updated = transaction
        .execute(
            "UPDATE command_receipts
             SET state = 'failed', response_json = ?2, updated_at_ms = ?3
             WHERE command_id = ?1 AND state = 'pending'",
            params![command_id, response_json, to_sqlite_integer(updated_at_ms)?],
        )
        .map_err(map_sqlite_error)?;
    if updated == 1 {
        Ok(())
    } else {
        Err(corrupt(format!(
            "{description} command receipt was not pending at failure record"
        )))
    }
}

fn stale_generation(provided: u64, current: u64) -> HaiderError {
    store_error(
        ErrorCode::SingleWriterViolation,
        format!("stale worker generation {provided}; current generation is {current}"),
        false,
    )
}

fn decode_session_metadata(
    session_id: &SessionId,
    json: &str,
) -> StoreResult<Option<SessionMetadataV1>> {
    let value: serde_json::Value = serde_json::from_str(json).map_err(|error| {
        corrupt(format!(
            "session {session_id} metadata JSON is invalid: {error}"
        ))
    })?;
    if value.as_object().is_some_and(serde_json::Map::is_empty) {
        return Ok(None);
    }
    serde_json::from_value(value).map(Some).map_err(|error| {
        corrupt(format!(
            "session {session_id} metadata does not match SessionMetadataV1: {error}"
        ))
    })
}

#[derive(Debug)]
struct ResolutionRow {
    session_id: String,
    menu_id: String,
    request_seq: u64,
    worker_generation: u64,
    answer_json: String,
    input_is_secret_reference: bool,
    resolution_seq: u64,
}

fn resolve_menu_transaction(
    transaction: &Transaction<'_>,
    cas: &FileCas,
    command: &MenuResolutionCommand,
    current_worker_generation: u64,
) -> StoreResult<MenuResolutionOutcome> {
    let answer_json = serde_json::to_string(&command.answer).map_err(|error| {
        store_error(
            ErrorCode::InvalidArgument,
            format!("cannot serialize menu answer: {error}"),
            false,
        )
    })?;
    if let Some(existing) = resolution_by_command(transaction, &command.command_id)? {
        let same_command = existing.session_id == command.session_id.as_str()
            && existing.menu_id == command.answer.menu.as_str()
            && existing.request_seq == command.request_seq
            && existing.worker_generation == command.worker_generation
            && existing.answer_json == answer_json
            && existing.input_is_secret_reference == command.input_is_secret_reference;
        return if same_command {
            Ok(MenuResolutionOutcome::IdempotentReplay {
                resolution_seq: existing.resolution_seq,
            })
        } else {
            Err(store_error(
                ErrorCode::InvalidArgument,
                "menu command id was already used with different coordinates or answer",
                false,
            ))
        };
    }
    let receipt_exists = transaction
        .query_row(
            "SELECT 1 FROM command_receipts WHERE command_id = ?1",
            [&command.command_id],
            |_| Ok(()),
        )
        .optional()
        .map_err(map_sqlite_error)?
        .is_some();
    if receipt_exists {
        return Err(store_error(
            ErrorCode::InvalidArgument,
            "command id was already used by another durable command",
            false,
        ));
    }
    let opening = load_envelope(transaction, &command.session_id, command.request_seq)?
        .ok_or_else(|| {
            store_error(
                ErrorCode::MenuNotFound,
                format!(
                    "menu request event {} does not exist in session {}",
                    command.request_seq, command.session_id
                ),
                false,
            )
        })?;
    let menu = opened_menu(&opening, &command.answer.menu)?;
    let graph_menu = matches!(
        menu.kind,
        MenuKind::GraphHumanConfirm { .. } | MenuKind::GraphAbandonConfirm { .. }
    );
    if command.worker_generation != current_worker_generation
        && !command.allow_prior_generation
        && !graph_menu
    {
        return Err(stale_generation(
            command.worker_generation,
            current_worker_generation,
        ));
    }
    if opening.worker_generation != command.worker_generation {
        return Err(store_error(
            ErrorCode::SingleWriterViolation,
            format!(
                "menu {} belongs to worker generation {}, not {}",
                command.answer.menu, opening.worker_generation, command.worker_generation
            ),
            false,
        ));
    }
    validate_answer(&menu, &command.answer, command.input_is_secret_reference)?;
    if let Some(resolution_seq) =
        resolution_by_menu(transaction, &command.session_id, &command.answer.menu)?
    {
        return Ok(MenuResolutionOutcome::AlreadyResolved { resolution_seq });
    }
    if let Some(resolution_seq) = historical_resolution(transaction, command)? {
        return Ok(MenuResolutionOutcome::AlreadyResolved { resolution_seq });
    }

    let committed_at_ms = now_ms()?;
    let event_id = menu_resolution_event_id(command);
    let payload = serde_json::to_value(EventPayload::MenuAnswered(command.answer.clone()))
        .map_err(|error| {
            store_error(
                ErrorCode::InvalidArgument,
                format!("cannot serialize menu resolution payload: {error}"),
                false,
            )
        })?;
    let envelope = EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: event_id.clone(),
        seq: 0,
        session_id: command.session_id.clone(),
        branch_id: opening.branch_id.clone(),
        run_id: opening.run_id.clone(),
        agent_id: opening.agent_id.clone(),
        device_id: command.device_id.clone(),
        authority_epoch: opening.authority_epoch,
        // The command presents the durable OPENING generation. A restart may
        // legitimately answer that still-pending checkpoint, but the newly
        // committed answer is current-generation work.
        worker_generation: current_worker_generation,
        causation_id: Some(opening.event_id.clone()),
        correlation_id: opening.correlation_id.clone(),
        committed_at_ms,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload,
    };
    let mut envelopes = vec![envelope];
    if let MenuKind::GraphHumanConfirm {
        graph_id,
        node,
        attempt,
    } = &menu.kind
    {
        let reductions = load_graph_reductions(transaction, &command.session_id)?;
        let reduction = reductions.graph(graph_id).cloned().ok_or_else(|| {
            store_error(
                ErrorCode::GraphNotActive,
                "graph confirmation has no graph instance",
                false,
            )
        })?;
        let status = reduction.status.clone().ok_or_else(|| {
            store_error(
                ErrorCode::GraphNotActive,
                "graph confirmation has no graph instance",
                false,
            )
        })?;
        if status.graph_id != *graph_id
            || status.phase != GraphPhase::Active
            || !status.node_is_ready(node)
            || status
                .nodes
                .iter()
                .find(|candidate| candidate.node == *node)
                .is_none_or(|candidate| candidate.current_attempt != Some(*attempt))
            || !graph_pending_menus(&status).iter().any(|id| id == &menu.id)
            || !reduction
                .template_nodes
                .iter()
                .any(|spec| &spec.name == node && matches!(spec.gate, GraphGateKind::HumanConfirm))
        {
            return Err(store_error(
                ErrorCode::GraphWrongNode,
                "graph confirmation is stale for the current obligation",
                false,
            ));
        }
        let key =
            command.answer.option_key.as_deref().ok_or_else(|| {
                store_error(ErrorCode::InvalidArgument, "missing answer key", false)
            })?;
        let payloads = match key {
            "confirm" => {
                let mut payloads = vec![EventPayload::GraphGateSatisfied(GraphGateSatisfied {
                    graph_id: graph_id.clone(),
                    node: node.clone(),
                    attempt: *attempt,
                })];
                let graph_followups = dependency_followups(&reduction, &status, node, *attempt)?;
                let child_completed = graph_followups.iter().any(|payload| {
                    matches!(
                        payload,
                        EventPayload::GraphCompleted(completed) if completed.graph_id == *graph_id
                    )
                });
                payloads.extend(graph_followups);
                if child_completed {
                    payloads.extend(todo_child_completed_followups(&reductions, graph_id)?);
                }
                payloads
            }
            "hold" => {
                let mut payloads = vec![EventPayload::GraphBlocked(GraphBlocked {
                    graph_id: graph_id.clone(),
                    node: node.clone(),
                    reason: GraphBlockReason::HumanHold,
                })];
                payloads.extend(
                    graph_pending_menus(&status)
                        .into_iter()
                        .filter(|pending| pending != &menu.id)
                        .map(|menu| EventPayload::MenuClosed {
                            menu,
                            reason: MenuCloseReason::Dismissed,
                        }),
                );
                payloads
            }
            _ => {
                return Err(store_error(
                    ErrorCode::InvalidArgument,
                    "unsupported graph confirmation answer",
                    false,
                ));
            }
        };
        for (index, payload) in payloads.into_iter().enumerate() {
            envelopes.push(EventEnvelope {
                schema_version: SCHEMA_VERSION,
                event_id: EventId::new(format!("graph-menu-settle-{}-{}", event_id, index + 1)),
                seq: 0,
                session_id: command.session_id.clone(),
                branch_id: None,
                run_id: None,
                agent_id: None,
                device_id: command.device_id.clone(),
                authority_epoch: opening.authority_epoch,
                worker_generation: current_worker_generation,
                causation_id: Some(event_id.clone()),
                correlation_id: None,
                committed_at_ms: 0,
                render: RenderTargets {
                    ui: true,
                    durable: true,
                    prompt: PromptRender::Omit,
                },
                payload: serde_json::to_value(payload).map_err(|error| {
                    store_error(
                        ErrorCode::InvalidArgument,
                        format!("cannot serialize graph menu settlement: {error}"),
                        false,
                    )
                })?,
            });
        }
    }
    if let MenuKind::GraphAbandonConfirm {
        graph_id,
        run_id,
        state_digest,
    } = &menu.kind
    {
        let reduction = load_graph_reduction(transaction, &command.session_id)?;
        let status = reduction.status.clone().ok_or_else(|| {
            store_error(
                ErrorCode::GraphNotActive,
                "graph finalization confirmation has no graph instance",
                false,
            )
        })?;
        if status.graph_id != *graph_id
            || status.phase != GraphPhase::Active
            || opening.run_id.as_ref() != Some(run_id)
            || graph_finalization_state_digest(&status)? != *state_digest
            || !graph_pending_menus(&status).iter().any(|id| id == &menu.id)
        {
            return Err(store_error(
                ErrorCode::GraphWrongNode,
                "graph finalization confirmation is stale for the current obligation",
                false,
            ));
        }
        let key =
            command.answer.option_key.as_deref().ok_or_else(|| {
                store_error(ErrorCode::InvalidArgument, "missing answer key", false)
            })?;
        match key {
            "continue-work" => {}
            "abandon-and-finish" => {
                let payloads = std::iter::once(EventPayload::GraphAbandoned(GraphAbandoned {
                    graph_id: graph_id.clone(),
                    why: "explicit finalization override".into(),
                }))
                .chain(
                    graph_pending_menus(&status)
                        .into_iter()
                        .filter(|pending| pending != &menu.id)
                        .map(|menu| EventPayload::MenuClosed {
                            menu,
                            reason: MenuCloseReason::Dismissed,
                        }),
                );
                for (index, payload) in payloads.enumerate() {
                    envelopes.push(EventEnvelope {
                        schema_version: SCHEMA_VERSION,
                        event_id: EventId::new(format!(
                            "graph-finalization-settle-{}-{}",
                            event_id,
                            index + 1
                        )),
                        seq: 0,
                        session_id: command.session_id.clone(),
                        branch_id: opening.branch_id.clone(),
                        run_id: opening.run_id.clone(),
                        agent_id: opening.agent_id.clone(),
                        device_id: command.device_id.clone(),
                        authority_epoch: opening.authority_epoch,
                        worker_generation: current_worker_generation,
                        causation_id: Some(event_id.clone()),
                        correlation_id: opening.correlation_id.clone(),
                        committed_at_ms: 0,
                        render: RenderTargets {
                            ui: true,
                            durable: true,
                            prompt: PromptRender::Omit,
                        },
                        payload: serde_json::to_value(payload).map_err(|error| {
                            store_error(
                                ErrorCode::InvalidArgument,
                                format!("cannot serialize graph finalization settlement: {error}"),
                                false,
                            )
                        })?,
                    });
                }
            }
            _ => {
                return Err(store_error(
                    ErrorCode::InvalidArgument,
                    "unsupported graph finalization answer",
                    false,
                ));
            }
        }
    }
    augment_workflow_graph_envelopes(transaction, cas, &command.session_id, &mut envelopes)?;
    append_transaction_envelopes(
        transaction,
        &command.session_id,
        committed_at_ms,
        &mut envelopes,
    )?;
    let resolution_seq = envelopes[0].seq;
    transaction
        .execute(
            "INSERT INTO menu_resolutions(
                session_id, menu_id, request_seq, worker_generation,
                command_id, answer_json, input_is_secret_reference, resolution_seq
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                command.session_id.as_str(),
                command.answer.menu.as_str(),
                to_sqlite_integer(command.request_seq)?,
                to_sqlite_integer(command.worker_generation)?,
                &command.command_id,
                answer_json,
                command.input_is_secret_reference,
                to_sqlite_integer(resolution_seq)?,
            ],
        )
        .map_err(map_sqlite_error)?;
    Ok(MenuResolutionOutcome::Committed {
        envelope: Box::new(envelopes.remove(0)),
        follow_up: envelopes,
        menu,
    })
}

/// Adds one non-engine fact to the durable post-commit hook outbox inside the
/// event's own transaction. Hook lifecycle/result facts are deliberately not
/// recursive inputs; run trust and profile update/account facts are inputs.
fn enqueue_hook_dispatch(transaction: &Connection, envelope: &RawEnvelope) -> StoreResult<()> {
    if HookEventPayload::is_engine_fact(&envelope.payload)
        || workflow_graph_journal_event(&envelope.payload)?.is_some()
    {
        return Ok(());
    }
    transaction
        .execute(
            "INSERT INTO hook_dispatch_outbox(session_id, seq) VALUES (?1, ?2)",
            params![
                envelope.session_id.as_str(),
                to_sqlite_integer(envelope.seq)?
            ],
        )
        .map_err(map_sqlite_error)?;
    Ok(())
}

fn resolution_by_command(
    transaction: &Transaction<'_>,
    command_id: &str,
) -> StoreResult<Option<ResolutionRow>> {
    transaction
        .query_row(
            "SELECT session_id, menu_id, request_seq, worker_generation,
                    answer_json, input_is_secret_reference, resolution_seq
             FROM menu_resolutions
             WHERE command_id = ?1",
            [command_id],
            |row| {
                Ok(ResolutionRow {
                    session_id: row.get(0)?,
                    menu_id: row.get(1)?,
                    request_seq: sql_u64(row.get(2)?)?,
                    worker_generation: sql_u64(row.get(3)?)?,
                    answer_json: row.get(4)?,
                    input_is_secret_reference: row.get(5)?,
                    resolution_seq: sql_u64(row.get(6)?)?,
                })
            },
        )
        .optional()
        .map_err(map_sqlite_error)
}

fn resolution_by_menu(
    transaction: &Transaction<'_>,
    session_id: &SessionId,
    menu_id: &MenuId,
) -> StoreResult<Option<u64>> {
    transaction
        .query_row(
            "SELECT resolution_seq
             FROM menu_resolutions
             WHERE session_id = ?1 AND menu_id = ?2",
            params![session_id.as_str(), menu_id.as_str()],
            |row| sql_u64(row.get(0)?),
        )
        .optional()
        .map_err(map_sqlite_error)
}

fn load_envelope(
    connection: &Connection,
    session_id: &SessionId,
    seq: u64,
) -> StoreResult<Option<RawEnvelope>> {
    let mut statement = connection
        .prepare_cached("SELECT envelope_json FROM events WHERE session_id = ?1 AND seq = ?2")
        .map_err(map_sqlite_error)?;
    let mut rows = statement
        .query(params![session_id.as_str(), to_sqlite_integer(seq)?])
        .map_err(map_sqlite_error)?;
    rows.next()
        .map_err(map_sqlite_error)?
        .map(|row| {
            decode_envelope_column(row, 0).map_err(|error| {
                corrupt(format!(
                    "invalid envelope for session {session_id}, seq {seq}: {error}"
                ))
            })
        })
        .transpose()
}

fn load_envelope_by_event_id(
    transaction: &Transaction<'_>,
    event_id: &EventId,
) -> StoreResult<Option<RawEnvelope>> {
    let mut statement = transaction
        .prepare_cached("SELECT envelope_json FROM events WHERE event_id = ?1")
        .map_err(map_sqlite_error)?;
    let mut rows = statement
        .query([event_id.as_str()])
        .map_err(map_sqlite_error)?;
    rows.next()
        .map_err(map_sqlite_error)?
        .map(|row| {
            decode_envelope_column(row, 0)
                .map_err(|error| corrupt(format!("invalid envelope for event {event_id}: {error}")))
        })
        .transpose()
}

fn opened_menu(opening: &RawEnvelope, menu_id: &MenuId) -> StoreResult<Menu> {
    let payload = decode_payload::<EventPayload>(&opening.payload).map_err(|_| {
        store_error(
            ErrorCode::MenuNotFound,
            format!("event {} is not a recognized menu request", opening.seq),
            false,
        )
    })?;
    match payload {
        EventPayload::MenuOpened(menu) if menu.id == *menu_id => Ok(menu),
        _ => Err(store_error(
            ErrorCode::MenuNotFound,
            format!("event {} does not open menu {}", opening.seq, menu_id),
            false,
        )),
    }
}

fn validate_answer(
    menu: &Menu,
    answer: &MenuAnswer,
    input_is_secret_reference: bool,
) -> StoreResult<()> {
    if matches!(menu.kind, MenuKind::Secret) {
        if !input_is_secret_reference || answer.value.as_deref().is_none_or(str::is_empty) {
            return Err(store_error(
                ErrorCode::InvalidArgument,
                "secret menus require a non-empty vault reference",
                false,
            ));
        }
    } else if input_is_secret_reference {
        return Err(store_error(
            ErrorCode::InvalidArgument,
            "vault references are accepted only by secret menus",
            false,
        ));
    }
    if menu.options.is_empty() {
        if !matches!(
            menu.kind,
            MenuKind::Question | MenuKind::Secret | MenuKind::File
        ) || answer.option_index != 0
            || answer
                .option_key
                .as_deref()
                .is_some_and(|key| !key.is_empty())
        {
            return Err(store_error(
                ErrorCode::InvalidArgument,
                "menu answer does not match the committed option version",
                false,
            ));
        }
        return Ok(());
    }
    let option = usize::try_from(answer.option_index)
        .ok()
        .and_then(|index| menu.options.get(index))
        .ok_or_else(|| {
            store_error(
                ErrorCode::InvalidArgument,
                "menu answer option index is outside the committed menu",
                false,
            )
        })?;
    if answer.option_key.as_deref() != Some(option.key.as_str()) {
        return Err(store_error(
            ErrorCode::InvalidArgument,
            "menu answer key and index do not match the committed menu version",
            false,
        ));
    }
    Ok(())
}

/// Scans the journal after the menu's opening event (`command.request_seq`)
/// for a resolution or closure the `menu_resolutions` index predates.
fn historical_resolution(
    transaction: &Transaction<'_>,
    command: &MenuResolutionCommand,
) -> StoreResult<Option<u64>> {
    let mut statement = transaction
        .prepare_cached(
            "SELECT seq, envelope_json
             FROM events
             WHERE session_id = ?1 AND seq > ?2
             ORDER BY seq ASC",
        )
        .map_err(map_sqlite_error)?;
    let mut rows = statement
        .query(params![
            command.session_id.as_str(),
            to_sqlite_integer(command.request_seq)?
        ])
        .map_err(map_sqlite_error)?;
    while let Some(row) = rows.next().map_err(map_sqlite_error)? {
        let seq = sql_u64(row.get(0).map_err(map_sqlite_error)?).map_err(map_sqlite_error)?;
        let envelope = decode_envelope_column(row, 1).map_err(|error| {
            corrupt(format!(
                "invalid envelope JSON for session {}, seq {seq}: {error}",
                command.session_id
            ))
        })?;
        let Ok(payload) = serde_json::from_value::<EventPayload>(envelope.payload) else {
            continue;
        };
        match payload {
            EventPayload::MenuAnswered(answer) if answer.menu == command.answer.menu => {
                return Ok(Some(seq));
            }
            EventPayload::MenuClosed { menu, .. } if menu == command.answer.menu => {
                return Err(store_error(
                    ErrorCode::MenuNotFound,
                    format!("menu {} is no longer pending", command.answer.menu),
                    false,
                ));
            }
            _ => {}
        }
    }
    Ok(None)
}

fn menu_resolution_event_id(command: &MenuResolutionCommand) -> EventId {
    let mut hasher = blake3::Hasher::new();
    for part in [
        command.session_id.as_str(),
        command.answer.menu.as_str(),
        &command.command_id,
    ] {
        let length = u64::try_from(part.len()).unwrap_or(u64::MAX);
        hasher.update(&length.to_be_bytes());
        hasher.update(part.as_bytes());
    }
    EventId::new(format!("menu-resolution-{}", hasher.finalize().to_hex()))
}

fn sql_u64(value: i64) -> rusqlite::Result<u64> {
    u64::try_from(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })
}

/// Advances and returns the profile-owned fencing generation.
///
/// `Store::open` calls this only after acquiring the exclusive profile lock
/// and after every other fallible setup step, so each successful open consumes
/// exactly one generation.
fn next_worker_generation(connection: &mut Connection) -> StoreResult<u64> {
    next_profile_counter(connection, "worker_generation", "worker generation")
}

/// Compare-and-set increment of one `profile_meta` singleton counter, in an
/// immediate transaction. `column` is a compile-time-constant identifier
/// (`worker_generation` / `daemon_generation`) — the `format!` SQL never
/// carries external input.
fn next_profile_counter(
    connection: &mut Connection,
    column: &str,
    description: &str,
) -> StoreResult<u64> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sqlite_error)?;
    let select = format!("SELECT {column} FROM profile_meta WHERE singleton = 1");
    let current: i64 = transaction
        .query_row(&select, [], |row| row.get(0))
        .map_err(map_sqlite_error)?;
    let next = current
        .checked_add(1)
        .ok_or_else(|| corrupt(format!("{description} space is exhausted")))?;
    let update = format!(
        "UPDATE profile_meta
         SET {column} = ?1
         WHERE singleton = 1 AND {column} = ?2"
    );
    let updated = transaction
        .execute(&update, params![next, current])
        .map_err(map_sqlite_error)?;
    if updated != 1 {
        return Err(corrupt(format!(
            "profile metadata is missing its {description} singleton"
        )));
    }
    transaction.commit().map_err(map_sqlite_error)?;
    u64::try_from(next).map_err(|_| corrupt(format!("database contains a negative {description}")))
}

impl EventStore for Store {
    fn append(&self, envelopes: &mut [RawEnvelope]) -> StoreResult<CommittedSeqRange> {
        append_envelopes(self, envelopes, false)
    }

    fn read(
        &self,
        session: &SessionId,
        since_seq: u64,
        limit: usize,
    ) -> StoreResult<Vec<RawEnvelope>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let connection = self.connection()?;
        // A limit beyond i64::MAX is effectively unbounded; clamp, don't error.
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut statement = connection
            .prepare_cached(
                "SELECT seq, envelope_json, event_id, committed_at_ms
                 FROM events
                 WHERE session_id = ?1 AND seq > ?2
                 ORDER BY seq ASC
                 LIMIT ?3",
            )
            .map_err(map_sqlite_error)?;
        let mut rows = statement
            .query(params![
                session.as_str(),
                to_sqlite_integer(since_seq)?,
                limit
            ])
            .map_err(map_sqlite_error)?;
        let mut envelopes = Vec::new();
        while let Some(row) = rows.next().map_err(map_sqlite_error)? {
            let stored_seq: i64 = row.get(0).map_err(map_sqlite_error)?;
            let stored_event_id: String = row.get(2).map_err(map_sqlite_error)?;
            let stored_committed_at_ms: i64 = row.get(3).map_err(map_sqlite_error)?;
            let envelope = decode_envelope_column(row, 1).map_err(|error| {
                corrupt(format!(
                    "invalid envelope for session {session}, seq {stored_seq}: {error}"
                ))
            })?;
            validate_stored_envelope(
                session,
                stored_seq,
                &stored_event_id,
                stored_committed_at_ms,
                &envelope,
            )?;
            envelopes.push(envelope);
        }
        Ok(envelopes)
    }

    fn latest_seq(&self, session: &SessionId) -> StoreResult<u64> {
        let connection = self.connection()?;
        let latest: i64 = connection
            .prepare_cached("SELECT COALESCE(MAX(seq), 0) FROM events WHERE session_id = ?1")
            .and_then(|mut statement| statement.query_row([session.as_str()], |row| row.get(0)))
            .map_err(map_sqlite_error)?;
        u64::try_from(latest).map_err(|_| corrupt("database contains a negative event sequence"))
    }
}

struct AppendTransactionOutcome {
    range: CommittedSeqRange,
    stamped: Vec<RawEnvelope>,
    changes_graph_reduction: bool,
    changes_graph_telemetry: bool,
}

fn append_envelopes(
    store: &Store,
    envelopes: &mut [RawEnvelope],
    validate_worker_transitions: bool,
) -> StoreResult<CommittedSeqRange> {
    let mut connection = store.connection()?;
    // IMMEDIATE makes durable-head validation, sequence allocation, and the
    // batch insert one indivisible write critical section.
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sqlite_error)?;
    let outcome =
        append_envelopes_in_transaction(&transaction, envelopes, validate_worker_transitions)?;
    transaction.commit().map_err(map_sqlite_error)?;
    envelopes.clone_from_slice(&outcome.stamped);
    update_append_caches(
        store,
        &connection,
        &outcome.range.session_id,
        envelopes,
        outcome.changes_graph_reduction,
        outcome.changes_graph_telemetry,
    );
    Ok(outcome.range)
}

fn append_envelopes_in_transaction(
    transaction: &Connection,
    envelopes: &[RawEnvelope],
    validate_worker_transitions: bool,
) -> StoreResult<AppendTransactionOutcome> {
    let (session, batch_len) = same_session_batch(envelopes)?;
    let changes_graph_reduction = envelopes
        .iter()
        .any(|envelope| graph_reduction_event(&envelope.payload));
    let changes_graph_telemetry = envelopes
        .iter()
        .any(|envelope| graph_telemetry_event(&envelope.payload));
    let committed_at_ms = now_ms()?;
    let committed_at_sql = to_sqlite_integer(committed_at_ms)?;
    transaction
        .prepare_cached(
            "INSERT OR IGNORE INTO sessions(id, created_at_ms, meta_json) VALUES (?1, ?2, ?3)",
        )
        .and_then(|mut statement| {
            statement.execute(params![session.as_str(), committed_at_sql, "{}"])
        })
        .map_err(map_sqlite_error)?;
    let run_head_metadata = ensure_run_head_projection(transaction, &session)?;
    if validate_worker_transitions {
        validate_worker_run_transitions(
            transaction,
            &session,
            envelopes,
            run_head_metadata.run_count,
        )?;
    }
    let latest: i64 = transaction
        .prepare_cached("SELECT COALESCE(MAX(seq), 0) FROM events WHERE session_id = ?1")
        .and_then(|mut statement| statement.query_row([session.as_str()], |row| row.get(0)))
        .map_err(map_sqlite_error)?;
    let first_seq = u64::try_from(latest)
        .map_err(|_| corrupt("database contains a negative event sequence"))?
        .checked_add(1)
        .ok_or_else(|| corrupt("event sequence space is exhausted"))?;
    let last_seq = first_seq
        .checked_add(batch_len - 1)
        .ok_or_else(|| corrupt("event sequence space is exhausted"))?;
    let mut stamped = Vec::with_capacity(envelopes.len());
    {
        let mut insert = transaction
            .prepare_cached(
                "INSERT INTO events(
                    session_id, seq, envelope_json, event_id, committed_at_ms, payload_kind
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )
            .map_err(map_sqlite_error)?;
        for (seq, envelope) in (first_seq..=last_seq).zip(envelopes.iter()) {
            let mut envelope = envelope.clone();
            envelope.seq = seq;
            envelope.committed_at_ms = committed_at_ms;
            stamp_queue_delta(&mut envelope)?;
            stamp_workspace_mutation(transaction, &mut envelope)?;
            let envelope_bytes = encode_envelope(&envelope).map_err(|error| {
                store_error(
                    ErrorCode::InvalidArgument,
                    format!("cannot serialize event envelope: {error}"),
                    false,
                )
            })?;
            insert
                .execute(params![
                    session.as_str(),
                    to_sqlite_integer(seq)?,
                    envelope_bytes,
                    envelope.event_id.as_str(),
                    committed_at_sql,
                    payload_kind(&envelope),
                ])
                .map_err(map_sqlite_error)?;
            enqueue_hook_dispatch(transaction, &envelope)?;
            stamped.push(envelope);
        }
    }
    update_run_head_projection_after_append(transaction, &session, &stamped)?;
    update_workflow_graph_projection_after_append(transaction, &session, &stamped)?;
    update_branch_heads(transaction, &stamped)?;
    Ok(AppendTransactionOutcome {
        range: CommittedSeqRange {
            session_id: session,
            first_seq,
            last_seq,
        },
        stamped,
        changes_graph_reduction,
        changes_graph_telemetry,
    })
}

fn update_append_caches(
    store: &Store,
    connection: &Connection,
    session: &SessionId,
    envelopes: &[RawEnvelope],
    changes_graph_reduction: bool,
    changes_graph_telemetry: bool,
) {
    if changes_graph_reduction {
        store.extend_graph_reduction(connection, session, envelopes);
    } else if changes_graph_telemetry {
        store.extend_graph_telemetry(connection, session, envelopes);
    }
}

fn isolatable_append_error(error: &HaiderError) -> bool {
    !matches!(
        error.code,
        ErrorCode::StoreCorrupt
            | ErrorCode::StoreLocked
            | ErrorCode::StoreFull
            | ErrorCode::StoreReadOnly
            | ErrorCode::StoreUnavailable
            | ErrorCode::Internal
    )
}

fn workspace_revision_for_seq(seq: u64) -> WorkspaceRevision {
    WorkspaceRevision::new(format!("workspace-revision:{seq}"))
}

fn stamp_queue_delta(envelope: &mut RawEnvelope) -> StoreResult<()> {
    if envelope
        .payload
        .get("type")
        .and_then(serde_json::Value::as_str)
        != Some("queue_changed")
    {
        return Ok(());
    }
    // MUTATION CHECK: making this decode fail-open permits a reserved
    // queue_changed payload without its required revision to commit and ride
    // the ordinary attachment stream.
    let EventPayload::QueueChanged(mut delta) =
        serde_json::from_value::<EventPayload>(envelope.payload.clone()).map_err(|error| {
            store_error(
                ErrorCode::InvalidArgument,
                format!("invalid reserved queue_changed payload: {error}"),
                false,
            )
        })?
    else {
        return Err(store_error(
            ErrorCode::InvalidArgument,
            "reserved queue_changed payload decoded as another event type",
            false,
        ));
    };
    delta.revision = envelope.seq;
    if let QueueChange::Enqueued { row } = &mut delta.change {
        row.created_at_ms = envelope.committed_at_ms;
    }
    envelope.payload =
        serde_json::to_value(EventPayload::QueueChanged(delta)).map_err(|error| {
            store_error(
                ErrorCode::InvalidArgument,
                format!("cannot serialize stamped queue delta: {error}"),
                false,
            )
        })?;
    Ok(())
}

#[allow(clippy::result_large_err)]
fn stamp_workspace_mutation(
    transaction: &Connection,
    envelope: &mut RawEnvelope,
) -> StoreResult<()> {
    if let Ok(EventPayload::Effect(EffectPhase::Outcome {
        effect,
        workspace_mutation: Some(mut mutation),
        outcome,
        freshness,
    })) = serde_json::from_value::<EventPayload>(envelope.payload.clone())
    {
        validate_workspace_mutation_intent(transaction, envelope, &effect, &mutation)?;
        stamp_workspace_mutation_fields(envelope.seq, &effect, &mut mutation);
        envelope.payload = serde_json::to_value(EventPayload::Effect(EffectPhase::Outcome {
            effect,
            outcome,
            freshness,
            workspace_mutation: Some(mutation),
        }))
        .map_err(|error| {
            store_error(
                ErrorCode::InvalidArgument,
                format!("cannot serialize workspace mutation outcome: {error}"),
                false,
            )
        })?;
        return Ok(());
    }
    let Some(TaskEventPayload::TaskCompleted(mut completed)) =
        TaskEventPayload::from_payload_value(&envelope.payload)
    else {
        return Ok(());
    };
    let Some(mut mutation) = completed.workspace_mutation.take() else {
        return Ok(());
    };
    let effect = mutation.effect_id.clone();
    validate_workspace_mutation_intent(transaction, envelope, &effect, &mutation)?;
    stamp_workspace_mutation_fields(envelope.seq, &effect, &mut mutation);
    completed.workspace_mutation = Some(mutation);
    envelope.payload = completed.to_payload_value().map_err(|error| {
        store_error(
            ErrorCode::InvalidArgument,
            format!("cannot serialize background workspace mutation: {error}"),
            false,
        )
    })?;
    Ok(())
}

fn stamp_workspace_mutation_fields(seq: u64, effect: &EffectId, mutation: &mut WorkspaceMutation) {
    let revision = workspace_revision_for_seq(seq);
    let subject_digest =
        workspace_mutation_subject_digest(effect, &mutation.mutation_digest, &revision);
    mutation.workspace_revision = Some(revision);
    mutation.subject_digest = Some(subject_digest);
}

#[allow(clippy::result_large_err)]
fn validate_workspace_mutation_intent(
    transaction: &Connection,
    envelope: &RawEnvelope,
    effect: &EffectId,
    mutation: &WorkspaceMutation,
) -> StoreResult<()> {
    if mutation.effect_id != *effect || mutation.mutation_digest.trim().is_empty() {
        return Err(store_error(
            ErrorCode::InvalidArgument,
            "workspace mutation must match its effect and carry a non-empty digest",
            false,
        ));
    }
    let mut statement = transaction
        .prepare_cached(
            "SELECT envelope_json FROM events
             WHERE session_id = ?1
               AND (
                   payload_kind = 'effect'
                   OR (
                       payload_kind IS NULL
                       AND instr(envelope_json, '\"type\":\"effect\"') > 0
                   )
               )
             ORDER BY seq ASC",
        )
        .map_err(map_sqlite_error)?;
    let mut rows = statement
        .query([envelope.session_id.as_str()])
        .map_err(map_sqlite_error)?;
    while let Some(row) = rows.next().map_err(map_sqlite_error)? {
        let candidate = decode_envelope_column(row, 0).map_err(|error| {
            corrupt(format!(
                "invalid effect envelope in session {}: {error}",
                envelope.session_id
            ))
        })?;
        let Ok(EventPayload::Effect(EffectPhase::Intent(intent))) =
            serde_json::from_value::<EventPayload>(candidate.payload)
        else {
            continue;
        };
        if intent.effect != *effect {
            continue;
        }
        if candidate.run_id != envelope.run_id
            || !matches!(
                intent.class,
                EffectClass::FsWrite | EffectClass::ProcessExec
            )
        {
            return Err(store_error(
                ErrorCode::InvalidArgument,
                "workspace mutation does not match a mutation-class effect intent in the same run",
                false,
            ));
        }
        return Ok(());
    }
    Err(store_error(
        ErrorCode::InvalidArgument,
        "workspace mutation has no durable mutation-class effect intent",
        false,
    ))
}

fn graph_reduction_event(payload: &serde_json::Value) -> bool {
    payload
        .get("type")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|kind| {
            kind.starts_with("graph_")
                || kind == "todo_graph_attached"
                || kind == "evidence_recorded"
                || kind.starts_with("menu_")
        })
}

fn graph_telemetry_event(payload: &serde_json::Value) -> bool {
    if graph_reduction_event(payload) {
        return true;
    }
    match payload.get("type").and_then(serde_json::Value::as_str) {
        Some("tool_result") => true,
        Some("item") => {
            payload
                .get("item")
                .and_then(|item| item.get("item"))
                .and_then(serde_json::Value::as_str)
                == Some("tool_call")
        }
        _ => false,
    }
}

fn budget_terminal_runs(envelopes: &[RawEnvelope]) -> HashSet<RunId> {
    envelopes
        .windows(2)
        .filter_map(|pair| {
            let run_id = pair[0].run_id.as_ref()?;
            if pair[1].run_id.as_ref() != Some(run_id) {
                return None;
            }
            let budget_failure = matches!(
                serde_json::from_value::<EventPayload>(pair[0].payload.clone()),
                Ok(EventPayload::RunFailed {
                    code: ErrorCode::BudgetExhausted,
                    ..
                })
            );
            let errored = matches!(
                serde_json::from_value::<EventPayload>(pair[1].payload.clone()),
                Ok(EventPayload::RunState(RunState::Errored))
            );
            (budget_failure && errored).then(|| run_id.clone())
        })
        .collect()
}

fn durable_headless_run_facts(
    transaction: &Connection,
    session_id: &SessionId,
    run_id: &RunId,
) -> StoreResult<(bool, bool)> {
    let mut statement = transaction
        .prepare_cached(
            "SELECT seq, envelope_json, event_id, committed_at_ms
             FROM events
             WHERE session_id = ?1
               AND payload_kind IN ('headless_run_configured', 'run_budget_exhausted')
             ORDER BY seq ASC",
        )
        .map_err(map_sqlite_error)?;
    let mut rows = statement
        .query([session_id.as_str()])
        .map_err(map_sqlite_error)?;
    let mut configured = false;
    let mut budget_exhausted = false;
    while let Some(row) = rows.next().map_err(map_sqlite_error)? {
        let stored_seq: i64 = row.get(0).map_err(map_sqlite_error)?;
        let stored_event_id: String = row.get(2).map_err(map_sqlite_error)?;
        let stored_committed_at_ms: i64 = row.get(3).map_err(map_sqlite_error)?;
        let envelope = decode_envelope_column(row, 1).map_err(|error| {
            corrupt(format!(
                "invalid headless envelope for session {session_id}, seq {stored_seq}: {error}"
            ))
        })?;
        validate_stored_envelope(
            session_id,
            stored_seq,
            &stored_event_id,
            stored_committed_at_ms,
            &envelope,
        )?;
        if envelope.run_id.as_ref() != Some(run_id) || !envelope.render.durable {
            continue;
        }
        match HeadlessRunEventPayload::from_payload_value(&envelope.payload) {
            Some(HeadlessRunEventPayload::HeadlessRunConfigured(_)) => configured = true,
            Some(HeadlessRunEventPayload::RunBudgetExhausted(_)) if configured => {
                budget_exhausted = true;
            }
            Some(HeadlessRunEventPayload::RunBudgetExhausted(_)) => {}
            None => {}
        }
        if configured && budget_exhausted {
            break;
        }
    }
    Ok((configured, budget_exhausted))
}

fn validate_worker_run_transitions(
    transaction: &Connection,
    session_id: &SessionId,
    envelopes: &[RawEnvelope],
    projection_run_count: u64,
) -> StoreResult<()> {
    let mut states = latest_run_states_for(
        transaction,
        session_id,
        envelopes
            .iter()
            .filter_map(|envelope| envelope.run_id.as_ref()),
        projection_run_count,
    )?;
    // Manual idle compaction is an internal maintenance run. Its atomic final
    // batch carries the projection-switch node and `Done`, but that `Done`
    // does not finalize provider work or discharge graph obligations.
    let manual_compaction_runs = envelopes
        .iter()
        .filter_map(|envelope| {
            let manual_node = matches!(
                serde_json::from_value::<EventPayload>(envelope.payload.clone()),
                Ok(EventPayload::NodeCommitted(TreeNode {
                    kind: NodeKind::Compaction {
                        resume_cause: haider_protocol::history::CompactionResume::ManualIdle,
                        ..
                    },
                    ..
                }))
            );
            let run_id = envelope.run_id.as_ref()?;
            (manual_node
                && states
                    .get(run_id)
                    .is_some_and(|(state, _, _)| state == &RunState::Compacting))
            .then_some(run_id.clone())
        })
        .collect::<HashSet<_>>();
    let budget_terminal_runs = budget_terminal_runs(envelopes);
    let commits_graph_guarded_done = envelopes.iter().any(|envelope| {
        matches!(
            serde_json::from_value::<EventPayload>(envelope.payload.clone()),
            Ok(EventPayload::RunState(RunState::Done))
        ) && envelope
            .run_id
            .as_ref()
            .is_none_or(|run_id| !manual_compaction_runs.contains(run_id))
    });
    if commits_graph_guarded_done
        && load_graph_reduction(transaction, session_id)?
            .status
            .is_some_and(|status| {
                status.phase == GraphPhase::Active
                    && (status
                        .run_set
                        .as_ref()
                        .is_some_and(|run_set| !run_set.is_complete())
                        || status.nodes.iter().any(|node| !node.satisfied))
            })
    {
        // Final guard against a pin/switch racing the provider's earlier
        // guard decision. MUTATION: removing this check permits a provider
        // Done to land beside newly unmet obligations.
        return Err(store_error(
            ErrorCode::GraphNotActive,
            "cannot commit Done while the active Convergence Graph has unmet obligations",
            false,
        ));
    }
    for envelope in envelopes {
        let supplemental_project_instructions =
            ProjectInstructionsLoaded::from_payload_value(&envelope.payload).is_some();
        let supplemental_computer_permission =
            PermissionEventPayload::from_payload_value(envelope.payload.clone()).is_ok();
        let supplemental_run_budget = matches!(
            HeadlessRunEventPayload::from_payload_value(&envelope.payload),
            Some(HeadlessRunEventPayload::RunBudgetExhausted(_))
        );
        let Some(run_id) = envelope.run_id.as_ref() else {
            if supplemental_project_instructions
                || supplemental_computer_permission
                || supplemental_run_budget
            {
                return Err(store_error(
                    ErrorCode::InvalidArgument,
                    "supplemental worker fact has no logical-turn run id",
                    false,
                ));
            }
            continue;
        };
        if (supplemental_project_instructions
            || supplemental_computer_permission
            || supplemental_run_budget)
            && (!envelope.render.durable || envelope.render.prompt != PromptRender::Omit)
        {
            return Err(store_error(
                ErrorCode::InvalidArgument,
                "supplemental worker fact must be durable and omitted from prompt replay",
                false,
            ));
        }
        if supplemental_run_budget
            && !durable_headless_run_facts(transaction, session_id, run_id)?.0
        {
            return Err(store_error(
                ErrorCode::InvalidArgument,
                format!("worker run {run_id} has no durable headless configuration"),
                false,
            ));
        }
        let payload = match serde_json::from_value::<EventPayload>(envelope.payload.clone()) {
            Ok(payload) => Some(payload),
            Err(_)
                if supplemental_project_instructions
                    || supplemental_computer_permission
                    || supplemental_run_budget =>
            {
                None
            }
            Err(error) => {
                return Err(store_error(
                    ErrorCode::InvalidArgument,
                    format!("worker envelope payload is invalid: {error}"),
                    false,
                ));
            }
        };
        if !states.contains_key(run_id)
            && matches!(
                &payload,
                Some(EventPayload::Item(ItemEvent::Started {
                    item: TurnItem::Extension { kind, .. },
                    ..
                })) if kind == COMPACTION_INTENT_EXTENSION_KIND
            )
        {
            // A compaction intent is the accepted prefix of the daemon's
            // internal job kind. It deliberately has no synthetic user row.
            states.insert(
                run_id.clone(),
                (RunState::Compacting, 0, envelope.branch_id.clone()),
            );
            continue;
        }
        let Some((durable, _, accepted_branch)) = states.get(run_id).cloned() else {
            return Err(store_error(
                ErrorCode::RunNotActive,
                format!("worker run {run_id} has no durable accepted state"),
                false,
            ));
        };
        if envelope.branch_id != accepted_branch
            && !matches!(&payload, Some(EventPayload::SessionState(_)))
        {
            return Err(store_error(
                ErrorCode::InvalidArgument,
                format!("worker run {run_id} emitted on a different branch"),
                false,
            ));
        }
        if durable.is_terminal() {
            return Err(store_error(
                ErrorCode::RunNotActive,
                format!("worker run {run_id} is already terminal"),
                false,
            ));
        }
        if let Some(EventPayload::RunState(next)) = payload {
            let budget_terminal_wins = if durable == RunState::Cancelling
                && next == RunState::Errored
                && budget_terminal_runs.contains(run_id)
            {
                let (configured, budget_exhausted) =
                    durable_headless_run_facts(transaction, session_id, run_id)?;
                configured && budget_exhausted
            } else {
                false
            };
            if durable == RunState::Cancelling
                && next != RunState::Cancelled
                && !budget_terminal_wins
            {
                return Err(store_error(
                    ErrorCode::RunNotActive,
                    format!("worker run {run_id} is durably cancelling; only Cancelled may follow"),
                    false,
                ));
            }
            states.insert(run_id.clone(), (next, 0, accepted_branch));
        }
    }
    Ok(())
}

impl Cas for Store {
    fn put(&self, bytes: &[u8]) -> StoreResult<ArtifactRef> {
        self.cas.put(bytes)
    }

    fn put_file(&self, path: &Path) -> StoreResult<ArtifactRef> {
        self.cas.put_file(path)
    }

    fn put_image(
        &self,
        bytes: &[u8],
        media_type: &str,
    ) -> StoreResult<haider_protocol::tool::ImageBlockRef> {
        self.cas.put_image(bytes, media_type)
    }

    fn get(&self, artifact: &ArtifactRef) -> StoreResult<Vec<u8>> {
        self.cas.get(artifact)
    }

    fn verify(&self, artifact: &ArtifactRef) -> bool {
        self.cas.verify(artifact)
    }
}

struct StoredBranch {
    branch_id: String,
    name: String,
    source_branch_id: Option<String>,
    fork_node_id: String,
    fork_seq: i64,
    created_seq: i64,
    created_at_ms: i64,
    head_node_id: String,
    head_seq: i64,
}

fn branch_select() -> &'static str {
    "SELECT branch_id, display_name, source_branch_id, fork_node_id,
            fork_seq, created_seq, created_at_ms, head_node_id, head_seq
     FROM branches"
}

fn stored_branch(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredBranch> {
    Ok(StoredBranch {
        branch_id: row.get(0)?,
        name: row.get(1)?,
        source_branch_id: row.get(2)?,
        fork_node_id: row.get(3)?,
        fork_seq: row.get(4)?,
        created_seq: row.get(5)?,
        created_at_ms: row.get(6)?,
        head_node_id: row.get(7)?,
        head_seq: row.get(8)?,
    })
}

fn decode_branch(row: StoredBranch) -> StoreResult<BranchDescriptor> {
    Ok(BranchDescriptor {
        branch_id: BranchId::new(row.branch_id),
        name: row.name,
        source_branch_id: row.source_branch_id.map(BranchId::new),
        fork_node_id: NodeId::new(row.fork_node_id),
        fork_seq: sql_u64(row.fork_seq).map_err(map_sqlite_error)?,
        created_seq: sql_u64(row.created_seq).map_err(map_sqlite_error)?,
        created_at_ms: sql_u64(row.created_at_ms).map_err(map_sqlite_error)?,
        head_node_id: NodeId::new(row.head_node_id),
        head_seq: sql_u64(row.head_seq).map_err(map_sqlite_error)?,
    })
}

fn branch_descriptor(
    connection: &Connection,
    session_id: &SessionId,
    branch_id: &BranchId,
) -> StoreResult<Option<BranchDescriptor>> {
    let sql = format!(
        "{} WHERE session_id = ?1 AND branch_id = ?2",
        branch_select()
    );
    connection
        .query_row(
            &sql,
            params![session_id.as_str(), branch_id.as_str()],
            stored_branch,
        )
        .optional()
        .map_err(map_sqlite_error)?
        .map(decode_branch)
        .transpose()
}

fn branch_lineage_descriptors(
    connection: &Connection,
    session_id: &SessionId,
    branch_id: Option<&BranchId>,
) -> StoreResult<Vec<BranchDescriptor>> {
    let Some(mut current) = branch_id.cloned() else {
        return Ok(Vec::new());
    };
    let mut reverse = Vec::new();
    let mut seen = HashSet::new();
    loop {
        if !seen.insert(current.clone()) {
            return Err(corrupt(format!(
                "branch registry contains a lineage cycle at {current}"
            )));
        }
        let descriptor = branch_descriptor(connection, session_id, &current)?.ok_or_else(|| {
            store_error(
                ErrorCode::InvalidArgument,
                format!("branch {current} does not exist in session {session_id}"),
                false,
            )
        })?;
        let source = descriptor.source_branch_id.clone();
        reverse.push(descriptor);
        let Some(source) = source else {
            break;
        };
        current = source;
    }
    reverse.reverse();
    Ok(reverse)
}

fn branch_lineage_scopes(
    connection: &Connection,
    session_id: &SessionId,
    branch_id: Option<&BranchId>,
) -> StoreResult<HashMap<Option<BranchId>, u64>> {
    let lineage = branch_lineage_descriptors(connection, session_id, branch_id)?;
    let mut scopes = HashMap::new();
    let mut ceiling = u64::MAX;
    for descriptor in lineage.iter().rev() {
        scopes.insert(Some(descriptor.branch_id.clone()), ceiling);
        ceiling = ceiling.min(descriptor.fork_seq);
    }
    scopes.insert(None, ceiling);
    Ok(scopes)
}

fn validate_metafork_commit(
    fork: &SessionForkCommand,
    command: &SessionMetaforkCommit,
) -> StoreResult<()> {
    const MAX_DESCRIPTION_BYTES: usize = 16 * 1024;
    const MAX_REASON_BYTES: usize = 4 * 1024;
    const MAX_REMOVALS: usize = 256;
    if command.description.trim().is_empty()
        || command.description.len() > MAX_DESCRIPTION_BYTES
        || command.model_proposal.removals.is_empty()
        || command.model_proposal.removals.len() > MAX_REMOVALS
    {
        return Err(store_error(
            ErrorCode::InvalidArgument,
            "metafork description and bounded non-empty model proposal are required",
            false,
        ));
    }
    let mut reviewed_event_count = 0_usize;
    for (index, removal) in command.model_proposal.removals.iter().enumerate() {
        if removal.from_seq == 0
            || removal.through_seq < removal.from_seq
            || removal.reason.trim().is_empty()
            || removal.reason.len() > MAX_REASON_BYTES
            || removal
                .preview
                .as_ref()
                .is_some_and(|preview| preview.trim().is_empty() || preview.len() > 2 * 1024)
        {
            return Err(store_error(
                ErrorCode::InvalidArgument,
                "metafork removal ranges and reasons must be valid",
                false,
            ));
        }
        reviewed_event_count = reviewed_event_count.saturating_add(removal.reviewed_events.len());
        if reviewed_event_count > 512
            || removal.reviewed_events.iter().any(|event| {
                event.source_seq < removal.from_seq
                    || event.source_seq > removal.through_seq
                    || event.source_event_id.as_str().is_empty()
                    || event.payload_kind.trim().is_empty()
                    || event.excerpt.is_empty()
                    || event.excerpt.len() > 384
            })
        {
            return Err(store_error(
                ErrorCode::InvalidArgument,
                "metafork reviewed-event roster must be bounded and remain inside its range",
                false,
            ));
        }
        if command
            .model_proposal
            .removals
            .iter()
            .take(index)
            .any(|prior| {
                removal.from_seq <= prior.through_seq && prior.from_seq <= removal.through_seq
            })
        {
            return Err(store_error(
                ErrorCode::InvalidArgument,
                "metafork removal ranges must not overlap",
                false,
            ));
        }
    }
    let expected = SessionMetaforkReviewManifest {
        command_id: fork.command_id.clone(),
        source_session_id: fork.source_session_id.clone(),
        worker_generation: fork.worker_generation,
        source_branch_id: fork.source_branch_id.clone(),
        fork_node_id: fork.fork_node_id.clone(),
        fork_seq: fork.fork_seq,
        name: fork.name.clone(),
        description: command.description.clone(),
        model_proposal: command.model_proposal.clone(),
    }
    .digest()
    .map_err(|error| {
        store_error(
            ErrorCode::InvalidArgument,
            format!("cannot digest metafork review manifest: {error}"),
            false,
        )
    })?;
    if command.accepted_proposal_digest != expected {
        return Err(store_error(
            ErrorCode::InvalidArgument,
            "metafork acceptance does not match the reviewed proposal",
            false,
        ));
    }
    Ok(())
}

/// Content address for the complete exact provider view used to justify a
/// fork cache-prefix inheritance decision.
pub fn fork_provider_view_prefix_digest(provider_view: &serde_json::Value) -> StoreResult<String> {
    let ledger =
        serde_json::from_value::<ProviderViewLedgerV1>(provider_view.clone()).map_err(|error| {
            store_error(
                ErrorCode::InvalidArgument,
                format!("cannot decode fork provider-view ledger: {error}"),
                false,
            )
        })?;
    ledger.prefix_digest().map_err(|error| {
        store_error(
            ErrorCode::InvalidArgument,
            format!("cannot digest fork provider-view ledger: {error}"),
            false,
        )
    })
}

struct SourceForkCacheBoundary {
    seq: u64,
    inherited_segment: Option<ForkCacheSegmentV1>,
}

fn source_fork_cache_boundary(
    connection: &Connection,
    source_session_id: &SessionId,
) -> StoreResult<Option<SourceForkCacheBoundary>> {
    let mut statement = connection
        .prepare_cached(
            "SELECT seq, envelope_json, event_id, committed_at_ms
             FROM events
             WHERE session_id = ?1 AND payload_kind = 'session_forked'
             ORDER BY seq DESC LIMIT 1",
        )
        .map_err(map_sqlite_error)?;
    let mut rows = statement
        .query([source_session_id.as_str()])
        .map_err(map_sqlite_error)?;
    let Some(row) = rows.next().map_err(map_sqlite_error)? else {
        return Ok(None);
    };
    let stored_seq: i64 = row.get(0).map_err(map_sqlite_error)?;
    let stored_event_id: String = row.get(2).map_err(map_sqlite_error)?;
    let stored_committed_at_ms: i64 = row.get(3).map_err(map_sqlite_error)?;
    let envelope = decode_envelope_column(row, 1).map_err(|error| {
        corrupt(format!(
            "invalid session-fork boundary for session {source_session_id}, seq {stored_seq}: {error}"
        ))
    })?;
    validate_stored_envelope(
        source_session_id,
        stored_seq,
        &stored_event_id,
        stored_committed_at_ms,
        &envelope,
    )?;
    let seq = u64::try_from(stored_seq)
        .map_err(|_| corrupt("session-fork boundary sequence is negative"))?;
    let inherited_segment =
        SessionForked::from_payload_value(&envelope.payload).and_then(|record| {
            (record.context_epoch == ForkContextEpoch::Inherited)
                .then_some(record.inherited_cache_segment)
                .flatten()
        });
    Ok(Some(SourceForkCacheBoundary {
        seq,
        inherited_segment,
    }))
}

fn inherited_fork_cache_segment(
    source_envelopes: &[RawEnvelope],
    source_owner_agent: Option<&AgentId>,
    source_metadata: &SessionMetadataV1,
    source_session_id: &SessionId,
    source_cache_boundary: Option<&SourceForkCacheBoundary>,
    candidate: &ForkCacheInheritanceCandidate,
) -> Option<ForkCacheSegmentV1> {
    const PROVIDER_VIEW_ATTEMPT_KIND: &str = "provider_view_attempt_v1";
    const PROVIDER_VIEW_ATTEMPT_PREFIX: &str = "provider_view_attempt_";

    for envelope in source_envelopes.iter().rev() {
        if envelope.agent_id.as_ref() != source_owner_agent {
            continue;
        }
        let payload = match serde_json::from_value::<EventPayload>(envelope.payload.clone()) {
            Ok(payload) => payload,
            Err(_)
                if raw_provider_view_kind(&envelope.payload)
                    .is_some_and(|kind| kind.starts_with(PROVIDER_VIEW_ATTEMPT_PREFIX)) =>
            {
                return None;
            }
            Err(_) => continue,
        };
        let EventPayload::Item(item_event) = payload else {
            continue;
        };
        let item = match item_event {
            ItemEvent::Completed { item, .. } => item,
            ItemEvent::Started {
                item: TurnItem::Extension { kind, .. },
                ..
            } if kind.starts_with(PROVIDER_VIEW_ATTEMPT_PREFIX) => return None,
            ItemEvent::Started { .. } | ItemEvent::Delta { .. } => continue,
        };
        let TurnItem::Extension { kind, data } = item else {
            continue;
        };
        if kind != PROVIDER_VIEW_ATTEMPT_KIND {
            if kind.starts_with(PROVIDER_VIEW_ATTEMPT_PREFIX) {
                return None;
            }
            continue;
        }

        // The newest known ledger is authoritative. A malformed newest
        // record must not fall back to an older, apparently compatible view.
        data.get("ordinal")?.as_u64()?;
        let source_view = data.get("view")?;
        if !provider_views_share_cache_prefix(source_view, &candidate.provider_view) {
            return None;
        }
        let provider = source_view.get("provider")?.as_str()?;
        let model = source_view.get("model")?.as_str()?;
        let account_scope = source_view.get("account_scope")?.as_str()?;
        let cache_epoch = source_view.get("cache_epoch")?.as_str()?;
        let stable_history_end = source_view.get("stable_history_end")?.as_u64()?;
        if provider != source_metadata.provider
            || model != source_metadata.model
            || account_scope.is_empty()
            || cache_epoch.is_empty()
        {
            return None;
        }
        let prefix_digest = fork_provider_view_prefix_digest(source_view).ok()?;
        let cache_route = effective_source_cache_route(
            source_cache_boundary,
            source_session_id,
            envelope.seq,
            provider,
            model,
            account_scope,
            cache_epoch,
            stable_history_end,
            &prefix_digest,
        )?;
        return Some(ForkCacheSegmentV1 {
            provider: provider.to_owned(),
            model: model.to_owned(),
            account_scope: account_scope.to_owned(),
            cache_route,
            cache_epoch: cache_epoch.to_owned(),
            prefix_digest,
            stable_history_end,
            source_provider_view_seq: envelope.seq,
            source_provider_view_event_id: envelope.event_id.clone(),
        });
    }
    None
}

// Every argument is a fail-closed fork-inheritance validation input (C3); a
// params struct would only relocate the same nine facts.
#[allow(clippy::too_many_arguments)]
fn effective_source_cache_route(
    source_cache_boundary: Option<&SourceForkCacheBoundary>,
    source_session_id: &SessionId,
    source_provider_view_seq: u64,
    provider: &str,
    model: &str,
    account_scope: &str,
    cache_epoch: &str,
    stable_history_end: u64,
    prefix_digest: &str,
) -> Option<String> {
    let Some(boundary) = source_cache_boundary else {
        return Some(source_session_id.as_str().to_owned());
    };
    if let Some(segment) = boundary.inherited_segment.as_ref() {
        let inherited_prefix = segment.provider == provider
            && segment.model == model
            && segment.account_scope == account_scope
            && segment.cache_epoch == cache_epoch
            && segment.stable_history_end == stable_history_end
            && segment.prefix_digest == prefix_digest
            && !segment.cache_route.is_empty();
        if inherited_prefix {
            return Some(segment.cache_route.clone());
        }
    }
    (source_provider_view_seq >= boundary.seq).then(|| source_session_id.as_str().to_owned())
}

fn raw_provider_view_kind(payload: &serde_json::Value) -> Option<&str> {
    payload.get("item")?.get("kind")?.as_str()
}

fn provider_views_share_cache_prefix(
    source: &serde_json::Value,
    child: &serde_json::Value,
) -> bool {
    if !provider_view_is_complete(source) || !provider_view_is_complete(child) {
        return false;
    }
    let same_domain = [
        "provider",
        "model",
        "dialect",
        "serialization_version",
        "header_epoch",
        "cache_epoch",
        "compaction_epoch",
        "reasoning_retention",
        "account_scope",
        "trim_sentinel",
        "system_block",
        "tool_schema_block",
    ]
    .into_iter()
    .all(|field| source.get(field) == child.get(field));
    if !same_domain {
        return false;
    }
    let Some(source_history) = source
        .get("history_blocks")
        .and_then(serde_json::Value::as_array)
    else {
        return false;
    };
    let Some(child_history) = child
        .get("history_blocks")
        .and_then(serde_json::Value::as_array)
    else {
        return false;
    };
    let stable_boundary_grew = source
        .get("stable_history_end")
        .and_then(serde_json::Value::as_u64)
        .zip(
            child
                .get("stable_history_end")
                .and_then(serde_json::Value::as_u64),
        )
        .is_some_and(|(source_end, child_end)| source_end <= child_end);
    stable_boundary_grew
        && source_history.len() <= child_history.len()
        && source_history
            .iter()
            .zip(child_history)
            .all(|(source_block, child_block)| source_block == child_block)
}

fn provider_view_is_complete(view: &serde_json::Value) -> bool {
    let nonempty_string = |field| {
        view.get(field)
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| !value.is_empty())
    };
    let block_ref = |value: &serde_json::Value| {
        value
            .get("content_hash")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|hash| {
                hash.len() == 71
                    && hash.starts_with("blake3:")
                    && hash[7..]
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            })
            && value
                .get("byte_len")
                .and_then(serde_json::Value::as_u64)
                .is_some()
    };
    let history_blocks = view
        .get("history_blocks")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|blocks| blocks.iter().all(block_ref));
    let latest_compaction_boundary_valid = view
        .get("latest_compaction_summary_end")
        .is_none_or(|boundary| boundary.is_null() || boundary.as_u64().is_some());
    let boundaries_valid = view.get("boundaries").is_none_or(|boundaries| {
        boundaries.as_array().is_some_and(|boundaries| {
            boundaries.iter().all(|boundary| {
                boundary
                    .get("section")
                    .and_then(serde_json::Value::as_str)
                    .is_some()
                    && boundary.get("message_end").is_none_or(|message_end| {
                        message_end.is_null() || message_end.as_u64().is_some()
                    })
            })
        })
    });

    [
        "provider",
        "model",
        "dialect",
        "serialization_version",
        "header_epoch",
        "cache_epoch",
        "compaction_epoch",
        "reasoning_retention",
        "trim_sentinel",
    ]
    .into_iter()
    .all(nonempty_string)
        && view
            .get("account_scope")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|scope| !scope.is_empty())
        && view
            .get("stable_history_end")
            .and_then(serde_json::Value::as_u64)
            .is_some()
        && view
            .get("current_user_start")
            .and_then(serde_json::Value::as_u64)
            .is_some()
        && view.get("system_block").is_some_and(block_ref)
        && view.get("tool_schema_block").is_some_and(block_ref)
        && history_blocks
        && latest_compaction_boundary_valid
        && boundaries_valid
}

fn load_fork_source_envelopes(
    connection: &Connection,
    session_id: &SessionId,
    fork_seq: u64,
    scopes: &HashMap<Option<BranchId>, u64>,
) -> StoreResult<Vec<RawEnvelope>> {
    let mut statement = connection
        .prepare_cached(
            "SELECT seq, envelope_json, event_id, committed_at_ms
             FROM events WHERE session_id = ?1 ORDER BY seq ASC",
        )
        .map_err(map_sqlite_error)?;
    let mut rows = statement
        .query([session_id.as_str()])
        .map_err(map_sqlite_error)?;
    let mut envelopes = Vec::new();
    while let Some(row) = rows.next().map_err(map_sqlite_error)? {
        let stored_seq: i64 = row.get(0).map_err(map_sqlite_error)?;
        let stored_event_id: String = row.get(2).map_err(map_sqlite_error)?;
        let stored_committed_at_ms: i64 = row.get(3).map_err(map_sqlite_error)?;
        let envelope = decode_envelope_column(row, 1).map_err(|error| {
            corrupt(format!(
                "invalid fork-source envelope for session {session_id}, seq {stored_seq}: {error}"
            ))
        })?;
        validate_stored_envelope(
            session_id,
            stored_seq,
            &stored_event_id,
            stored_committed_at_ms,
            &envelope,
        )?;
        if envelope.seq <= fork_seq
            && scopes
                .get(&envelope.branch_id)
                .is_some_and(|ceiling| envelope.seq <= *ceiling)
        {
            envelopes.push(envelope);
        }
    }
    Ok(envelopes)
}

fn remapped_fork_event_id(session_id: &SessionId, source_event_id: &EventId) -> EventId {
    let digest = blake3::hash(
        format!(
            "session-fork-event-v1\0{}\0{}",
            session_id.as_str(),
            source_event_id.as_str()
        )
        .as_bytes(),
    );
    EventId::new(format!("session-fork-{}", digest.to_hex()))
}

fn fork_boundary_event_id(session_id: &SessionId, coordinate: &str) -> EventId {
    let digest = blake3::hash(
        format!(
            "session-fork-boundary-v1\0{}\0{coordinate}",
            session_id.as_str()
        )
        .as_bytes(),
    );
    EventId::new(format!("session-fork-boundary-{}", digest.to_hex()))
}

fn fork_run_boundary_event_id(
    session_id: &SessionId,
    run_id: &RunId,
    agent_id: Option<&AgentId>,
) -> EventId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"session-fork-run-boundary-v2\0");
    for bytes in [session_id.as_str().as_bytes(), run_id.as_str().as_bytes()] {
        hasher.update(&u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_be_bytes());
        hasher.update(bytes);
    }
    match agent_id {
        None => {
            hasher.update(&[0]);
        }
        Some(agent_id) => {
            hasher.update(&[1]);
            let bytes = agent_id.as_str().as_bytes();
            hasher.update(&u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_be_bytes());
            hasher.update(bytes);
        }
    }
    EventId::new(format!(
        "session-fork-boundary-{}",
        hasher.finalize().to_hex()
    ))
}

fn insert_forked_envelope(connection: &Connection, envelope: &RawEnvelope) -> StoreResult<()> {
    let bytes = encode_envelope(envelope).map_err(|error| {
        store_error(
            ErrorCode::InvalidArgument,
            format!("cannot serialize forked envelope: {error}"),
            false,
        )
    })?;
    connection
        .execute(
            "INSERT INTO events(
                session_id, seq, envelope_json, event_id, committed_at_ms, payload_kind
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                envelope.session_id.as_str(),
                to_sqlite_integer(envelope.seq)?,
                bytes,
                envelope.event_id.as_str(),
                to_sqlite_integer(envelope.committed_at_ms)?,
                payload_kind(envelope),
            ],
        )
        .map_err(map_sqlite_error)?;
    Ok(())
}

fn append_fork_boundary_closures(
    connection: &Connection,
    command: &SessionForkCommand,
    now: u64,
    envelopes: &mut Vec<RawEnvelope>,
) -> StoreResult<()> {
    // Run activity is reduced per agent lane. A terminal observation in one
    // lane says nothing about another lane that happens to share the run id.
    let mut run_states = HashMap::<(RunId, Option<AgentId>), RunState>::new();
    for envelope in envelopes.iter() {
        if let Some(run_id) = envelope.run_id.clone()
            && let Ok(EventPayload::RunState(state)) =
                serde_json::from_value::<EventPayload>(envelope.payload.clone())
        {
            run_states.insert((run_id, envelope.agent_id.clone()), state);
        }
    }
    for ((run_id, agent_id), state) in run_states {
        if state.is_terminal() {
            continue;
        }
        let seq = u64::try_from(envelopes.len())
            .map_err(|_| corrupt("forked journal is too large"))?
            .checked_add(1)
            .ok_or_else(|| corrupt("forked journal sequence space is exhausted"))?;
        let envelope = EventEnvelope {
            schema_version: SCHEMA_VERSION,
            event_id: fork_run_boundary_event_id(&command.session_id, &run_id, agent_id.as_ref()),
            seq,
            session_id: command.session_id.clone(),
            branch_id: None,
            run_id: Some(run_id),
            agent_id,
            device_id: command.device_id.clone(),
            authority_epoch: 0,
            worker_generation: command.worker_generation,
            causation_id: None,
            correlation_id: None,
            committed_at_ms: now,
            render: RenderTargets {
                ui: true,
                durable: true,
                prompt: PromptRender::Omit,
            },
            payload: serde_json::to_value(EventPayload::RunState(RunState::Cancelled)).map_err(
                |error| {
                    store_error(
                        ErrorCode::InvalidArgument,
                        format!("cannot serialize fork run boundary: {error}"),
                        false,
                    )
                },
            )?,
        };
        insert_forked_envelope(connection, &envelope)?;
        envelopes.push(envelope);
    }
    let seq = u64::try_from(envelopes.len())
        .map_err(|_| corrupt("forked journal is too large"))?
        .checked_add(1)
        .ok_or_else(|| corrupt("forked journal sequence space is exhausted"))?;
    let idle = EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: fork_boundary_event_id(&command.session_id, "session-idle"),
        seq,
        session_id: command.session_id.clone(),
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: command.device_id.clone(),
        authority_epoch: 0,
        worker_generation: command.worker_generation,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: now,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: serde_json::to_value(EventPayload::SessionState(SessionState::Idle {
            interrupted: false,
        }))
        .map_err(|error| {
            store_error(
                ErrorCode::InvalidArgument,
                format!("cannot serialize fork session boundary: {error}"),
                false,
            )
        })?,
    };
    insert_forked_envelope(connection, &idle)?;
    envelopes.push(idle);
    Ok(())
}

fn validate_branch_fork(
    connection: &Connection,
    session_id: &SessionId,
    source_branch_id: Option<&BranchId>,
    fork_node_id: &NodeId,
    fork_seq: u64,
    owner_agent_id: Option<&AgentId>,
) -> StoreResult<()> {
    let scopes = branch_lineage_scopes(connection, session_id, source_branch_id)?;
    let mut statement = connection
        .prepare_cached(
            "SELECT seq, envelope_json FROM events
             WHERE session_id = ?1 ORDER BY seq ASC",
        )
        .map_err(map_sqlite_error)?;
    let mut rows = statement
        .query([session_id.as_str()])
        .map_err(map_sqlite_error)?;
    let mut nodes = HashMap::<NodeId, (TreeNode, u64, Option<RunId>, Option<BranchId>)>::new();
    let mut run_states = HashMap::<RunId, RunState>::new();
    let mut candidate = None;
    while let Some(row) = rows.next().map_err(map_sqlite_error)? {
        let seq = sql_u64(row.get(0).map_err(map_sqlite_error)?).map_err(map_sqlite_error)?;
        let envelope = decode_envelope_column(row, 1).map_err(|error| {
            corrupt(format!(
                "invalid envelope JSON for session {session_id}, seq {seq}: {error}"
            ))
        })?;
        let admitted = envelope.agent_id.as_ref() == owner_agent_id
            && scopes
                .get(&envelope.branch_id)
                .is_some_and(|ceiling| seq <= *ceiling);
        if !admitted {
            continue;
        }
        let Ok(payload) = serde_json::from_value::<EventPayload>(envelope.payload.clone()) else {
            continue;
        };
        match payload {
            EventPayload::NodeCommitted(node) => {
                if nodes
                    .insert(
                        node.node.clone(),
                        (
                            node.clone(),
                            seq,
                            envelope.run_id.clone(),
                            envelope.branch_id.clone(),
                        ),
                    )
                    .is_some()
                {
                    return Err(corrupt(format!(
                        "branch lineage contains duplicate node {}",
                        node.node
                    )));
                }
                if seq == fork_seq && node.node == *fork_node_id {
                    candidate = Some((node, envelope.run_id));
                }
            }
            EventPayload::RunState(state) => {
                if let Some(run_id) = envelope.run_id {
                    run_states.insert(run_id, state);
                }
            }
            _ => {}
        }
    }
    let (candidate_node, candidate_run) = candidate.ok_or_else(|| {
        store_error(
            ErrorCode::InvalidArgument,
            "fork node and sequence do not name one admitted source-lineage node",
            false,
        )
    })?;

    let head = if let Some(source_branch_id) = source_branch_id {
        let descriptor =
            branch_descriptor(connection, session_id, source_branch_id)?.ok_or_else(|| {
                store_error(
                    ErrorCode::InvalidArgument,
                    "source branch does not exist",
                    false,
                )
            })?;
        let record = nodes.get(&descriptor.head_node_id).ok_or_else(|| {
            corrupt(format!(
                "branch {} head node {} is outside its declared lineage",
                descriptor.branch_id, descriptor.head_node_id
            ))
        })?;
        if record.1 != descriptor.head_seq {
            return Err(corrupt(format!(
                "branch {} head node/sequence disagree",
                descriptor.branch_id
            )));
        }
        descriptor.head_node_id
    } else {
        nodes
            .values()
            .filter(|(_, _, _, branch_id)| branch_id.is_none())
            .max_by_key(|(_, seq, _, _)| *seq)
            .map(|(node, _, _, _)| node.node.clone())
            .ok_or_else(|| {
                store_error(
                    ErrorCode::InvalidArgument,
                    "legacy/main branch has no history node to fork",
                    false,
                )
            })?
    };

    let mut current = head;
    let mut seen = HashSet::new();
    let mut on_ancestry = false;
    loop {
        if !seen.insert(current.clone()) {
            return Err(corrupt(format!(
                "history tree contains a cycle at {current}"
            )));
        }
        let (node, seq, _, _) = nodes.get(&current).ok_or_else(|| {
            corrupt(format!(
                "history tree references missing lineage node {current}"
            ))
        })?;
        if node.node == *fork_node_id && *seq == fork_seq {
            on_ancestry = true;
            break;
        }
        let Some(parent) = node.parent.clone() else {
            break;
        };
        current = parent;
    }
    if !on_ancestry {
        return Err(store_error(
            ErrorCode::InvalidArgument,
            "fork coordinate is not on the source branch's declared ancestry",
            false,
        ));
    }

    let idle_compaction = matches!(
        candidate_node.kind,
        NodeKind::Compaction {
            resume_cause: haider_protocol::history::CompactionResume::ManualIdle,
            ..
        }
    );
    let terminal_turn = candidate_run
        .as_ref()
        .and_then(|run_id| run_states.get(run_id))
        .is_some_and(RunState::is_terminal);
    if !idle_compaction && !terminal_turn {
        return Err(store_error(
            ErrorCode::InvalidArgument,
            "branches may fork only from terminal turns or idle compaction nodes",
            false,
        ));
    }
    Ok(())
}

fn update_branch_heads(transaction: &Connection, envelopes: &[RawEnvelope]) -> StoreResult<()> {
    for envelope in envelopes {
        let Some(branch_id) = envelope.branch_id.as_ref() else {
            continue;
        };
        if envelope.agent_id.is_some() {
            continue;
        }
        let Ok(EventPayload::NodeCommitted(node)) =
            serde_json::from_value::<EventPayload>(envelope.payload.clone())
        else {
            continue;
        };
        let updated = transaction
            .execute(
                "UPDATE branches SET head_node_id = ?3, head_seq = ?4
                 WHERE session_id = ?1 AND branch_id = ?2",
                params![
                    envelope.session_id.as_str(),
                    branch_id.as_str(),
                    node.node.as_str(),
                    to_sqlite_integer(envelope.seq)?,
                ],
            )
            .map_err(map_sqlite_error)?;
        if updated != 1 {
            return Err(store_error(
                ErrorCode::InvalidArgument,
                format!("branch {branch_id} is not registered for node commit"),
                false,
            ));
        }
    }
    Ok(())
}

struct StoredDelegation {
    agent_id: String,
    child_session_id: String,
    child_run_id: String,
    parent_session_id: String,
    parent_run_id: String,
    parent_branch_id: Option<String>,
    call_id: String,
    tool_item_id: String,
    parent_agent_id: Option<String>,
    root_session_id: String,
    depth: i64,
    task: String,
    prompt: String,
    manifest_json: String,
    state: String,
    report_json: Option<String>,
}

fn delegation_select() -> &'static str {
    "SELECT agent_id, child_session_id, child_run_id, parent_session_id,
            parent_run_id, parent_branch_id, call_id, tool_item_id, parent_agent_id,
            root_session_id, depth, task, prompt, manifest_json, state,
            report_json
     FROM delegations"
}

fn stored_delegation(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredDelegation> {
    Ok(StoredDelegation {
        agent_id: row.get(0)?,
        child_session_id: row.get(1)?,
        child_run_id: row.get(2)?,
        parent_session_id: row.get(3)?,
        parent_run_id: row.get(4)?,
        parent_branch_id: row.get(5)?,
        call_id: row.get(6)?,
        tool_item_id: row.get(7)?,
        parent_agent_id: row.get(8)?,
        root_session_id: row.get(9)?,
        depth: row.get(10)?,
        task: row.get(11)?,
        prompt: row.get(12)?,
        manifest_json: row.get(13)?,
        state: row.get(14)?,
        report_json: row.get(15)?,
    })
}

fn decode_delegation(row: StoredDelegation) -> StoreResult<DelegationRecord> {
    let manifest = serde_json::from_str(&row.manifest_json)
        .map_err(|error| corrupt(format!("delegation manifest is corrupt: {error}")))?;
    let report = row
        .report_json
        .map(|json| {
            serde_json::from_str(&json)
                .map_err(|error| corrupt(format!("delegation report is corrupt: {error}")))
        })
        .transpose()?;
    let state = match row.state.as_str() {
        "spawned" => DelegationState::Spawned,
        "running" => DelegationState::Running,
        "reported" => DelegationState::Reported,
        "collected" => DelegationState::Collected,
        other => return Err(corrupt(format!("unknown delegation state `{other}`"))),
    };
    let depth = u32::try_from(row.depth)
        .map_err(|_| corrupt("delegation depth is negative or too large"))?;
    Ok(DelegationRecord {
        agent_id: AgentId::new(row.agent_id),
        child_session_id: SessionId::new(row.child_session_id),
        child_run_id: RunId::new(row.child_run_id),
        parent_session_id: SessionId::new(row.parent_session_id),
        parent_run_id: RunId::new(row.parent_run_id),
        parent_branch_id: row.parent_branch_id.map(BranchId::new),
        call_id: row.call_id,
        tool_item_id: ItemId::new(row.tool_item_id),
        parent_agent_id: row.parent_agent_id.map(AgentId::new),
        root_session_id: SessionId::new(row.root_session_id),
        depth,
        task: row.task,
        prompt: row.prompt,
        manifest,
        state,
        report,
    })
}

fn lookup_delegation_by_agent(
    connection: &Connection,
    agent: &AgentId,
) -> StoreResult<Option<DelegationRecord>> {
    let sql = format!("{} WHERE agent_id = ?1", delegation_select());
    connection
        .query_row(&sql, [agent.as_str()], stored_delegation)
        .optional()
        .map_err(map_sqlite_error)?
        .map(decode_delegation)
        .transpose()
}

fn lookup_delegation_by_child_session(
    connection: &Connection,
    session_id: &SessionId,
) -> StoreResult<Option<DelegationRecord>> {
    let sql = format!("{} WHERE child_session_id = ?1", delegation_select());
    connection
        .query_row(&sql, [session_id.as_str()], stored_delegation)
        .optional()
        .map_err(map_sqlite_error)?
        .map(decode_delegation)
        .transpose()
}

fn lookup_delegation_by_parent_call(
    connection: &Connection,
    session_id: &SessionId,
    run_id: &RunId,
    call_id: &str,
) -> StoreResult<Option<DelegationRecord>> {
    let sql = format!(
        "{} WHERE parent_session_id = ?1 AND parent_run_id = ?2 AND call_id = ?3",
        delegation_select()
    );
    connection
        .query_row(
            &sql,
            params![session_id.as_str(), run_id.as_str(), call_id],
            stored_delegation,
        )
        .optional()
        .map_err(map_sqlite_error)?
        .map(decode_delegation)
        .transpose()
}

fn delegations_for_parent_session_limited(
    connection: &Connection,
    session_id: &SessionId,
    limit: usize,
) -> StoreResult<Vec<DelegationRecord>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let sql = format!(
        "{} WHERE parent_session_id = ?1
         ORDER BY created_at_ms, call_id, agent_id LIMIT ?2",
        delegation_select()
    );
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let mut statement = connection.prepare_cached(&sql).map_err(map_sqlite_error)?;
    let rows = statement
        .query_map(params![session_id.as_str(), limit], stored_delegation)
        .map_err(map_sqlite_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(map_sqlite_error)?;
    rows.into_iter().map(decode_delegation).collect()
}

fn delegation_count_for_parent_session(
    connection: &Connection,
    session_id: &SessionId,
) -> StoreResult<u32> {
    let count = connection
        .query_row(
            "SELECT COUNT(*) FROM delegations WHERE parent_session_id = ?1",
            params![session_id.as_str()],
            |row| row.get::<_, i64>(0),
        )
        .map_err(map_sqlite_error)?;
    u32::try_from(count).map_err(|_| {
        corrupt(format!(
            "session {session_id} has more direct delegations than the fleet wire can represent"
        ))
    })
}

/// Crash-honest global live-set reduction used inside delegation admission.
///
/// Reported/collected rows are terminal bookkeeping truth (including launch
/// failures that never produced a child run). Spawned/running rows remain live
/// unless the exact durable child run has reached Done, Errored, or Cancelled.
/// A missing run head is live: it represents the crash window after the link
/// committed but before the first child turn was accepted.
fn live_delegation_count(connection: &Connection) -> StoreResult<u64> {
    let sql = format!(
        "{} WHERE state IN ('spawned', 'running')
         ORDER BY created_at_ms, agent_id",
        delegation_select()
    );
    let mut statement = connection.prepare_cached(&sql).map_err(map_sqlite_error)?;
    let rows = statement
        .query_map([], stored_delegation)
        .map_err(map_sqlite_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(map_sqlite_error)?;
    let mut live = 0_u64;
    for stored in rows {
        let record = decode_delegation(stored)?;
        let terminal =
            latest_run_state(connection, &record.child_session_id, &record.child_run_id)?
                .is_some_and(|(state, _, _)| state.is_terminal());
        if !terminal {
            live = live.saturating_add(1);
        }
    }
    Ok(live)
}

fn validate_delegation(record: &DelegationRecord) -> StoreResult<()> {
    record.manifest.placement.ensure_local()?;
    if record.depth == 0
        || record.task.trim().is_empty()
        || record.prompt.trim().is_empty()
        || record.call_id.is_empty()
        || record.manifest.agent != record.agent_id
    {
        return Err(store_error(
            ErrorCode::InvalidArgument,
            "delegation identity, task, prompt, call, and depth must be valid",
            false,
        ));
    }
    if record.state != DelegationState::Spawned || record.report.is_some() {
        return Err(store_error(
            ErrorCode::InvalidArgument,
            "new delegation must begin spawned without a report",
            false,
        ));
    }
    Ok(())
}

fn require_same_delegation_identity(
    existing: &DelegationRecord,
    requested: &DelegationRecord,
) -> StoreResult<()> {
    let same = existing.agent_id == requested.agent_id
        && existing.child_session_id == requested.child_session_id
        && existing.child_run_id == requested.child_run_id
        && existing.parent_session_id == requested.parent_session_id
        && existing.parent_run_id == requested.parent_run_id
        && existing.parent_branch_id == requested.parent_branch_id
        && existing.call_id == requested.call_id
        && existing.tool_item_id == requested.tool_item_id
        && existing.parent_agent_id == requested.parent_agent_id
        && existing.root_session_id == requested.root_session_id
        && existing.depth == requested.depth
        && existing.task == requested.task
        && existing.prompt == requested.prompt
        && existing.manifest == requested.manifest;
    if same {
        Ok(())
    } else {
        Err(store_error(
            ErrorCode::InvalidArgument,
            "delegation receipt was replayed with different semantics",
            false,
        ))
    }
}

fn delegation_state_name(state: DelegationState) -> &'static str {
    match state {
        DelegationState::Spawned => "spawned",
        DelegationState::Running => "running",
        DelegationState::Reported => "reported",
        DelegationState::Collected => "collected",
    }
}

/// Validates an append batch: non-empty and single-session.
/// Returns the batch's session and its length as a u64.
fn same_session_batch(envelopes: &[RawEnvelope]) -> StoreResult<(SessionId, u64)> {
    let Some(first) = envelopes.first() else {
        return Err(store_error(
            ErrorCode::InvalidArgument,
            "cannot append an empty envelope batch",
            false,
        ));
    };
    let session = first.session_id.clone();
    if envelopes
        .iter()
        .any(|envelope| envelope.session_id != session)
    {
        return Err(store_error(
            ErrorCode::InvalidArgument,
            "one append batch cannot span multiple sessions",
            false,
        ));
    }
    let batch_len = u64::try_from(envelopes.len()).map_err(|_| {
        store_error(
            ErrorCode::InvalidArgument,
            "envelope batch is too large",
            false,
        )
    })?;
    Ok((session, batch_len))
}

/// Opens the profile's long-lived journal connection with the required pragmas
/// (WAL, configured synchronous policy, foreign keys, busy timeout).
fn open_connection(path: &Path) -> StoreResult<Connection> {
    open_connection_with(path, configured_store_synchronous()?)
}

fn open_connection_with(path: &Path, synchronous: StoreSynchronous) -> StoreResult<Connection> {
    let connection = Connection::open(path).map_err(map_sqlite_error)?;
    connection
        .busy_timeout(BUSY_TIMEOUT)
        .map_err(map_sqlite_error)?;
    connection
        .pragma_update(None, "foreign_keys", true)
        .map_err(map_sqlite_error)?;
    connection
        .pragma_update(None, "journal_mode", "WAL")
        .map_err(map_sqlite_error)?;
    connection
        .pragma_update(None, "synchronous", synchronous.pragma_value())
        .map_err(map_sqlite_error)?;
    Ok(connection)
}

fn configured_store_synchronous() -> StoreResult<StoreSynchronous> {
    let Some(value) = std::env::var_os(STORE_SYNCHRONOUS_ENV) else {
        return Ok(DEFAULT_STORE_SYNCHRONOUS);
    };
    let value = value.into_string().map_err(|_| {
        store_error(
            ErrorCode::InvalidArgument,
            format!("{STORE_SYNCHRONOUS_ENV} must be UTF-8 and one of: normal, full"),
            false,
        )
    })?;
    parse_store_synchronous(&value)
}

fn parse_store_synchronous(value: &str) -> StoreResult<StoreSynchronous> {
    match value {
        "normal" => Ok(StoreSynchronous::Normal),
        "full" => Ok(StoreSynchronous::Full),
        _ => Err(store_error(
            ErrorCode::InvalidArgument,
            format!("{STORE_SYNCHRONOUS_ENV} must be one of: normal, full (got `{value}`)"),
            false,
        )),
    }
}

fn encode_envelope(envelope: &RawEnvelope) -> Result<Vec<u8>, rmp_serde::encode::Error> {
    rmp_serde::to_vec_named(envelope)
}

fn payload_kind(envelope: &RawEnvelope) -> &str {
    let kind = envelope
        .payload
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    if kind == "item"
        && envelope
            .payload
            .get("item")
            .and_then(|item| item.get("item"))
            .and_then(serde_json::Value::as_str)
            == Some("tool_call")
    {
        "item_tool_call"
    } else {
        kind
    }
}

#[derive(Debug)]
enum FilteredReadError {
    Decode,
    Store(HaiderError),
}

fn read_reducer_page_with_connection(
    connection: &Connection,
    session: &SessionId,
    since_seq: u64,
    limit: usize,
    byte_budget: usize,
    payload_kinds: &[&str],
) -> Result<Vec<RawEnvelope>, FilteredReadError> {
    let (sql, parameter_capacity) = reducer_page_sql(payload_kinds.len())?;
    let mut parameters = Vec::with_capacity(parameter_capacity);
    parameters.push(SqlValue::Text(session.as_str().to_owned()));
    parameters.push(SqlValue::Integer(
        to_sqlite_integer(since_seq).map_err(FilteredReadError::Store)?,
    ));
    parameters.extend(
        payload_kinds
            .iter()
            .map(|kind| SqlValue::Text((*kind).to_owned())),
    );
    parameters.push(SqlValue::Integer(i64::try_from(limit).unwrap_or(i64::MAX)));

    let mut statement = connection
        .prepare_cached(&sql)
        .map_err(map_sqlite_error)
        .map_err(FilteredReadError::Store)?;
    let mut rows = statement
        .query(rusqlite::params_from_iter(parameters.iter()))
        .map_err(map_sqlite_error)
        .map_err(FilteredReadError::Store)?;
    let mut envelopes = Vec::new();
    let mut spent = 0_usize;
    while let Some(row) = rows
        .next()
        .map_err(map_sqlite_error)
        .map_err(FilteredReadError::Store)?
    {
        let stored_seq: i64 = row
            .get(0)
            .map_err(map_sqlite_error)
            .map_err(FilteredReadError::Store)?;
        let stored_event_id: String = row
            .get(2)
            .map_err(map_sqlite_error)
            .map_err(FilteredReadError::Store)?;
        let stored_committed_at_ms: i64 = row
            .get(3)
            .map_err(map_sqlite_error)
            .map_err(FilteredReadError::Store)?;
        let envelope = decode_envelope_column(row, 1).map_err(|_| FilteredReadError::Decode)?;
        validate_stored_envelope(
            session,
            stored_seq,
            &stored_event_id,
            stored_committed_at_ms,
            &envelope,
        )
        .map_err(FilteredReadError::Store)?;
        let weight = envelope_weight_bytes(&envelope);
        if !envelopes.is_empty() && spent.saturating_add(weight) > byte_budget {
            break;
        }
        spent = spent.saturating_add(weight);
        envelopes.push(envelope);
        if spent >= byte_budget {
            break;
        }
    }
    Ok(envelopes)
}

fn journal_boundary_with_connection(
    connection: &Connection,
    session: &SessionId,
) -> StoreResult<Option<(u64, EventId)>> {
    connection
        .query_row(
            "SELECT seq, event_id FROM events
             WHERE session_id = ?1 ORDER BY seq DESC LIMIT 1",
            [session.as_str()],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(map_sqlite_error)?
        .map(|(seq, event_id)| {
            let seq = u64::try_from(seq)
                .ok()
                .filter(|seq| *seq > 0)
                .ok_or_else(|| corrupt("database contains an invalid event sequence"))?;
            Ok((seq, EventId::new(event_id)))
        })
        .transpose()
}

fn reducer_page_sql(payload_kind_count: usize) -> Result<(String, usize), FilteredReadError> {
    let parameter_capacity = payload_kind_count.checked_add(3).ok_or_else(|| {
        FilteredReadError::Store(store_error(
            ErrorCode::InvalidArgument,
            "reducer payload-kind filter is too large",
            false,
        ))
    })?;
    let kind_parameters = (0..payload_kind_count)
        .map(|index| format!("?{}", index + 3))
        .collect::<Vec<_>>()
        .join(", ");
    let limit_parameter = parameter_capacity;
    let sql = format!(
        "SELECT seq, envelope_json, event_id, committed_at_ms
         FROM events INDEXED BY events_payload_kind_session_seq
         WHERE (payload_kind IN ({kind_parameters}) OR payload_kind IS NULL)
           AND session_id = ?1 AND seq > ?2
         ORDER BY seq ASC
         LIMIT ?{limit_parameter}"
    );
    Ok((sql, parameter_capacity))
}

fn decode_payload<'de, T>(payload: &'de serde_json::Value) -> Result<T, serde_json::Error>
where
    T: Deserialize<'de>,
{
    T::deserialize(payload)
}

/// Decodes the authoritative event record according to its SQLite storage
/// class. Version-13-and-earlier rows are JSON `TEXT`; current rows are
/// MessagePack `BLOB`s.
fn decode_envelope_column(row: &rusqlite::Row<'_>, index: usize) -> Result<RawEnvelope, String> {
    let value = row
        .get_ref(index)
        .map_err(|error| format!("cannot read encoded envelope column: {error}"))?;
    match value {
        ValueRef::Text(bytes) => serde_json::from_slice(bytes)
            .map_err(|error| format!("legacy JSON decode failed: {error}")),
        ValueRef::Blob(bytes) => rmp_serde::from_slice(bytes)
            .map_err(|error| format!("MessagePack decode failed: {error}")),
        ValueRef::Null => Err("encoded envelope has SQLite NULL storage class".to_owned()),
        ValueRef::Integer(_) => Err("encoded envelope has SQLite INTEGER storage class".to_owned()),
        ValueRef::Real(_) => Err("encoded envelope has SQLite REAL storage class".to_owned()),
    }
}

/// Cross-checks the denormalized row columns against the fields embedded in
/// the encoded envelope. Any disagreement means the journal was tampered with or
/// corrupted, never a validation bug in the caller.
fn validate_stored_envelope(
    requested_session: &SessionId,
    stored_seq: i64,
    stored_event_id: &str,
    stored_committed_at_ms: i64,
    envelope: &RawEnvelope,
) -> StoreResult<()> {
    let seq = u64::try_from(stored_seq)
        .map_err(|_| corrupt("database contains a negative event sequence"))?;
    let committed_at_ms = u64::try_from(stored_committed_at_ms)
        .map_err(|_| corrupt("database contains a negative commit timestamp"))?;
    if envelope.session_id != *requested_session
        || envelope.seq != seq
        || envelope.event_id.as_str() != stored_event_id
        || envelope.committed_at_ms != committed_at_ms
    {
        return Err(corrupt(format!(
            "event row and envelope disagree for session {requested_session}, seq {stored_seq}"
        )));
    }
    Ok(())
}

/// Maps SQLite failure classes onto protocol error codes: busy/locked becomes
/// retryable `StoreLocked`, constraint violations become `InvalidArgument`
/// (the caller sent conflicting data, e.g. a duplicate event ID), and
/// corrupt-database classes become `StoreCorrupt`.
pub(crate) fn map_sqlite_error(error: SqliteError) -> HaiderError {
    match &error {
        SqliteError::SqliteFailure(inner, _)
            if matches!(
                inner.code,
                SqliteErrorCode::DatabaseBusy | SqliteErrorCode::DatabaseLocked
            ) =>
        {
            store_error(
                ErrorCode::StoreLocked,
                format!("SQLite journal is busy: {error}"),
                true,
            )
        }
        SqliteError::SqliteFailure(inner, _) if inner.code == SqliteErrorCode::DiskFull => {
            store_error(
                ErrorCode::StoreFull,
                format!(
                    "SQLite journal cannot be written because the profile disk is full: {error}"
                ),
                true,
            )
        }
        SqliteError::SqliteFailure(inner, _)
            if matches!(
                inner.code,
                SqliteErrorCode::ReadOnly
                    | SqliteErrorCode::PermissionDenied
                    | SqliteErrorCode::AuthorizationForStatementDenied
            ) =>
        {
            store_error(
                ErrorCode::StoreReadOnly,
                format!("SQLite journal is read-only: {error}"),
                true,
            )
        }
        SqliteError::SqliteFailure(inner, _)
            if matches!(
                inner.code,
                SqliteErrorCode::SystemIoFailure | SqliteErrorCode::CannotOpen
            ) =>
        {
            store_error(
                ErrorCode::StoreUnavailable,
                format!("SQLite journal is unavailable: {error}"),
                true,
            )
        }
        SqliteError::SqliteFailure(inner, _)
            if matches!(inner.code, SqliteErrorCode::ConstraintViolation) =>
        {
            store_error(
                ErrorCode::InvalidArgument,
                format!("event append violates a journal constraint: {error}"),
                false,
            )
        }
        SqliteError::SqliteFailure(inner, _)
            if matches!(
                inner.code,
                SqliteErrorCode::DatabaseCorrupt | SqliteErrorCode::NotADatabase
            ) =>
        {
            corrupt(format!("SQLite journal is corrupt: {error}"))
        }
        _ => store_error(
            ErrorCode::Internal,
            format!("SQLite journal operation failed: {error}"),
            false,
        ),
    }
}

/// C1 — template resolution: the built-in catalog first, then the Loom
/// registry. A registered pipe workflow is pinnable BY NAME exactly like a
/// catalog entry, everywhere templates resolve (pin, switch, child refs).
struct ResolvedGraphTemplate {
    template: haider_protocol::graph::GraphTemplateSpec,
    revision: u32,
}

fn resolve_graph_template_tx(
    transaction: &rusqlite::Transaction<'_>,
    name: &str,
) -> StoreResult<Option<ResolvedGraphTemplate>> {
    if let Some(template) = graph_template(name) {
        return Ok(Some(ResolvedGraphTemplate {
            revision: template.version,
            template,
        }));
    }
    // Current-by-name selection excludes archived rows. Already pinned runs
    // resolve their immutable revision through the retained-revision path.
    let record = transaction
        .query_row(
            "SELECT rev, record_json FROM loom_workflows
             WHERE id = ?1 AND archived = 0",
            [name],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(map_sqlite_error)?;
    record
        .map(|(revision, json)| {
            let workflow: LoomWorkflow = serde_json::from_str(&json)
                .map_err(|_| corrupt("loom workflow record is not decodable"))?;
            let revision = u32::try_from(revision)
                .map_err(|_| corrupt("loom workflow rev is out of range"))?;
            if workflow.rev != revision {
                return Err(corrupt("loom workflow row and record revisions differ"));
            }
            Ok(ResolvedGraphTemplate {
                template: workflow.template,
                revision,
            })
        })
        .transpose()
}

fn enqueue_typed_agent_install(
    transaction: &Transaction<'_>,
    contract: &TypedAgentContract,
    now: u64,
) -> StoreResult<Option<TypedAgentInstallJob>> {
    if contract.required_clis.is_empty() {
        return Ok(None);
    }
    let job_id = format!(
        "install:{}:{}:{}",
        contract.agent_type_id, contract.agent_type_rev, contract.agent_type_digest
    );
    let job = TypedAgentInstallJob::queued(job_id, contract, now)
        .map_err(typed_agent_install_validation_error)?;
    transaction
        .execute(
            "INSERT INTO loom_cli_install_jobs(
                 job_id, agent_type_id, agent_type_rev, agent_type_digest, state,
                 total, completed, current_cli, error, created_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, NULL, ?8, ?8)",
            params![
                job.job_id.as_str(),
                job.agent_type_id.as_str(),
                i64::from(job.agent_type_rev),
                job.agent_type_digest.as_str(),
                typed_agent_install_state_str(job.state),
                i64::from(job.progress.total),
                i64::from(job.progress.completed),
                to_sqlite_integer(now)?,
            ],
        )
        .map_err(map_sqlite_error)?;
    for (ordinal, required_cli) in contract.required_clis.iter().enumerate() {
        let ordinal = u16::try_from(ordinal).map_err(|_| {
            corrupt("typed-agent required CLI ordinal exceeds the validated contract bound")
        })?;
        let item = TypedAgentInstallItem {
            job_id: job.job_id.clone(),
            ordinal,
            required_cli: required_cli.clone(),
            state: TypedAgentInstallState::Queued,
            error: None,
            created_at_ms: now,
            updated_at_ms: now,
        };
        item.validate()
            .map_err(typed_agent_install_validation_error)?;
        transaction
            .execute(
                "INSERT INTO loom_cli_install_items(
                     job_id, ordinal, cli_program, state, error, created_at_ms, updated_at_ms)
                 VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?5)",
                params![
                    item.job_id.as_str(),
                    i64::from(item.ordinal),
                    item.required_cli.program.as_str(),
                    typed_agent_install_state_str(item.state),
                    to_sqlite_integer(now)?,
                ],
            )
            .map_err(map_sqlite_error)?;
    }
    insert_typed_agent_install_event(transaction, &job)?;
    Ok(Some(job))
}

fn insert_typed_agent_install_event(
    connection: &Connection,
    job: &TypedAgentInstallJob,
) -> StoreResult<()> {
    job.validate()
        .map_err(typed_agent_install_validation_error)?;
    let inserted = connection
        .execute(
            "INSERT INTO loom_cli_install_events(
                 job_id, agent_type_id, agent_type_rev, agent_type_digest,
                 state, total, completed, current_cli, error,
                 created_at_ms, updated_at_ms, cancelled)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                job.job_id.as_str(),
                job.agent_type_id.as_str(),
                i64::from(job.agent_type_rev),
                job.agent_type_digest.as_str(),
                typed_agent_install_state_str(job.state),
                i64::from(job.progress.total),
                i64::from(job.progress.completed),
                job.progress.current_cli.as_deref(),
                job.error.as_deref(),
                to_sqlite_integer(job.created_at_ms)?,
                to_sqlite_integer(job.updated_at_ms)?,
                job.cancelled,
            ],
        )
        .map_err(map_sqlite_error)?;
    if inserted != 1 {
        return Err(corrupt(format!(
            "typed-agent install event for `{}` affected no row",
            job.job_id
        )));
    }
    Ok(())
}

fn typed_agent_install_events_tx(
    connection: &Connection,
    job_id: &str,
    after_cursor: u64,
) -> StoreResult<Vec<TypedAgentInstallEvent>> {
    let limit = i64::try_from(TYPED_AGENT_INSTALL_WATCH_PAGE_MAX_EVENTS)
        .map_err(|_| corrupt("typed-agent install watch page bound exceeds SQLite INTEGER"))?;
    let mut statement = connection
        .prepare_cached(
            "SELECT cursor, job_id, agent_type_id, agent_type_rev,
                    agent_type_digest, state, total, completed, current_cli,
                    error, created_at_ms, updated_at_ms, cancelled
             FROM loom_cli_install_events
             WHERE job_id = ?1 AND cursor > ?2
             ORDER BY cursor
             LIMIT ?3",
        )
        .map_err(map_sqlite_error)?;
    let mut rows = statement
        .query(params![job_id, to_sqlite_integer(after_cursor)?, limit])
        .map_err(map_sqlite_error)?;
    let mut events = Vec::new();
    while let Some(row) = rows.next().map_err(map_sqlite_error)? {
        let cursor = u64::try_from(row.get::<_, i64>(0).map_err(map_sqlite_error)?)
            .map_err(|_| corrupt("typed-agent install event cursor is negative"))?;
        let agent_type_rev = u32::try_from(row.get::<_, i64>(3).map_err(map_sqlite_error)?)
            .map_err(|_| corrupt("typed-agent install event revision is out of range"))?;
        let total = u16::try_from(row.get::<_, i64>(6).map_err(map_sqlite_error)?)
            .map_err(|_| corrupt("typed-agent install event total is out of range"))?;
        let completed = u16::try_from(row.get::<_, i64>(7).map_err(map_sqlite_error)?)
            .map_err(|_| corrupt("typed-agent install event completion count is out of range"))?;
        let created_at_ms = u64::try_from(row.get::<_, i64>(10).map_err(map_sqlite_error)?)
            .map_err(|_| corrupt("typed-agent install event creation timestamp is negative"))?;
        let updated_at_ms = u64::try_from(row.get::<_, i64>(11).map_err(map_sqlite_error)?)
            .map_err(|_| corrupt("typed-agent install event update timestamp is negative"))?;
        let state: String = row.get(5).map_err(map_sqlite_error)?;
        let cancelled: bool = row.get(12).map_err(map_sqlite_error)?;
        let event = TypedAgentInstallEvent {
            cursor,
            job: TypedAgentInstallJob {
                job_id: row.get(1).map_err(map_sqlite_error)?,
                agent_type_id: row.get(2).map_err(map_sqlite_error)?,
                agent_type_rev,
                agent_type_digest: row.get(4).map_err(map_sqlite_error)?,
                state: typed_agent_install_state(&state, cancelled)?,
                cancelled,
                progress: TypedAgentInstallProgress {
                    total,
                    completed,
                    current_cli: row.get(8).map_err(map_sqlite_error)?,
                },
                error: row.get(9).map_err(map_sqlite_error)?,
                created_at_ms,
                updated_at_ms,
            },
        };
        event
            .validate()
            .map_err(typed_agent_install_validation_error)?;
        events.push(event);
    }
    Ok(events)
}

fn typed_agent_install_jobs_tx(
    connection: &Connection,
    job_id: Option<&str>,
    agent_type_id: Option<&str>,
) -> StoreResult<Vec<TypedAgentInstallJob>> {
    let mut statement = connection
        .prepare_cached(
            "SELECT job_id, agent_type_id, agent_type_rev, agent_type_digest,
                    state, total, completed, current_cli, error,
                    created_at_ms, updated_at_ms, cancelled
             FROM loom_cli_install_jobs
             WHERE (?1 IS NULL OR job_id = ?1)
               AND (?2 IS NULL OR agent_type_id = ?2)
             ORDER BY agent_type_id, agent_type_rev, job_id",
        )
        .map_err(map_sqlite_error)?;
    let mut rows = statement
        .query(params![job_id, agent_type_id])
        .map_err(map_sqlite_error)?;
    let mut jobs = Vec::new();
    while let Some(row) = rows.next().map_err(map_sqlite_error)? {
        jobs.push(typed_agent_install_job_row(row)?);
    }
    Ok(jobs)
}

/// Status is a reconnect surface, so its unfiltered history bound belongs in
/// SQL rather than after row hydration. Select the newest jobs first, then
/// restore the public stable type/revision order within that bounded window.
fn typed_agent_install_status_jobs_tx(
    connection: &Connection,
    job_id: Option<&str>,
    agent_type_id: Option<&str>,
) -> StoreResult<Vec<TypedAgentInstallJob>> {
    let mut statement = connection
        .prepare_cached(
            "SELECT job_id, agent_type_id, agent_type_rev, agent_type_digest,
                    state, total, completed, current_cli, error,
                    created_at_ms, updated_at_ms, cancelled
             FROM loom_cli_install_jobs
             WHERE (?1 IS NULL OR job_id = ?1)
               AND (?2 IS NULL OR agent_type_id = ?2)
             ORDER BY updated_at_ms DESC, agent_type_rev DESC, job_id DESC
             LIMIT ?3",
        )
        .map_err(map_sqlite_error)?;
    let limit = i64::try_from(TYPED_AGENT_INSTALL_STATUS_MAX_JOBS)
        .map_err(|_| corrupt("typed-agent install status job bound exceeds SQLite INTEGER"))?;
    let mut rows = statement
        .query(params![job_id, agent_type_id, limit])
        .map_err(map_sqlite_error)?;
    let mut jobs = Vec::new();
    while let Some(row) = rows.next().map_err(map_sqlite_error)? {
        jobs.push(typed_agent_install_job_row(row)?);
    }
    jobs.sort_by(|left, right| {
        (&left.agent_type_id, left.agent_type_rev, &left.job_id).cmp(&(
            &right.agent_type_id,
            right.agent_type_rev,
            &right.job_id,
        ))
    });
    Ok(jobs)
}

fn typed_agent_install_job_tx(
    connection: &Connection,
    job_id: &str,
) -> StoreResult<Option<TypedAgentInstallJob>> {
    let mut jobs = typed_agent_install_jobs_tx(connection, Some(job_id), None)?;
    if jobs.len() > 1 {
        return Err(corrupt(format!(
            "typed-agent install job id `{job_id}` is not unique"
        )));
    }
    Ok(jobs.pop())
}

fn typed_agent_install_items_tx(
    connection: &Connection,
    job_id: Option<&str>,
    agent_type_id: Option<&str>,
) -> StoreResult<Vec<TypedAgentInstallItem>> {
    let mut statement = connection
        .prepare_cached(
            "SELECT item.job_id, item.ordinal, item.cli_program, item.state,
                    item.error, item.created_at_ms, item.updated_at_ms
             FROM loom_cli_install_items AS item
             JOIN loom_cli_install_jobs AS job ON job.job_id = item.job_id
             WHERE (?1 IS NULL OR item.job_id = ?1)
               AND (?2 IS NULL OR job.agent_type_id = ?2)
             ORDER BY job.agent_type_id, job.agent_type_rev, item.ordinal",
        )
        .map_err(map_sqlite_error)?;
    let mut rows = statement
        .query(params![job_id, agent_type_id])
        .map_err(map_sqlite_error)?;
    let mut items = Vec::new();
    while let Some(row) = rows.next().map_err(map_sqlite_error)? {
        items.push(typed_agent_install_item_row(row)?);
    }
    Ok(items)
}

/// One bounded item query matching the status job window. Keeping the bound
/// in the subquery avoids both historical over-read and one query per job.
fn typed_agent_install_status_items_tx(
    connection: &Connection,
    job_id: Option<&str>,
    agent_type_id: Option<&str>,
) -> StoreResult<Vec<TypedAgentInstallItem>> {
    let mut statement = connection
        .prepare_cached(
            "SELECT item.job_id, item.ordinal, item.cli_program, item.state,
                    item.error, item.created_at_ms, item.updated_at_ms
             FROM loom_cli_install_items AS item
             JOIN loom_cli_install_jobs AS job ON job.job_id = item.job_id
             WHERE item.job_id IN (
                 SELECT retained.job_id
                 FROM loom_cli_install_jobs AS retained
                 WHERE (?1 IS NULL OR retained.job_id = ?1)
                   AND (?2 IS NULL OR retained.agent_type_id = ?2)
                 ORDER BY retained.updated_at_ms DESC,
                          retained.agent_type_rev DESC, retained.job_id DESC
                 LIMIT ?3
             )
             ORDER BY job.agent_type_id, job.agent_type_rev, item.ordinal",
        )
        .map_err(map_sqlite_error)?;
    let limit = i64::try_from(TYPED_AGENT_INSTALL_STATUS_MAX_JOBS)
        .map_err(|_| corrupt("typed-agent install status job bound exceeds SQLite INTEGER"))?;
    let mut rows = statement
        .query(params![job_id, agent_type_id, limit])
        .map_err(map_sqlite_error)?;
    let mut items = Vec::new();
    while let Some(row) = rows.next().map_err(map_sqlite_error)? {
        items.push(typed_agent_install_item_row(row)?);
    }
    Ok(items)
}

fn typed_agent_install_item_tx(
    connection: &Connection,
    job_id: &str,
    ordinal: u16,
) -> StoreResult<Option<TypedAgentInstallItem>> {
    let items = typed_agent_install_items_tx(connection, Some(job_id), None)?;
    Ok(items.into_iter().find(|item| item.ordinal == ordinal))
}

/// Cross-row invariant for the durable installer state machine. Job and item
/// validators intentionally remain reusable value validators; this store
/// boundary additionally proves that aggregate progress describes the exact
/// item snapshot committed in the same transaction.
fn validate_typed_agent_install_aggregate(
    connection: &Connection,
    actual_job: &TypedAgentInstallJob,
    next_job: &TypedAgentInstallJob,
    item_update: Option<&TypedAgentInstallItemCas>,
) -> StoreResult<()> {
    let actual_items = typed_agent_install_items_tx(connection, Some(&actual_job.job_id), None)?;
    validate_typed_agent_install_snapshot(actual_job, &actual_items).map_err(|message| {
        corrupt(format!(
            "typed-agent install job `{}` has inconsistent durable rows: {message}",
            actual_job.job_id
        ))
    })?;

    let mut next_items = actual_items;
    if let Some(update) = item_update {
        let item = next_items
            .iter_mut()
            .find(|item| item.ordinal == update.expected.ordinal)
            .ok_or_else(|| {
                corrupt(format!(
                    "typed-agent install item `{}:{}` disappeared during aggregate validation",
                    update.expected.job_id, update.expected.ordinal
                ))
            })?;
        *item = update.next.clone();
    }
    validate_typed_agent_install_snapshot(next_job, &next_items).map_err(|message| {
        store_error(
            ErrorCode::InvalidArgument,
            format!(
                "typed-agent install update for `{}` is inconsistent: {message}",
                next_job.job_id
            ),
            false,
        )
    })?;

    if let Some(update) = item_update
        && update.next.state == TypedAgentInstallState::Failed
        && (next_job.state != TypedAgentInstallState::Failed || next_job.error != update.next.error)
    {
        return Err(store_error(
            ErrorCode::InvalidArgument,
            "a failed typed-agent install item and its job must fail atomically with the same error",
            false,
        ));
    }
    Ok(())
}

fn validate_typed_agent_install_snapshot(
    job: &TypedAgentInstallJob,
    items: &[TypedAgentInstallItem],
) -> Result<(), &'static str> {
    if items.len() != usize::from(job.progress.total) {
        return Err("item count does not match job total");
    }
    if items
        .iter()
        .enumerate()
        .any(|(ordinal, item)| usize::from(item.ordinal) != ordinal || item.job_id != job.job_id)
    {
        return Err("item identity or ordinal does not match its job");
    }
    let succeeded = items
        .iter()
        .filter(|item| item.state == TypedAgentInstallState::Succeeded)
        .count();
    if succeeded != usize::from(job.progress.completed) {
        return Err("completed count does not equal succeeded item count");
    }
    match job.state {
        TypedAgentInstallState::Queued => {
            if items
                .iter()
                .any(|item| item.state != TypedAgentInstallState::Queued)
            {
                return Err("queued job contains a non-queued item");
            }
        }
        TypedAgentInstallState::Installing => {
            let Some(current) = job.progress.current_cli.as_deref() else {
                return Err("installing job has no current CLI");
            };
            let Some(item) = items
                .iter()
                .find(|item| item.required_cli.program == current)
            else {
                return Err("current CLI has no durable item");
            };
            if matches!(
                item.state,
                TypedAgentInstallState::Succeeded | TypedAgentInstallState::Failed
            ) {
                return Err("installing job points at a terminal item");
            }
        }
        TypedAgentInstallState::Verifying | TypedAgentInstallState::Succeeded => {
            if items
                .iter()
                .any(|item| item.state != TypedAgentInstallState::Succeeded)
            {
                return Err("verifying/succeeded job contains an incomplete item");
            }
        }
        TypedAgentInstallState::Failed => {}
    }
    Ok(())
}

fn typed_agent_install_job_row(row: &rusqlite::Row<'_>) -> StoreResult<TypedAgentInstallJob> {
    let agent_type_rev = u32::try_from(row.get::<_, i64>(2).map_err(map_sqlite_error)?)
        .map_err(|_| corrupt("typed-agent install job revision is out of range"))?;
    let total = u16::try_from(row.get::<_, i64>(5).map_err(map_sqlite_error)?)
        .map_err(|_| corrupt("typed-agent install job total is out of range"))?;
    let completed = u16::try_from(row.get::<_, i64>(6).map_err(map_sqlite_error)?)
        .map_err(|_| corrupt("typed-agent install job completion count is out of range"))?;
    let created_at_ms = u64::try_from(row.get::<_, i64>(9).map_err(map_sqlite_error)?)
        .map_err(|_| corrupt("typed-agent install job creation timestamp is negative"))?;
    let updated_at_ms = u64::try_from(row.get::<_, i64>(10).map_err(map_sqlite_error)?)
        .map_err(|_| corrupt("typed-agent install job update timestamp is negative"))?;
    let state: String = row.get(4).map_err(map_sqlite_error)?;
    let cancelled: bool = row.get(11).map_err(map_sqlite_error)?;
    let job = TypedAgentInstallJob {
        job_id: row.get(0).map_err(map_sqlite_error)?,
        agent_type_id: row.get(1).map_err(map_sqlite_error)?,
        agent_type_rev,
        agent_type_digest: row.get(3).map_err(map_sqlite_error)?,
        state: typed_agent_install_state(&state, cancelled)?,
        cancelled,
        progress: TypedAgentInstallProgress {
            total,
            completed,
            current_cli: row.get(7).map_err(map_sqlite_error)?,
        },
        error: row.get(8).map_err(map_sqlite_error)?,
        created_at_ms,
        updated_at_ms,
    };
    job.validate().map_err(|error| {
        corrupt(format!(
            "typed-agent install job `{}` is invalid: {error}",
            job.job_id
        ))
    })?;
    Ok(job)
}

fn typed_agent_install_item_row(row: &rusqlite::Row<'_>) -> StoreResult<TypedAgentInstallItem> {
    let ordinal = u16::try_from(row.get::<_, i64>(1).map_err(map_sqlite_error)?)
        .map_err(|_| corrupt("typed-agent install item ordinal is out of range"))?;
    let created_at_ms = u64::try_from(row.get::<_, i64>(5).map_err(map_sqlite_error)?)
        .map_err(|_| corrupt("typed-agent install item creation timestamp is negative"))?;
    let updated_at_ms = u64::try_from(row.get::<_, i64>(6).map_err(map_sqlite_error)?)
        .map_err(|_| corrupt("typed-agent install item update timestamp is negative"))?;
    let state: String = row.get(3).map_err(map_sqlite_error)?;
    let item = TypedAgentInstallItem {
        job_id: row.get(0).map_err(map_sqlite_error)?,
        ordinal,
        required_cli: TypedAgentRequiredCli {
            program: row.get(2).map_err(map_sqlite_error)?,
        },
        state: typed_agent_install_state(&state, false)?,
        error: row.get(4).map_err(map_sqlite_error)?,
        created_at_ms,
        updated_at_ms,
    };
    item.validate().map_err(|error| {
        corrupt(format!(
            "typed-agent install item `{}:{}` is invalid: {error}",
            item.job_id, item.ordinal
        ))
    })?;
    Ok(item)
}

fn typed_agent_install_state(value: &str, cancelled: bool) -> StoreResult<TypedAgentInstallState> {
    if cancelled {
        if value != "failed" {
            return Err(corrupt(
                "cancelled typed-agent install row is not stored as failed",
            ));
        }
        return Ok(TypedAgentInstallState::Failed);
    }
    match value {
        "queued" => Ok(TypedAgentInstallState::Queued),
        "installing" => Ok(TypedAgentInstallState::Installing),
        "verifying" => Ok(TypedAgentInstallState::Verifying),
        "succeeded" => Ok(TypedAgentInstallState::Succeeded),
        "failed" => Ok(TypedAgentInstallState::Failed),
        _ => Err(corrupt(format!(
            "typed-agent install state `{value}` is unknown"
        ))),
    }
}

const fn typed_agent_install_state_str(state: TypedAgentInstallState) -> &'static str {
    match state {
        TypedAgentInstallState::Queued => "queued",
        TypedAgentInstallState::Installing => "installing",
        TypedAgentInstallState::Verifying => "verifying",
        TypedAgentInstallState::Succeeded => "succeeded",
        TypedAgentInstallState::Failed => "failed",
    }
}

fn typed_agent_install_validation_error(error: TypedAgentContractError) -> HaiderError {
    store_error(
        ErrorCode::InvalidArgument,
        format!("invalid typed-agent install state: {error}"),
        false,
    )
}

fn typed_agent_install_conflict(message: impl Into<String>) -> HaiderError {
    store_error(ErrorCode::RevisionConflict, message, false)
}

fn loom_expectation_matches(
    expected: &LoomRevisionExpectation,
    current: Option<(u32, &str)>,
) -> bool {
    match current {
        None => expected.rev == 0 && expected.digest.is_none(),
        Some((rev, digest)) => {
            expected.rev == rev
                && expected
                    .digest
                    .as_deref()
                    .is_none_or(|expected_digest| expected_digest == digest)
        }
    }
}

fn loom_revision_conflict(
    expected: &LoomRevisionExpectation,
    current: Option<(u32, &str)>,
) -> LoomRevisionConflict {
    LoomRevisionConflict {
        expected: expected.clone(),
        current_rev: current.map(|(rev, _)| rev),
        current_digest: current.map(|(_, digest)| digest.to_owned()),
    }
}

fn validate_loom_registry_id(id: &str) -> StoreResult<()> {
    let valid = !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
    if valid {
        Ok(())
    } else {
        Err(store_error(
            ErrorCode::InvalidArgument,
            "Loom registry id must be a 1..=64 byte identifier",
            false,
        ))
    }
}

fn loom_registry_entry_kind_str(kind: LoomRegistryEntryKind) -> StoreResult<&'static str> {
    match kind {
        LoomRegistryEntryKind::AgentType => Ok("agent_type"),
        LoomRegistryEntryKind::Workflow => Ok("workflow"),
        _ => Err(store_error(
            ErrorCode::InvalidArgument,
            "unknown Loom registry entry kind",
            false,
        )),
    }
}

fn loom_registry_entry_kind(value: &str) -> StoreResult<LoomRegistryEntryKind> {
    match value {
        "agent_type" => Ok(LoomRegistryEntryKind::AgentType),
        "workflow" => Ok(LoomRegistryEntryKind::Workflow),
        _ => Err(corrupt(format!(
            "Loom registry event entry kind `{value}` is unknown"
        ))),
    }
}

fn loom_registry_delta_kind_str(kind: LoomRegistryDeltaKind) -> StoreResult<&'static str> {
    match kind {
        LoomRegistryDeltaKind::Upserted => Ok("upserted"),
        LoomRegistryDeltaKind::Archived => Ok("archived"),
        LoomRegistryDeltaKind::Unarchived => Ok("unarchived"),
        LoomRegistryDeltaKind::RevisionAdded => Ok("revision_added"),
        _ => Err(store_error(
            ErrorCode::InvalidArgument,
            "unknown Loom registry delta kind",
            false,
        )),
    }
}

fn loom_registry_delta_kind(value: &str) -> StoreResult<LoomRegistryDeltaKind> {
    match value {
        "upserted" => Ok(LoomRegistryDeltaKind::Upserted),
        "archived" => Ok(LoomRegistryDeltaKind::Archived),
        "unarchived" => Ok(LoomRegistryDeltaKind::Unarchived),
        "revision_added" => Ok(LoomRegistryDeltaKind::RevisionAdded),
        _ => Err(corrupt(format!(
            "Loom registry event change kind `{value}` is unknown"
        ))),
    }
}

fn insert_loom_registry_event(
    connection: &Connection,
    change: LoomRegistryDeltaKind,
    entry: &LoomRegistryEntryRef,
    now: u64,
) -> StoreResult<u64> {
    let inserted = connection
        .execute(
            "INSERT INTO loom_registry_events(
                 entry_kind, entry_id, change_kind, rev, digest, archived, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                loom_registry_entry_kind_str(entry.kind)?,
                entry.id.as_str(),
                loom_registry_delta_kind_str(change)?,
                i64::from(entry.rev),
                entry.digest.as_str(),
                entry.archived,
                to_sqlite_integer(now)?,
            ],
        )
        .map_err(map_sqlite_error)?;
    if inserted != 1 {
        return Err(corrupt("Loom registry event insert affected no row"));
    }
    u64::try_from(connection.last_insert_rowid())
        .map_err(|_| corrupt("Loom registry event cursor is negative"))
}

fn insert_loom_registry_upsert_events(
    connection: &Connection,
    kind: LoomRegistryEntryKind,
    registration: &LoomRegistration,
    archived: bool,
    now: u64,
) -> StoreResult<Option<u64>> {
    if !registration.updated {
        return Ok(None);
    }
    let entry = LoomRegistryEntryRef {
        kind,
        id: registration.id.clone(),
        rev: registration.rev,
        digest: registration.digest.clone(),
        archived,
    };
    insert_loom_registry_event(connection, LoomRegistryDeltaKind::Upserted, &entry, now)?;
    insert_loom_registry_event(
        connection,
        LoomRegistryDeltaKind::RevisionAdded,
        &entry,
        now,
    )
    .map(Some)
}

fn loom_registry_head_tx(connection: &Connection) -> StoreResult<u64> {
    connection
        .query_row(
            "SELECT COALESCE(MAX(cursor), 0) FROM loom_registry_events",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map_err(map_sqlite_error)
        .and_then(|cursor| {
            u64::try_from(cursor).map_err(|_| corrupt("Loom registry head cursor is negative"))
        })
}

fn loom_agent_type_rows_tx(
    connection: &Connection,
    include_archived: bool,
) -> StoreResult<Vec<(LoomAgentType, bool)>> {
    let mut statement = connection
        .prepare(
            "SELECT record_json, archived FROM loom_agent_types
             WHERE (?1 OR archived = 0) ORDER BY id",
        )
        .map_err(map_sqlite_error)?;
    let mut rows = statement
        .query([include_archived])
        .map_err(map_sqlite_error)?;
    let mut records = Vec::new();
    while let Some(row) = rows.next().map_err(map_sqlite_error)? {
        let json: String = row.get(0).map_err(map_sqlite_error)?;
        let record = serde_json::from_str(&json)
            .map_err(|_| corrupt("loom agent type record is not decodable"))?;
        records.push((record, row.get(1).map_err(map_sqlite_error)?));
    }
    Ok(records)
}

fn loom_workflow_rows_tx(
    connection: &Connection,
    include_archived: bool,
) -> StoreResult<Vec<(LoomWorkflow, bool)>> {
    let mut statement = connection
        .prepare(
            "SELECT record_json, archived FROM loom_workflows
             WHERE (?1 OR archived = 0) ORDER BY id",
        )
        .map_err(map_sqlite_error)?;
    let mut rows = statement
        .query([include_archived])
        .map_err(map_sqlite_error)?;
    let mut records = Vec::new();
    while let Some(row) = rows.next().map_err(map_sqlite_error)? {
        let json: String = row.get(0).map_err(map_sqlite_error)?;
        let record = serde_json::from_str(&json)
            .map_err(|_| corrupt("loom workflow record is not decodable"))?;
        records.push((record, row.get(1).map_err(map_sqlite_error)?));
    }
    Ok(records)
}

fn loom_agent_types_tx(
    connection: &Connection,
    include_archived: bool,
) -> StoreResult<Vec<LoomAgentType>> {
    loom_agent_type_rows_tx(connection, include_archived)
        .map(|rows| rows.into_iter().map(|(record, _)| record).collect())
}

fn loom_workflows_tx(
    connection: &Connection,
    include_archived: bool,
) -> StoreResult<Vec<LoomWorkflow>> {
    loom_workflow_rows_tx(connection, include_archived)
        .map(|rows| rows.into_iter().map(|(record, _)| record).collect())
}

fn loom_archived_entries_tx(connection: &Connection) -> StoreResult<Vec<LoomRegistryEntryRef>> {
    let mut entries = Vec::new();
    for (kind, table) in [
        (LoomRegistryEntryKind::AgentType, "loom_agent_types"),
        (LoomRegistryEntryKind::Workflow, "loom_workflows"),
    ] {
        let sql = format!("SELECT id, rev, digest FROM {table} WHERE archived = 1 ORDER BY id");
        let mut statement = connection.prepare(&sql).map_err(map_sqlite_error)?;
        let mut rows = statement.query([]).map_err(map_sqlite_error)?;
        while let Some(row) = rows.next().map_err(map_sqlite_error)? {
            entries.push(LoomRegistryEntryRef {
                kind,
                id: row.get(0).map_err(map_sqlite_error)?,
                rev: u32::try_from(row.get::<_, i64>(1).map_err(map_sqlite_error)?)
                    .map_err(|_| corrupt("archived Loom revision is out of range"))?,
                digest: row.get(2).map_err(map_sqlite_error)?,
                archived: true,
            });
        }
    }
    Ok(entries)
}

fn loom_registry_record_tx(
    store_root: &Path,
    connection: &Connection,
    kind: LoomRegistryEntryKind,
    id: &str,
    rev: u32,
    digest: &str,
) -> StoreResult<LoomRegistryRecord> {
    match kind {
        LoomRegistryEntryKind::AgentType => {
            validate_loom_revision_address(id, rev, digest)?;
            let path = store_root
                .join(LOOM_AGENT_REVISIONS_DIR)
                .join(id)
                .join(format!("{rev}-{digest}.json"));
            let bytes =
                fs::read(&path).map_err(|error| loom_revision_io_error("read", &path, error))?;
            let record: LoomAgentType = serde_json::from_slice(&bytes)
                .map_err(|_| corrupt("retained Loom agent-type revision is not decodable"))?;
            if record.id != id || record.rev != rev || record.digest() != digest {
                return Err(corrupt(
                    "retained Loom agent-type revision does not match registry replay address",
                ));
            }
            Ok(LoomRegistryRecord::AgentType(record))
        }
        LoomRegistryEntryKind::Workflow => {
            let json = connection
                .query_row(
                    "SELECT record_json FROM loom_workflow_revisions
                     WHERE id = ?1 AND rev = ?2 AND digest = ?3",
                    params![id, i64::from(rev), digest],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(map_sqlite_error)?
                .ok_or_else(|| corrupt("Loom workflow revision for registry replay is missing"))?;
            let record: LoomWorkflow = serde_json::from_str(&json)
                .map_err(|_| corrupt("Loom workflow revision is not decodable"))?;
            Ok(LoomRegistryRecord::Workflow(record))
        }
        _ => Err(corrupt(
            "unknown Loom registry entry kind reached durable replay",
        )),
    }
}

fn validate_loom_revision_address(id: &str, rev: u32, digest: &str) -> StoreResult<()> {
    let valid_id = !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'));
    let valid_digest = digest.len() == 32
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    if valid_id && rev > 0 && valid_digest {
        Ok(())
    } else {
        Err(store_error(
            ErrorCode::InvalidArgument,
            "invalid Loom revision address",
            false,
        ))
    }
}

fn verify_retained_loom_agent_type(path: &Path, expected: &LoomAgentType) -> StoreResult<()> {
    let bytes = fs::read(path)
        .map_err(|error| loom_revision_io_error("read retained revision", path, error))?;
    let retained: LoomAgentType = serde_json::from_slice(&bytes)
        .map_err(|_| corrupt("retained Loom agent-type revision is not decodable"))?;
    if retained == *expected {
        Ok(())
    } else {
        Err(corrupt(
            "retained Loom agent-type digest addresses different content",
        ))
    }
}

fn sync_loom_revision_directory(path: &Path) -> StoreResult<()> {
    haider_platform::sync_directory(path)
        .map_err(|error| loom_revision_io_error("sync directory", path, error))
}

fn loom_revision_io_error(action: &str, path: &Path, error: std::io::Error) -> HaiderError {
    let code = match error.kind() {
        ErrorKind::StorageFull => ErrorCode::StoreFull,
        ErrorKind::PermissionDenied | ErrorKind::ReadOnlyFilesystem => ErrorCode::StoreReadOnly,
        _ => ErrorCode::Internal,
    };
    store_error(
        code,
        format!("cannot {action} {}: {error}", path.display()),
        false,
    )
}

/// Round 3 — canonical form BEFORE validation and digesting: typed I/O is
/// trimmed, API hosts are lowercased. Comparisons downstream (grant hosts,
/// tail joins) then never fight case or stray whitespace.
fn normalize_agent_type(record: &LoomAgentType) -> LoomAgentType {
    let mut record = record.clone();
    record.in_type = record.in_type.trim().to_owned();
    record.out_type = record.out_type.trim().to_owned();
    for api in &mut record.apis {
        *api = api.trim().to_ascii_lowercase();
    }
    for cli in &mut record.clis {
        *cli = cli.trim().to_owned();
    }
    for denial in &mut record.denials {
        *denial = denial.trim().to_owned();
        if let Some(host) = denial.strip_prefix("api:") {
            *denial = format!("api:{}", host.to_ascii_lowercase());
        }
    }
    record
}

/// B1 registration bounds: identifiers, non-empty semantics, bounded lists.
fn validate_agent_type(record: &LoomAgentType) -> StoreResult<()> {
    let ident = |value: &str| {
        !value.is_empty()
            && value.len() <= 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    };
    let reject = |message: &str| {
        Err(HaiderError::new(
            ErrorCode::InvalidArgument,
            message.to_string(),
            false,
        ))
    };
    if !ident(&record.id) {
        return reject("agent type id must be a bounded identifier");
    }
    if record.name.trim().is_empty() || record.name.len() > 120 {
        return reject("agent type name must be 1..=120 bytes");
    }
    if record.job.trim().is_empty() || record.job.len() > 4096 {
        return reject("agent type job must be 1..=4096 bytes");
    }
    if !haider_protocol::loom::valid_type_expr(record.in_type.trim())
        || !haider_protocol::loom::valid_type_expr(record.out_type.trim())
    {
        return reject("agent type I/O must be bounded type expressions (`A` or `A + B`)");
    }
    // Display fields reach terminal cells verbatim — bound them here, never
    // in the renderer. Color is empty or exactly `#rrggbb`; the glyph is a
    // short printable cluster.
    let color_ok = record.color.is_empty()
        || (record.color.len() == 7
            && record.color.starts_with('#')
            && record
                .color
                .bytes()
                .skip(1)
                .all(|byte| byte.is_ascii_hexdigit()));
    if !color_ok {
        return reject("agent type color must be empty or `#rrggbb`");
    }
    let invisible = |character: char| {
        character.is_control()
            || matches!(
                character,
                '\u{00AD}'
                    | '\u{034F}'
                    | '\u{061C}'
                    | '\u{180B}'..='\u{180F}'
                    | '\u{200B}'..='\u{200F}'
                    | '\u{202A}'..='\u{202E}'
                    | '\u{2028}'
                    | '\u{2029}'
                    | '\u{2060}'..='\u{206F}'
                    | '\u{FE00}'..='\u{FE0F}'
                    | '\u{FEFF}'
                    | '\u{E0100}'..='\u{E01EF}'
            )
    };
    // A glyph must LEAD with a visible base character — a combining-only
    // glyph renders as a mutation of whatever cell precedes it.
    let leads_combining = record.glyph.chars().next().is_some_and(|character| {
        matches!(
            character,
            '\u{0300}'..='\u{036F}' | '\u{1AB0}'..='\u{1AFF}' | '\u{20D0}'..='\u{20FF}'
        )
    });
    if record.glyph.len() > 16 || leads_combining || record.glyph.chars().any(invisible) {
        return reject(
            "agent type glyph must be ≤16 bytes, lead with a base character, and carry no \
             control/invisible/reordering characters",
        );
    }
    for list in [
        &record.clis,
        &record.apis,
        &record.denials,
        &record.skills,
        &record.scripts,
    ] {
        if list.len() > 32 {
            return reject("agent type capability lists are bounded to 32 entries");
        }
        if list.iter().any(|item| item.is_empty() || item.len() > 128) {
            return reject("agent type capability entries must be 1..=128 bytes");
        }
        if list
            .iter()
            .any(|item| item.chars().any(|character| character.is_control()))
        {
            return reject("agent type capability entries must not carry control characters");
        }
    }
    // Round 4 — a CLI grant is a PROGRAM NAME the exec fence compares by
    // exact first token, and that token then meets a SHELL: any byte the
    // shell can expand or re-quote ($VAR, quotes, backslashes, globs) turns
    // a declared literal into a different program. Allowlist, not denylist.
    let cli_byte_ok =
        |byte: u8| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'+' | b'/');
    if record
        .clis
        .iter()
        .any(|cli| cli.starts_with('-') || !cli.bytes().all(cli_byte_ok))
    {
        return reject(
            "agent type CLI entries must be bare program names or absolute paths \
             (alphanumeric plus . _ - + / only)",
        );
    }
    // Rounds 5-6 — the AUTHORITY MODEL, stated once: the CLI fence is a
    // tool-discipline guard, not a jail. Registration is HUMAN-GATED (the
    // plan gate / a Control-plane RPC), so the human who accepts a grant is
    // the security boundary; the fence's job is to make that review
    // MEANINGFUL — what was declared is what runs, and the obvious
    // network-scope bypass (curl et al. via chaining) is closed. Shell
    // BUILTINS and pure DISPATCHERS are denied because they make the
    // review meaningless (declaring `eval` or `busybox` reads as one tool
    // but grants everything). General interpreters (python, node, find)
    // remain declarable: they are legitimate leaf-specialist tools, their
    // names honestly convey their power, and the human approves them.
    const CLI_DISPATCHERS: [&str; 26] = [
        ".", "source", "eval", "exec", "command", "builtin", "env", "xargs", "sh", "bash", "zsh",
        "dash", "ksh", "csh", "tcsh", "fish", "nohup", "time", "nice", "sudo", "doas", "su",
        "setsid", "stdbuf", "busybox", "toybox",
    ];
    if record.clis.iter().any(|cli| {
        let base = cli.rsplit('/').next().unwrap_or(cli);
        CLI_DISPATCHERS.contains(&base) || !cli.bytes().any(|byte| byte.is_ascii_alphanumeric())
    }) {
        return reject(
            "agent type CLI entries must name a concrete program, never a shell \
             builtin/dispatcher (., source, eval, exec, env, xargs, a shell, ...)",
        );
    }
    // An API grant is a HOST the network fence compares literally — never a
    // URL, port, or credential-bearing form.
    if record
        .apis
        .iter()
        .any(|api| api.contains(['/', ':', '@', ' ', '\t']))
    {
        return reject("agent type API entries must be bare hosts (no scheme/port/path)");
    }
    let mut seen_denials = std::collections::HashSet::new();
    for denial in &record.denials {
        let denied_cli = denial.strip_prefix("cli:");
        let denied_api = denial.strip_prefix("api:");
        let valid = denied_cli.is_some_and(|value| {
            !value.is_empty() && value.len() <= 128 && value.bytes().all(cli_byte_ok)
        }) || denied_api.is_some_and(|value| {
            !value.is_empty()
                && value.len() <= 128
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
        });
        if !valid {
            return reject("agent type denials must be cli:<program> or api:<host> keys");
        }
        if !seen_denials.insert(denial) {
            return reject("agent type denials must be unique");
        }
        if denied_cli.is_some_and(|cli| record.clis.iter().any(|grant| grant == cli))
            || denied_api.is_some_and(|api| record.apis.iter().any(|grant| grant == api))
        {
            return reject("agent type capability cannot be both granted and denied");
        }
    }
    Ok(())
}

fn corrupt(message: impl Into<String>) -> HaiderError {
    store_error(ErrorCode::StoreCorrupt, message, false)
}

fn validate_profile_installation_id(value: &str) -> StoreResult<()> {
    let suffix = value.strip_prefix("dev-").ok_or_else(|| {
        corrupt("profile installation id does not begin with the required `dev-` prefix")
    })?;
    if suffix.len() != 32
        || !suffix
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(corrupt(
            "profile installation id is not 32 lowercase hexadecimal digits",
        ));
    }
    Ok(())
}

fn store_io_error(operation: &str, error: std::io::Error) -> HaiderError {
    store_error(
        ErrorCode::Internal,
        format!("cannot {operation}: {error}"),
        error.kind() == std::io::ErrorKind::Interrupted,
    )
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod reducer_filter_tests {
    use super::*;
    use crate::usage_ledger::reduce_journal_usage;

    #[derive(Debug, PartialEq)]
    struct ReducerOutputs {
        run_states: HashMap<RunId, DurableRunHead>,
        queue_revision: u64,
        queue: Vec<(QueueRow, RunId, Option<BranchId>, u64)>,
        ordinary_prompt_source: Option<(RunId, u64)>,
        prompt_source: Option<(RunId, u64)>,
        failed_turn: Option<(RunId, RunId, u64)>,
        tree_head: Option<NodeId>,
    }

    fn seed_session(store: &Store, session_id: &SessionId) {
        let connection = Connection::open(store.database_path()).expect("open test journal");
        connection
            .execute(
                "INSERT INTO sessions(id, created_at_ms, meta_json) VALUES (?1, 1, '{}')",
                [session_id.as_str()],
            )
            .expect("seed test session");
    }

    fn fact(
        store: &Store,
        session_id: &SessionId,
        event_id: impl Into<String>,
        run_id: Option<RunId>,
        payload: serde_json::Value,
    ) -> RawEnvelope {
        EventEnvelope {
            schema_version: SCHEMA_VERSION,
            event_id: EventId::new(event_id),
            seq: 0,
            session_id: session_id.clone(),
            branch_id: None,
            run_id,
            agent_id: None,
            device_id: DeviceId::new("reducer-filter-test"),
            authority_epoch: 0,
            worker_generation: store.worker_generation(),
            causation_id: None,
            correlation_id: None,
            committed_at_ms: 0,
            render: RenderTargets {
                ui: false,
                durable: true,
                prompt: PromptRender::Omit,
            },
            payload,
        }
    }

    fn typed_payload(payload: EventPayload) -> serde_json::Value {
        serde_json::to_value(payload).expect("serialize reducer fixture")
    }

    fn reducer_outputs(
        store: &Store,
        session_id: &SessionId,
        queued_run: &RunId,
        retry_run: &RunId,
    ) -> StoreResult<ReducerOutputs> {
        let connection = store.connection()?;
        let (queue_revision, queue) = queue_entries(&connection, session_id)?;
        Ok(ReducerOutputs {
            run_states: latest_run_states(&connection, session_id)?,
            queue_revision,
            queue: queue
                .into_iter()
                .map(|entry| (entry.row, entry.run_id, entry.branch_id, entry.accepted_seq))
                .collect(),
            ordinary_prompt_source: main_timeline_run_prompt_source(
                &connection,
                session_id,
                queued_run,
            )?,
            prompt_source: main_timeline_run_prompt_source(&connection, session_id, retry_run)?,
            failed_turn: latest_main_timeline_failed_turn(&connection, session_id)?,
            tree_head: latest_tree_head(&connection, session_id, None, None)?,
        })
    }

    /// MUTATION CHECK: remove any production reducer kind or its `NULL`
    /// fallback. The indexed result then differs from the same production
    /// reducer forced through an all-legacy (therefore full-scan) journal.
    #[test]
    fn store_reducers_match_unfiltered_outputs() {
        let root = tempfile::tempdir().expect("tempdir");
        let store = Store::open(root.path()).expect("open store");
        let session_id = SessionId::new("store-reducer-filter-equivalence");
        seed_session(&store, &session_id);
        let queued_run = RunId::new("queued-run");
        let retry_run = RunId::new("retry-run");
        let user_event_id = EventId::new("queued-user");
        let node_id = NodeId::new("latest-node");
        let mut events = vec![
            fact(
                &store,
                &session_id,
                "queued-state",
                Some(queued_run.clone()),
                typed_payload(EventPayload::RunState(RunState::Queued)),
            ),
            fact(
                &store,
                &session_id,
                user_event_id.as_str(),
                Some(queued_run.clone()),
                typed_payload(EventPayload::UserMessage {
                    text: "queued prompt".into(),
                    attachments: Vec::new(),
                    mode: DeliveryMode::Queue,
                }),
            ),
            fact(
                &store,
                &session_id,
                "queue-enqueued",
                None,
                typed_payload(EventPayload::QueueChanged(
                    haider_protocol::queue::QueueDelta {
                        revision: 0,
                        change: QueueChange::Enqueued {
                            row: QueueRow {
                                id: user_event_id,
                                text: "queued prompt".into(),
                                mode: DeliveryMode::Queue,
                                ordinal: 1,
                                created_at_ms: 0,
                            },
                        },
                    },
                )),
            ),
            fact(
                &store,
                &session_id,
                "manual-retry",
                Some(retry_run.clone()),
                RunRetryEventPayload::RunRetried {
                    failed_run_id: queued_run.clone(),
                    prompt_run_id: queued_run.clone(),
                    user_seq: 2,
                }
                .to_payload_value()
                .expect("serialize retry fact"),
            ),
            fact(
                &store,
                &session_id,
                "retry-failed",
                Some(retry_run.clone()),
                typed_payload(EventPayload::RunFailed {
                    code: ErrorCode::ProviderError,
                    message: "provider failed".into(),
                    retryable: false,
                    presentation: None,
                }),
            ),
            fact(
                &store,
                &session_id,
                "retry-errored",
                Some(retry_run.clone()),
                typed_payload(EventPayload::RunState(RunState::Errored)),
            ),
            fact(
                &store,
                &session_id,
                "latest-node-event",
                Some(retry_run.clone()),
                typed_payload(EventPayload::NodeCommitted(TreeNode {
                    node: node_id,
                    parent: None,
                    kind: NodeKind::UserTurn {
                        text: "node".into(),
                        attachments: Vec::new(),
                    },
                })),
            ),
            fact(
                &store,
                &session_id,
                "irrelevant-event",
                None,
                serde_json::json!({"type": "irrelevant"}),
            ),
        ];
        store.append(&mut events).expect("append reducer fixtures");

        let filtered = reducer_outputs(&store, &session_id, &queued_run, &retry_run)
            .expect("reduce indexed journal");
        let connection = Connection::open(store.database_path()).expect("open raw journal");
        connection
            .execute(
                "UPDATE events SET payload_kind = NULL WHERE session_id = ?1",
                [session_id.as_str()],
            )
            .expect("force unfiltered legacy scan");
        drop(connection);
        let unfiltered = reducer_outputs(&store, &session_id, &queued_run, &retry_run)
            .expect("reduce unfiltered journal");
        assert_eq!(filtered, unfiltered);
    }

    fn usage_payload(input: u64) -> serde_json::Value {
        serde_json::json!({
            "type": "usage",
            "input": input,
            "output": 1,
            "reasoning": 0,
            "cached": 0,
            "source": "provider_reported",
            "account": "usage-test-account"
        })
    }

    /// MUTATION CHECK: remove any usage reducer kind. Each kind independently
    /// changes the rollup, so the indexed result diverges from an all-legacy
    /// production reduction.
    #[test]
    fn usage_reducer_matches_unfiltered_output() {
        let root = tempfile::tempdir().expect("tempdir");
        let store = Store::open(root.path()).expect("open store");
        let session_id = SessionId::new("usage-reducer-filter-equivalence");
        seed_session(&store, &session_id);
        let fork = SessionForked {
            source_session_id: SessionId::new("usage-source"),
            source_branch_id: None,
            fork_node_id: NodeId::new("fork-node"),
            fork_seq: 1,
            mode: haider_protocol::session_fork::SessionForkMode::Fork,
            description: None,
            accepted_proposal_digest: None,
            omissions: Vec::new(),
            context_epoch: ForkContextEpoch::Fresh,
            inherited_cache_segment: None,
        };
        let manifest = AgentManifest {
            agent: AgentId::new("usage-child"),
            role: haider_protocol::agent::AgentRole::Subagent,
            task: "usage child".into(),
            callsign: None,
            model_profile: "test".into(),
            grant: haider_protocol::agent::Grant {
                tools: Vec::new(),
                effect_ceiling: Vec::new(),
            },
            budget_tokens: None,
            placement: haider_protocol::agent::Placement::Local,
            lease: haider_protocol::ids::LeaseId::new("usage-lease"),
            fencing_epoch: 1,
            attempt: 0,
            parent: None,
            coordinates: None,
            cli_scope: None,
        };
        let mut events = vec![
            fact(
                &store,
                &session_id,
                "inherited-usage",
                None,
                usage_payload(100),
            ),
            fact(
                &store,
                &session_id,
                "inherited-spawn",
                None,
                typed_payload(EventPayload::AgentSpawned(manifest.clone())),
            ),
            fact(
                &store,
                &session_id,
                "fork-boundary",
                None,
                fork.to_payload_value().expect("serialize fork fact"),
            ),
            fact(&store, &session_id, "child-usage", None, usage_payload(10)),
            fact(
                &store,
                &session_id,
                "child-spawned",
                None,
                typed_payload(EventPayload::AgentSpawned(manifest)),
            ),
            fact(
                &store,
                &session_id,
                "child-failed",
                None,
                typed_payload(EventPayload::RunFailed {
                    code: ErrorCode::ProviderError,
                    message: "failed".into(),
                    retryable: false,
                    presentation: None,
                }),
            ),
            fact(
                &store,
                &session_id,
                "usage-irrelevant",
                None,
                serde_json::json!({"type": "irrelevant"}),
            ),
        ];
        store.append(&mut events).expect("append usage fixtures");

        let filtered = reduce_journal_usage(
            &store
                .all_usage_reducer_envelopes()
                .expect("read indexed usage facts"),
        );
        let connection = Connection::open(store.database_path()).expect("open raw journal");
        connection
            .execute(
                "UPDATE events SET payload_kind = NULL WHERE session_id = ?1",
                [session_id.as_str()],
            )
            .expect("force unfiltered legacy scan");
        drop(connection);
        let unfiltered = reduce_journal_usage(
            &store
                .all_usage_reducer_envelopes()
                .expect("read unfiltered usage facts"),
        );
        assert_eq!(filtered, unfiltered);
    }

    /// MUTATION CHECK: drop the explicit payload-kind index selection. The
    /// real schema's `(session_id, seq)` primary key then wins and every row is
    /// visited despite the SQL predicate.
    #[test]
    fn reducer_page_query_is_pinned_to_payload_kind_index() {
        let root = tempfile::tempdir().expect("tempdir");
        let store = Store::open(root.path()).expect("open store");
        let (sql, _) = reducer_page_sql(4).expect("build reducer SQL");
        let explain = format!("EXPLAIN QUERY PLAN {sql}");
        let parameters = [
            SqlValue::Text("session".into()),
            SqlValue::Integer(0),
            SqlValue::Text("effect".into()),
            SqlValue::Text("menu_opened".into()),
            SqlValue::Text("menu_answered".into()),
            SqlValue::Text("menu_closed".into()),
            SqlValue::Integer(1_024),
        ];
        let connection = Connection::open(store.database_path()).expect("open raw journal");
        let mut statement = connection.prepare(&explain).expect("prepare query plan");
        let details = statement
            .query_map(rusqlite::params_from_iter(parameters.iter()), |row| {
                row.get::<_, String>(3)
            })
            .expect("query plan")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect query plan");
        assert!(details.iter().any(|detail| {
            detail.contains("SEARCH events USING INDEX events_payload_kind_session_seq")
                && detail.contains("payload_kind=?")
                && detail.contains("session_id=?")
        }));
        assert!(
            details
                .iter()
                .all(|detail| !detail.contains("sqlite_autoindex_events_1"))
        );
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod run_head_projection_tests {
    use super::*;

    fn raw_event(
        seq: u64,
        run_id: Option<&str>,
        branch_id: Option<&str>,
        payload: serde_json::Value,
    ) -> RawEnvelope {
        EventEnvelope {
            schema_version: SCHEMA_VERSION,
            event_id: EventId::new(format!("run-head-event-{seq}")),
            seq,
            session_id: SessionId::new("run-head-session"),
            branch_id: branch_id.map(BranchId::new),
            run_id: run_id.map(RunId::new),
            agent_id: None,
            device_id: DeviceId::new("run-head-device"),
            authority_epoch: 0,
            worker_generation: 1,
            causation_id: None,
            correlation_id: None,
            committed_at_ms: seq,
            render: RenderTargets {
                ui: true,
                durable: true,
                prompt: PromptRender::Omit,
            },
            payload,
        }
    }

    fn event(
        seq: u64,
        run_id: Option<&str>,
        branch_id: Option<&str>,
        payload: EventPayload,
    ) -> RawEnvelope {
        raw_event(
            seq,
            run_id,
            branch_id,
            serde_json::to_value(payload).expect("serialize run-head test payload"),
        )
    }

    fn state(seq: u64, run_id: &str, branch_id: Option<&str>, state: RunState) -> RawEnvelope {
        event(seq, Some(run_id), branch_id, EventPayload::RunState(state))
    }

    fn user(seq: u64, run_id: &str, branch_id: Option<&str>) -> RawEnvelope {
        event(
            seq,
            Some(run_id),
            branch_id,
            EventPayload::UserMessage {
                text: format!("message-{seq}"),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
        )
    }

    fn legacy_run_states(journal: &[RawEnvelope]) -> StoreResult<HashMap<RunId, DurableRunHead>> {
        let mut states = HashMap::new();
        for envelope in journal {
            let Some(run_id) = envelope.run_id.clone() else {
                continue;
            };
            let Ok(EventPayload::RunState(state)) =
                serde_json::from_value::<EventPayload>(envelope.payload.clone())
            else {
                continue;
            };
            if states
                .get(&run_id)
                .is_some_and(|(_, _, branch_id)| branch_id != &envelope.branch_id)
            {
                return Err(corrupt("legacy run crossed branch scopes"));
            }
            states.insert(run_id, (state, envelope.seq, envelope.branch_id.clone()));
        }
        Ok(states)
    }

    fn legacy_durable_runs(
        journal: &[RawEnvelope],
    ) -> StoreResult<HashMap<RunId, ProjectedRunHead>> {
        let mut runs = HashMap::<RunId, ProjectedRunHead>::new();
        for envelope in journal {
            let Some(run_id) = envelope.run_id.clone() else {
                continue;
            };
            if let Ok(RunRetryEventPayload::RunRetried { prompt_run_id, .. }) =
                RunRetryEventPayload::from_payload_value(envelope.payload.clone())
            {
                let head = runs
                    .entry(run_id.clone())
                    .or_insert_with(|| ProjectedRunHead {
                        state: RunState::Queued,
                        state_seq: None,
                        accepted_seq: None,
                        branch_id: envelope.branch_id.clone(),
                        prompt_run_id: None,
                    });
                if head.branch_id != envelope.branch_id {
                    return Err(corrupt("legacy retry crossed branch scopes"));
                }
                head.accepted_seq = Some(envelope.seq);
                head.prompt_run_id = Some(prompt_run_id);
                continue;
            }
            let Ok(payload) = serde_json::from_value::<EventPayload>(envelope.payload.clone())
            else {
                continue;
            };
            match payload {
                EventPayload::RunState(state) => {
                    let head = runs
                        .entry(run_id.clone())
                        .or_insert_with(|| ProjectedRunHead {
                            state: state.clone(),
                            state_seq: None,
                            accepted_seq: None,
                            branch_id: envelope.branch_id.clone(),
                            prompt_run_id: None,
                        });
                    if head.branch_id != envelope.branch_id {
                        return Err(corrupt("legacy state crossed branch scopes"));
                    }
                    head.state = state;
                    head.state_seq = Some(envelope.seq);
                }
                EventPayload::UserMessage { .. } => {
                    let head = runs
                        .entry(run_id.clone())
                        .or_insert_with(|| ProjectedRunHead {
                            state: RunState::Queued,
                            state_seq: None,
                            accepted_seq: None,
                            branch_id: envelope.branch_id.clone(),
                            prompt_run_id: None,
                        });
                    if head.branch_id != envelope.branch_id {
                        return Err(corrupt("legacy user message crossed branch scopes"));
                    }
                    head.accepted_seq = Some(envelope.seq);
                }
                _ => {}
            }
        }
        Ok(runs)
    }

    fn projected_from_journal(
        journal: &[RawEnvelope],
    ) -> StoreResult<HashMap<RunId, ProjectedRunHead>> {
        let mut projected = HashMap::new();
        for envelope in journal {
            apply_run_head_envelope(&mut projected, envelope)?;
        }
        Ok(projected)
    }

    fn assert_projection_equivalent(journal: &[RawEnvelope]) {
        let projected = projected_from_journal(journal).expect("project journal");
        assert_eq!(
            projected,
            legacy_durable_runs(journal).expect("legacy durable reduction")
        );
        let projected_states = projected
            .into_iter()
            .filter_map(|(run_id, head)| {
                head.state_seq
                    .map(|state_seq| (run_id, (head.state, state_seq, head.branch_id)))
            })
            .collect::<HashMap<_, _>>();
        assert_eq!(
            projected_states,
            legacy_run_states(journal).expect("legacy state reduction")
        );
    }

    fn representative_journal() -> Vec<RawEnvelope> {
        let compaction_item = TurnItem::Extension {
            kind: COMPACTION_INTENT_EXTENSION_KIND.into(),
            data: serde_json::json!({
                "operation_id": "compact-op",
                "covers_from": "node-a",
                "covers_to": "node-b",
                "resume_cause": "manual_idle"
            }),
        };
        vec![
            state(1, "run-a", None, RunState::Queued),
            user(2, "run-a", None),
            state(3, "run-a", None, RunState::Thinking),
            state(4, "run-b", Some("branch-b"), RunState::Queued),
            user(5, "run-b", Some("branch-b")),
            state(6, "run-b", Some("branch-b"), RunState::Cancelling),
            state(7, "run-b", Some("branch-b"), RunState::Cancelled),
            state(8, "run-a", None, RunState::Errored),
            raw_event(
                9,
                Some("run-retry"),
                None,
                RunRetryEventPayload::RunRetried {
                    failed_run_id: RunId::new("run-a"),
                    prompt_run_id: RunId::new("run-a"),
                    user_seq: 2,
                }
                .to_payload_value()
                .expect("serialize retry"),
            ),
            state(10, "run-retry", None, RunState::Thinking),
            state(11, "run-retry", None, RunState::Done),
            event(
                12,
                Some("run-compact"),
                Some("branch-compact"),
                EventPayload::Item(ItemEvent::Started {
                    item_id: ItemId::new("compact-intent"),
                    item: compaction_item,
                }),
            ),
            state(
                13,
                "run-compact",
                Some("branch-compact"),
                RunState::Compacting,
            ),
            state(14, "run-compact", Some("branch-compact"), RunState::Done),
        ]
    }

    fn generated_journal(seed: u64) -> Vec<RawEnvelope> {
        let run_ids = ["run-0", "run-1", "run-2", "run-3"];
        let branch_ids = [None, Some("branch-a"), Some("branch-b"), None];
        let mut entropy = seed.wrapping_add(1);
        let mut journal = Vec::new();
        for seq in 1..=48 {
            entropy = entropy
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let run_index = usize::try_from((entropy >> 17) % 4).expect("run index");
            let run_id = run_ids[run_index];
            let branch_id = branch_ids[run_index];
            let envelope = match entropy % 10 {
                0 => user(seq, run_id, branch_id),
                1 => raw_event(
                    seq,
                    Some(run_id),
                    branch_id,
                    RunRetryEventPayload::RunRetried {
                        failed_run_id: RunId::new(format!("failed-{run_id}")),
                        prompt_run_id: RunId::new(format!("prompt-{run_id}")),
                        user_seq: seq.saturating_sub(1).max(1),
                    }
                    .to_payload_value()
                    .expect("serialize generated retry"),
                ),
                2 => event(
                    seq,
                    Some(run_id),
                    branch_id,
                    EventPayload::Item(ItemEvent::Started {
                        item_id: ItemId::new(format!("generated-item-{seq}")),
                        item: TurnItem::Extension {
                            kind: COMPACTION_INTENT_EXTENSION_KIND.into(),
                            data: serde_json::json!({"seed": seed, "seq": seq}),
                        },
                    }),
                ),
                3 => raw_event(
                    seq,
                    Some(run_id),
                    branch_id,
                    serde_json::json!({"type": "future_run_fact", "seed": seed}),
                ),
                variant => {
                    let run_state = match variant {
                        4 => RunState::Queued,
                        5 => RunState::Thinking,
                        6 => RunState::Compacting,
                        7 => RunState::Cancelling,
                        8 => RunState::Cancelled,
                        _ if entropy & 1 == 0 => RunState::Done,
                        _ => RunState::Errored,
                    };
                    state(seq, run_id, branch_id, run_state)
                }
            };
            journal.push(envelope);
        }
        journal
    }

    fn insert_journal(connection: &Connection, journal: &[RawEnvelope]) {
        connection
            .execute(
                "INSERT INTO sessions(id, created_at_ms, meta_json) VALUES (?1, 1, '{}')",
                ["run-head-session"],
            )
            .expect("insert test session");
        for envelope in journal {
            connection
                .execute(
                    "INSERT INTO events(
                         session_id, seq, envelope_json, event_id,
                         committed_at_ms, payload_kind
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        envelope.session_id.as_str(),
                        to_sqlite_integer(envelope.seq).expect("event seq"),
                        encode_envelope(envelope).expect("encode event"),
                        envelope.event_id.as_str(),
                        to_sqlite_integer(envelope.committed_at_ms).expect("commit time"),
                        payload_kind(envelope),
                    ],
                )
                .expect("insert test event");
        }
    }

    #[test]
    fn projection_matches_both_full_reducers_for_prefixes_branches_retries_and_compaction() {
        let journal = representative_journal();
        for prefix in 0..=journal.len() {
            assert_projection_equivalent(&journal[..prefix]);
        }

        let mut forked = journal;
        for envelope in &mut forked {
            envelope.session_id = SessionId::new("forked-session");
            envelope.branch_id = None;
        }
        assert_projection_equivalent(&forked);

        for seed in 0..64 {
            let generated = generated_journal(seed);
            for prefix in 0..=generated.len() {
                assert_projection_equivalent(&generated[..prefix]);
            }
        }
    }

    #[test]
    fn committed_append_batches_incrementally_match_full_recomputation() {
        let mut connection = Connection::open_in_memory().expect("open in-memory database");
        migrations::migrate(&mut connection).expect("migrate database");
        connection
            .execute(
                "INSERT INTO sessions(id, created_at_ms, meta_json) VALUES (?1, 1, '{}')",
                ["run-head-session"],
            )
            .expect("insert append-test session");
        let session_id = SessionId::new("run-head-session");
        let source = generated_journal(17);
        let mut journal = Vec::new();
        let mut offset = 0;
        for batch_len in [1, 5, 2, 9, 3, 11, 7, 10] {
            let mut batch = source[offset..offset + batch_len].to_vec();
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .expect("begin append transaction");
            append_transaction_envelopes(&transaction, &session_id, 99, &mut batch)
                .expect("append projected batch");
            transaction.commit().expect("commit projected batch");
            journal.extend(batch);
            offset += batch_len;
            assert_eq!(
                load_projected_run_heads(&connection, &session_id)
                    .expect("load incremental projection"),
                legacy_durable_runs(&journal).expect("recompute appended journal")
            );
        }
        assert_eq!(offset, source.len());
    }

    #[test]
    fn terminal_failure_batch_matches_the_legacy_full_scan() {
        let mut connection = Connection::open_in_memory().expect("open in-memory database");
        migrations::migrate(&mut connection).expect("migrate database");
        connection
            .execute(
                "INSERT INTO sessions(id, created_at_ms, meta_json) VALUES (?1, 1, '{}')",
                ["run-head-session"],
            )
            .expect("insert terminal-test session");
        let session_id = SessionId::new("run-head-session");
        let run_id = RunId::new("load-stalled-run");

        let mut active = vec![state(1, run_id.as_str(), None, RunState::Thinking)];
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .expect("begin active append");
        append_transaction_envelopes(&transaction, &session_id, 99, &mut active)
            .expect("append active state");
        transaction.commit().expect("commit active state");

        let mut terminal = vec![
            event(
                2,
                Some(run_id.as_str()),
                None,
                EventPayload::RunFailed {
                    code: ErrorCode::ProviderError,
                    message: "process start failed".into(),
                    retryable: true,
                    presentation: None,
                },
            ),
            state(3, run_id.as_str(), None, RunState::Errored),
            event(
                4,
                None,
                None,
                EventPayload::SessionState(SessionState::Idle { interrupted: false }),
            ),
        ];
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .expect("begin terminal append");
        append_transaction_envelopes(&transaction, &session_id, 100, &mut terminal)
            .expect("append adjacent terminal facts");
        transaction
            .commit()
            .expect("commit adjacent terminal facts");

        let journal = active.into_iter().chain(terminal).collect::<Vec<_>>();
        let legacy = legacy_run_states(&journal).expect("legacy full-scan state");
        let projected = latest_run_state(&connection, &session_id, &run_id)
            .expect("projected state query")
            .expect("projected run exists");
        assert_eq!(legacy.get(&run_id), Some(&projected));
        assert_eq!(projected.0, RunState::Errored);
        assert!(projected.0.is_terminal());
    }

    #[test]
    fn stateful_nonterminal_lookup_ignores_user_and_retry_only_heads() {
        let mut connection = Connection::open_in_memory().expect("open in-memory database");
        migrations::migrate(&mut connection).expect("migrate database");
        let user_only = [user(1, "run-user-only", None)];
        insert_journal(&connection, &user_only);
        let session_id = SessionId::new("run-head-session");
        backfill_run_head_projections(&mut connection).expect("backfill user-only journal");

        assert!(!has_nonterminal_run(&connection, &session_id).expect("query user-only head"));
        let user_head = projected_run_head(&connection, &session_id, &RunId::new("run-user-only"))
            .expect("load user-only head")
            .expect("user-only head exists for daemon reduction");
        assert_eq!(user_head.state, RunState::Queued);
        assert_eq!(user_head.state_seq, None);

        let mut retry_only = [raw_event(
            0,
            Some("run-retry-only"),
            None,
            RunRetryEventPayload::RunRetried {
                failed_run_id: RunId::new("run-user-only"),
                prompt_run_id: RunId::new("run-user-only"),
                user_seq: 1,
            }
            .to_payload_value()
            .expect("serialize retry-only head"),
        )];
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .expect("begin retry-only append");
        append_transaction_envelopes(&transaction, &session_id, 99, &mut retry_only)
            .expect("append retry-only head");
        transaction.commit().expect("commit retry-only head");
        assert!(!has_nonterminal_run(&connection, &session_id).expect("query retry-only head"));

        let mut stateful = [state(2, "run-user-only", None, RunState::Queued)];
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .expect("begin stateful append");
        append_transaction_envelopes(&transaction, &session_id, 100, &mut stateful)
            .expect("append stateful head");
        transaction.commit().expect("commit stateful head");
        assert!(has_nonterminal_run(&connection, &session_id).expect("query stateful head"));

        let mut terminal = [state(99, "run-user-only", None, RunState::Done)];
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .expect("begin terminal append");
        append_transaction_envelopes(&transaction, &session_id, 101, &mut terminal)
            .expect("append terminal head");
        transaction.commit().expect("commit terminal head");
        assert!(!has_nonterminal_run(&connection, &session_id).expect("query terminal head"));
    }

    #[test]
    fn group_commit_updates_and_rolls_back_stateful_projections_with_the_journal() {
        let root = tempfile::tempdir().expect("profile");
        let store = Store::open(root.path()).expect("open store");
        let session_a = SessionId::new("run-head-group-a");
        let session_b = SessionId::new("run-head-group-b");
        let mut envelope_a = state(0, "run-a", None, RunState::Queued);
        envelope_a.session_id = session_a.clone();
        envelope_a.event_id = EventId::new("run-head-group-event-a");
        let mut envelope_b = state(0, "run-b", None, RunState::Thinking);
        envelope_b.session_id = session_b.clone();
        envelope_b.event_id = EventId::new("run-head-group-event-b");
        let mut batches = vec![
            JournalAppendBatch {
                envelopes: vec![envelope_a],
                validate_worker_transitions: false,
            },
            JournalAppendBatch {
                envelopes: vec![envelope_b],
                validate_worker_transitions: false,
            },
        ];
        let reject_once = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let reject = std::sync::Arc::clone(&reject_once);
        store
            .connection()
            .expect("journal connection")
            .commit_hook(Some(move || {
                reject.swap(false, std::sync::atomic::Ordering::SeqCst)
            }))
            .expect("install rejecting commit hook");

        store
            .append_group(&mut batches)
            .expect_err("reject stateful outer commit");
        {
            let connection = store
                .connection()
                .expect("journal connection after rollback");
            let event_count = connection
                .query_row("SELECT COUNT(*) FROM events", [], |row| {
                    row.get::<_, i64>(0)
                })
                .expect("count rolled-back events");
            let projection_count = connection
                .query_row("SELECT COUNT(*) FROM run_heads", [], |row| {
                    row.get::<_, i64>(0)
                })
                .expect("count rolled-back projections");
            assert_eq!((event_count, projection_count), (0, 0));
        }

        let outcomes = store
            .append_group(&mut batches)
            .expect("retry stateful group commit");
        assert!(outcomes.iter().all(Result::is_ok));
        let connection = store.connection().expect("journal connection after commit");
        assert_eq!(
            latest_run_state(&connection, &session_a, &RunId::new("run-a"))
                .expect("load committed run a")
                .map(|(state, _, _)| state),
            Some(RunState::Queued)
        );
        assert_eq!(
            latest_run_state(&connection, &session_b, &RunId::new("run-b"))
                .expect("load committed run b")
                .map(|(state, _, _)| state),
            Some(RunState::Thinking)
        );
    }

    #[test]
    fn store_open_migrates_and_backfills_a_v22_journal_idempotently() {
        let root = tempfile::tempdir().expect("profile");
        let database_path;
        let mut journal = representative_journal();
        let expected = legacy_durable_runs(&journal).expect("reduce v22 fixture");
        {
            let store = Store::open(root.path()).expect("open current store");
            database_path = store.database_path().to_path_buf();
            store
                .append(&mut journal)
                .expect("append v22 fixture journal");
        }
        let raw = Connection::open(&database_path).expect("open raw v22 fixture");
        raw.execute_batch(
            "DROP TABLE workflow_node_states;
             DROP TABLE workflow_graph_instances;
             ALTER TABLE profile_meta DROP COLUMN workflow_graph_backfill_version;
             DROP TABLE provider_view_gc;
             DROP TABLE provider_view_blocks;
             DROP TABLE provider_view_requests;
             DROP TABLE provider_view_session_cursors;
             DROP TABLE run_heads;
             DROP TABLE run_head_sessions;
             DELETE FROM schema_migrations WHERE version >= 23;
             PRAGMA user_version = 22;",
        )
        .expect("rewind fixture to v22");
        drop(raw);

        for pass in 0..2 {
            let store = Store::open(root.path()).expect("migrate v22 store");
            assert_eq!(store.schema_version().expect("schema version"), 26);
            let connection = store.connection().expect("migrated journal connection");
            assert_eq!(
                load_projected_run_heads(&connection, &SessionId::new("run-head-session"))
                    .expect("load migrated projection"),
                expected,
                "migration pass {pass}"
            );
        }
    }

    #[test]
    fn projection_backfill_is_resumable_and_corruption_rebuilds_transactionally() {
        let mut connection = Connection::open_in_memory().expect("open in-memory database");
        migrations::migrate(&mut connection).expect("migrate database");
        let mut journal = representative_journal();
        insert_journal(&connection, &journal);
        let session_id = SessionId::new("run-head-session");
        let expected = legacy_durable_runs(&journal).expect("legacy reduction");

        backfill_run_head_projections(&mut connection).expect("initial resumable backfill");
        assert_eq!(
            load_projected_run_heads(&connection, &session_id).expect("load backfill"),
            expected
        );
        backfill_run_head_projections(&mut connection).expect("idempotent backfill");

        connection
            .execute(
                "DELETE FROM run_heads WHERE session_id = ?1 AND run_id = 'run-b'",
                [session_id.as_str()],
            )
            .expect("delete projected row");
        assert_eq!(
            load_projected_run_heads(&connection, &session_id).expect("rebuild missing row"),
            expected
        );

        connection
            .execute(
                "UPDATE run_heads SET checksum = zeroblob(32)
                 WHERE session_id = ?1 AND run_id = 'run-a'",
                [session_id.as_str()],
            )
            .expect("corrupt projected row");
        assert_eq!(
            load_projected_run_heads(&connection, &session_id).expect("rebuild corrupt row"),
            expected
        );

        connection
            .execute(
                "UPDATE run_heads SET checksum = printf('%032d', 0)
                 WHERE session_id = ?1 AND run_id = 'run-b'",
                [session_id.as_str()],
            )
            .expect("replace projected checksum with wrong SQLite storage class");
        assert_eq!(
            load_projected_run_heads(&connection, &session_id)
                .expect("rebuild malformed row storage class"),
            expected
        );

        connection
            .execute(
                "UPDATE run_head_sessions SET through_seq = 'corrupt'
                 WHERE session_id = ?1",
                [session_id.as_str()],
            )
            .expect("replace projection watermark with wrong SQLite storage class");
        assert_eq!(
            load_projected_run_heads(&connection, &session_id)
                .expect("rebuild malformed metadata storage class"),
            expected
        );

        connection
            .execute(
                "UPDATE run_heads SET run_id = 'damaged-run-a'
                 WHERE session_id = ?1 AND run_id = 'run-a'",
                [session_id.as_str()],
            )
            .expect("damage projected primary-key identity");
        assert_eq!(
            projected_run_head(&connection, &session_id, &RunId::new("run-a"))
                .expect("rebuild missing targeted head"),
            expected.get(&RunId::new("run-a")).cloned()
        );

        let next_seq = u64::try_from(journal.len()).expect("journal length") + 1;
        let new_head = state(next_seq, "run-new", None, RunState::Queued);
        connection
            .execute(
                "INSERT INTO events(
                     session_id, seq, envelope_json, event_id,
                     committed_at_ms, payload_kind
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    session_id.as_str(),
                    to_sqlite_integer(next_seq).expect("new seq"),
                    encode_envelope(&new_head).expect("encode new head"),
                    new_head.event_id.as_str(),
                    to_sqlite_integer(new_head.committed_at_ms).expect("new commit time"),
                    payload_kind(&new_head),
                ],
            )
            .expect("insert crash-truncated journal suffix");
        journal.push(new_head);
        let expected_after_suffix = legacy_durable_runs(&journal).expect("legacy suffix reduction");
        assert_eq!(
            load_projected_run_heads(&connection, &session_id).expect("rebuild stale watermark"),
            expected_after_suffix
        );

        let before_seq = journal_latest_seq(&connection, &session_id).expect("journal head");
        let mut rolled_back = [state(0, "rolled-back-run", None, RunState::Queued)];
        {
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .expect("begin append transaction");
            append_transaction_envelopes(&transaction, &session_id, 99, &mut rolled_back)
                .expect("append inside uncommitted transaction");
        }
        assert_eq!(
            journal_latest_seq(&connection, &session_id).expect("journal head after rollback"),
            before_seq
        );
        assert_eq!(
            load_projected_run_heads(&connection, &session_id).expect("projection after rollback"),
            expected_after_suffix
        );

        let checkpoint =
            run_heads_projection_checkpoint(&connection, &session_id).expect("checkpoint");
        let payload = rmp_serde::from_slice::<RunHeadsCheckpointPayload>(&checkpoint.payload)
            .expect("decode checkpoint");
        assert_eq!(payload.version, RUN_HEADS_PAYLOAD_VERSION);
        assert_eq!(payload.heads.len(), expected_after_suffix.len());
        let nonterminal_plan = connection
            .prepare(
                "EXPLAIN QUERY PLAN
                 SELECT run_id, state_json, state_seq, terminal, accepted_seq,
                        branch_id, prompt_run_id, checksum
                 FROM run_heads
                 WHERE session_id = ?1 AND terminal = 0 AND state_seq IS NOT NULL
                 ORDER BY state_seq DESC LIMIT 1",
            )
            .and_then(|mut statement| {
                statement
                    .query_map([session_id.as_str()], |row| row.get::<_, String>(3))?
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .expect("explain nonterminal lookup");
        assert!(
            nonterminal_plan
                .iter()
                .any(|detail| detail.contains("run_heads_nonterminal")),
            "nonterminal lookup plan: {nonterminal_plan:?}"
        );
        let cancellation_plan = connection
            .prepare(
                "EXPLAIN QUERY PLAN
                 SELECT run_id, state_json, state_seq, terminal, accepted_seq,
                        branch_id, prompt_run_id, checksum
                 FROM run_heads WHERE session_id = ?1 AND run_id = ?2",
            )
            .and_then(|mut statement| {
                statement
                    .query_map(params![session_id.as_str(), "run-a"], |row| {
                        row.get::<_, String>(3)
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .expect("explain targeted cancellation lookup");
        assert!(
            cancellation_plan
                .iter()
                .any(|detail| detail.contains("sqlite_autoindex_run_heads_1")),
            "targeted cancellation plan: {cancellation_plan:?}"
        );
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod streaming_usage_checkpoint_tests {
    use super::*;
    use crate::usage_ledger::reduce_journal_usage;

    fn usage_envelope(
        store: &Store,
        session_id: &SessionId,
        event_id: &str,
        input: u64,
        model: &str,
    ) -> RawEnvelope {
        EventEnvelope {
            schema_version: SCHEMA_VERSION,
            event_id: EventId::new(event_id),
            seq: 0,
            session_id: session_id.clone(),
            branch_id: None,
            run_id: Some(RunId::new(format!("run-{session_id}"))),
            agent_id: None,
            device_id: DeviceId::new("usage-checkpoint-test"),
            authority_epoch: 0,
            worker_generation: store.worker_generation(),
            causation_id: None,
            correlation_id: None,
            committed_at_ms: 0,
            render: RenderTargets {
                ui: false,
                durable: true,
                prompt: PromptRender::Omit,
            },
            payload: serde_json::json!({
                "type": "usage",
                "input": input,
                "output": input / 2,
                "reasoning": 0,
                "cached": 0,
                "source": "provider_reported",
                "account": "account-main",
                "scope": {
                    "provider": "test-provider",
                    "model": model,
                    "auth_scope": "api_key",
                    "cache_epoch": "epoch-1"
                }
            }),
        }
    }

    fn append_usage(
        store: &Store,
        session_id: &SessionId,
        event_id: &str,
        input: u64,
        model: &str,
    ) {
        let mut envelope = [usage_envelope(store, session_id, event_id, input, model)];
        store.append(&mut envelope).expect("append usage envelope");
    }

    fn append_fork_audit(store: &Store, session_id: &SessionId, source: &SessionId) {
        let mut envelope = [usage_envelope(
            store,
            session_id,
            "beta-fork-audit",
            0,
            "unused",
        )];
        envelope[0].run_id = None;
        envelope[0].payload = serde_json::json!({
            "type": "session_forked",
            "source_session_id": source,
            "fork_node_id": "fork-node",
            "fork_seq": 2,
            "mode": "fork",
            "context_epoch": "fresh"
        });
        store.append(&mut envelope).expect("append fork audit");
    }

    #[test]
    fn streamed_multi_session_fold_matches_retained_vector_bytes() {
        let root = tempfile::tempdir().expect("profile");
        let store = Store::open(root.path()).expect("store");
        let alpha = SessionId::new("usage-stream-alpha");
        let beta = SessionId::new("usage-stream-beta");
        let mut alpha_pages = (0..=REPLAY_PAGE_SIZE)
            .map(|index| {
                usage_envelope(
                    &store,
                    &alpha,
                    &format!("alpha-{index}"),
                    10 + u64::try_from(index).expect("page index fits u64"),
                    "model-a",
                )
            })
            .collect::<Vec<_>>();
        store
            .append(&mut alpha_pages)
            .expect("append page-crossing usage journal");
        append_usage(&store, &beta, "beta-inherited", 99, "model-inherited");
        append_fork_audit(&store, &beta, &alpha);
        append_usage(&store, &beta, "beta-1", 20, "model-b");

        let retained = reduce_journal_usage(
            &store
                .all_journal_envelopes()
                .expect("retained profile journal"),
        );
        assert!(
            alpha_pages.len() > REPLAY_PAGE_SIZE,
            "fixture must cross the reducer page boundary"
        );
        let streamed = store.fold_usage_journals().expect("streamed usage fold");
        assert_eq!(streamed, retained);
        assert_eq!(
            rmp_serde::to_vec_named(&streamed).expect("encode streamed result"),
            rmp_serde::to_vec_named(&retained).expect("encode retained result")
        );
    }

    #[test]
    fn usage_checkpoint_resumes_from_the_previous_session_head() {
        let root = tempfile::tempdir().expect("profile");
        let store = Store::open(root.path()).expect("store");
        let session_id = SessionId::new("usage-checkpoint-resume");
        append_usage(&store, &session_id, "resume-1", 10, "model-a");
        append_usage(&store, &session_id, "resume-2", 12, "model-a");
        store.fold_usage_journals().expect("initial fold");
        append_usage(&store, &session_id, "resume-3", 14, "model-a");

        let (_, cursor) = store
            .load_usage_checkpoint(&session_id)
            .expect("load checkpoint");
        assert_eq!(cursor, 2, "only the newly appended suffix remains");
        let streamed = store.fold_usage_journals().expect("resume fold");
        let retained = reduce_journal_usage(
            &store
                .all_journal_envelopes()
                .expect("retained profile journal"),
        );
        assert_eq!(streamed, retained);
        let checkpoint = store
            .session_projection_checkpoint(
                &session_id,
                USAGE_CHECKPOINT_PROJECTION,
                USAGE_CHECKPOINT_TIMELINE,
            )
            .expect("checkpoint read")
            .expect("checkpoint exists");
        assert_eq!(checkpoint.through_seq, 3);
    }

    #[test]
    fn corrupt_usage_checkpoint_falls_back_to_a_full_streaming_fold() {
        let root = tempfile::tempdir().expect("profile");
        let store = Store::open(root.path()).expect("store");
        let session_id = SessionId::new("usage-checkpoint-corrupt");
        append_usage(&store, &session_id, "corrupt-1", 10, "model-a");
        append_usage(&store, &session_id, "corrupt-2", 16, "model-a");
        store.fold_usage_journals().expect("initial fold");
        let head = store
            .read(&session_id, 1, 1)
            .expect("read head")
            .pop()
            .expect("head envelope");
        store
            .put_session_projection_checkpoint(&SessionProjectionCheckpoint {
                session_id: session_id.clone(),
                projection: USAGE_CHECKPOINT_PROJECTION.to_owned(),
                timeline_key: USAGE_CHECKPOINT_TIMELINE.to_owned(),
                through_seq: head.seq,
                boundary_event_id: head.event_id,
                payload: b"not a reducer checkpoint".to_vec(),
            })
            .expect("install corrupt checkpoint payload");

        let (_, cursor) = store
            .load_usage_checkpoint(&session_id)
            .expect("corrupt checkpoint is a cache miss");
        assert_eq!(cursor, 0);
        let streamed = store
            .fold_usage_journals()
            .expect("full streaming fallback");
        let retained = reduce_journal_usage(
            &store
                .all_journal_envelopes()
                .expect("retained profile journal"),
        );
        assert_eq!(streamed, retained);
        let (_, cursor) = store
            .load_usage_checkpoint(&session_id)
            .expect("load repaired checkpoint");
        assert_eq!(cursor, 2);

        let connection = Connection::open(store.database_path()).expect("checkpoint connection");
        connection
            .execute(
                "UPDATE session_projection_checkpoints
                 SET payload = 7
                 WHERE session_id = ?1 AND projection = ?2 AND timeline_key = ?3",
                params![
                    session_id.as_str(),
                    USAGE_CHECKPOINT_PROJECTION,
                    USAGE_CHECKPOINT_TIMELINE
                ],
            )
            .expect("corrupt checkpoint storage class");
        let (_, cursor) = store
            .load_usage_checkpoint(&session_id)
            .expect("malformed checkpoint storage is a cache miss");
        assert_eq!(cursor, 0);
        let repaired = store
            .fold_usage_journals()
            .expect("malformed storage falls back to full streaming fold");
        assert_eq!(repaired, retained);
        let (_, cursor) = store
            .load_usage_checkpoint(&session_id)
            .expect("load storage-class-repaired checkpoint");
        assert_eq!(cursor, 2);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod m2d_law_tests {
    use super::*;

    fn fact(seq: u64, payload: EventPayload) -> RawEnvelope {
        EventEnvelope {
            schema_version: SCHEMA_VERSION,
            event_id: EventId::new(format!("m2d-law-{seq}")),
            seq,
            session_id: SessionId::new("m2d-dependency-by-id"),
            branch_id: None,
            run_id: None,
            agent_id: None,
            device_id: DeviceId::new("m2d-law"),
            authority_epoch: 0,
            worker_generation: 1,
            causation_id: None,
            correlation_id: None,
            committed_at_ms: seq,
            render: RenderTargets {
                ui: true,
                durable: true,
                prompt: PromptRender::Omit,
            },
            payload: serde_json::to_value(payload).expect("serialize M2d law fact"),
        }
    }

    fn one_node_pin(graph_id: GraphId) -> EventPayload {
        let node = GraphNodeName::new("WORK").expect("node");
        EventPayload::GraphPinned(GraphPinned {
            graph_id,
            template: "one-node".into(),
            digest: "one-node-digest".into(),
            template_version: 1,
            start_node: Some(node.clone()),
            nodes: vec![haider_protocol::graph::GraphNodeSpec {
                name: node,
                gate: GraphGateKind::CommandGreen,
                executor: haider_protocol::graph::GraphExecutorShape::Inline,
                max_attempts: 2,
                max_evidence_per_attempt: Some(2),
                depends_on: Vec::new(),
                red_target: None,
                verify_slots: Vec::new(),
            }],
        })
    }

    #[test]
    fn dependency_followup_resolves_by_todo_id_then_orders_by_ordinal() {
        // Expected failure under mutation: treating `depends_on_todo_id` as an
        // ordinal unlocks todo 99, while iterating insertion order opens 8 before 9.
        let root = GraphId::new("root");
        let completed = GraphId::new("child-7");
        let later_ordinal = GraphId::new("child-8");
        let earlier_ordinal = GraphId::new("child-9");
        let wrong_dependency = GraphId::new("child-99");
        let run_set_id = GraphRunSetId::new("run-set");
        let plan_item_id = ItemId::new("plan");
        let mut facts = vec![
            fact(1, one_node_pin(root.clone())),
            fact(
                2,
                EventPayload::GraphRunSetOpened(GraphRunSetOpened {
                    run_set_id: run_set_id.clone(),
                    root_graph_id: root,
                    plan_item_id: plan_item_id.clone(),
                    plan_event_id: EventId::new("plan-event"),
                    required_children: 4,
                }),
            ),
        ];
        let attachments = [
            (7, None, completed.clone(), 3),
            (8, Some(7), later_ordinal.clone(), 2),
            (9, Some(7), earlier_ordinal.clone(), 1),
            (99, Some(3), wrong_dependency.clone(), 0),
        ];
        let mut seq = 3;
        for (todo_id, dependency, graph_id, ordinal) in attachments {
            facts.push(fact(
                seq,
                EventPayload::TodoGraphAttached(TodoGraphAttached {
                    run_set_id: run_set_id.clone(),
                    plan_item_id: plan_item_id.clone(),
                    todo_id,
                    depends_on_todo_id: dependency,
                    child_graph_id: graph_id.clone(),
                    ordinal,
                }),
            ));
            seq += 1;
            facts.push(fact(seq, one_node_pin(graph_id)));
            seq += 1;
        }
        facts.push(fact(
            seq,
            EventPayload::GraphAttemptOpened(GraphAttemptOpened {
                graph_id: completed.clone(),
                node: GraphNodeName::new("WORK").expect("node"),
                attempt: 1,
            }),
        ));
        let reductions = reduce_graphs(&facts);
        let followups = todo_child_completed_followups(&reductions, &completed)
            .expect("derive deterministic dependency followups");
        let opened = followups
            .iter()
            .filter_map(|payload| match payload {
                EventPayload::GraphAttemptOpened(opened) => Some(opened.graph_id.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(opened, vec![earlier_ordinal, later_ordinal]);
        assert!(!opened.contains(&wrong_dependency));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod store_synchronous_tests {
    use super::*;

    fn queried_synchronous(connection: &Connection) -> i64 {
        connection
            .pragma_query_value(None, "synchronous", |row| row.get(0))
            .expect("query synchronous pragma")
    }

    /// MUTATION CHECK: deleting the `synchronous` `pragma_update` in
    /// `open_connection_with` (SQLite's own per-connection default is FULL),
    /// or swapping `pragma_value`'s arms, must fail the NORMAL=1 assertion.
    #[test]
    fn open_connection_applies_the_requested_synchronous_mode() {
        let dir = tempfile::tempdir().expect("tempdir");
        let normal =
            open_connection_with(&dir.path().join("normal.sqlite"), StoreSynchronous::Normal)
                .expect("open NORMAL");
        assert_eq!(queried_synchronous(&normal), 1, "NORMAL applies as 1");
        let full = open_connection_with(&dir.path().join("full.sqlite"), StoreSynchronous::Full)
            .expect("open FULL");
        assert_eq!(queried_synchronous(&full), 2, "FULL applies as 2");
    }

    /// MUTATION CHECK: flipping `DEFAULT_STORE_SYNCHRONOUS` to `Full` must
    /// fail this pin (the gate never exports the escape env; the guard below
    /// keeps the pin honest if a shell ever does).
    #[test]
    fn default_open_is_normal_unless_the_env_escape_is_set() {
        let expected = match std::env::var(STORE_SYNCHRONOUS_ENV).as_deref() {
            Ok("full") => 2,
            _ => 1,
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let connection = open_connection(&dir.path().join("default.sqlite")).expect("open default");
        assert_eq!(queried_synchronous(&connection), expected);
    }

    /// MUTATION CHECK: mapping `full` to `Normal`, or accepting arbitrary
    /// values, must fail these parse pins.
    #[test]
    fn synchronous_env_values_parse_exactly() {
        assert_eq!(
            parse_store_synchronous("normal").expect("normal"),
            StoreSynchronous::Normal
        );
        assert_eq!(
            parse_store_synchronous("full").expect("full"),
            StoreSynchronous::Full
        );
        let error = parse_store_synchronous("extra").expect_err("unknown value refuses");
        assert_eq!(error.code, ErrorCode::InvalidArgument);
        assert!(error.message.contains(STORE_SYNCHRONOUS_ENV));
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod group_commit_tests;
