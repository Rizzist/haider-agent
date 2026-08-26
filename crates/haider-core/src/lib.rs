//! Session runtime for the Haider harness.
//!
//! [`actor::HarnessActor`] drives one session's turns: provider stream events
//! become committed protocol envelopes. This crate also owns the runtime side
//! of the W1 store seam: [`StoreHandle`] mirrors haider-store's `EventStore`
//! surface, with [`SqliteStoreHandle`] adapting the synchronous durable store
//! without blocking a Tokio runtime worker.
//!
//! Seam contract (`StoreHandle`, kept in lockstep with B1):
//! - `append` assigns `seq` and `committed_at_ms` in place; `seq` is
//!   contiguous per session starting at 1, and one batch never spans sessions.
//! - The runtime may publish an envelope only after `append` has returned
//!   success — durable-before-visible.
//! - `event_id` uniqueness is minted by the runtime (see
//!   `HarnessActor::next_event_id`), not the store.
//! - [`SqliteStoreHandle`] also adapts the durable profile CAS to
//!   `haider_tools::CasSink`; bounded tool-result overflow therefore uses the
//!   same verified content-addressed store without coupling tools back to core.

mod actor;
mod fake_store;
mod prompt_history;
mod recovery;
mod sqlite_store;

pub use actor::{
    COMPACTION_MIN_FREED_PERCENT, CacheDiagnosticKey, CancelToken, ChildWaitCheckpoint,
    ContextCompactor, DeferredTicket, DeferredToolCheckpoint, DeferredToolResult, EventIdGenerator,
    FinalizationGuard, FinalizationGuardDecision, HarnessActor, HarnessConfig, HarnessHandle,
    PartialStreamCheckpoint, PreviousCacheRequest, PromotedSteerReservation,
    ProviderAttemptDecision, ProviderAttemptResolver, ProviderDerivedRequestState,
    ProviderPairSwitch, ProviderPairSwitchCause, ProviderPairSwitchCommitter,
    ProviderPairSwitchTarget, RealRetrySleeper, RequestInputCheckpoint, ResolvedProviderAttempt,
    RetrySleeper, STREAM_DELTA_COALESCE_WINDOW, SubmitCheckpointTurn, SubmitChildWaitTurn,
    SubmitCommittedTurn, SubmitPartialStreamTurn, SubmitTurn, ToolDispatchResult, ToolDispatcher,
    TurnHandle, TurnOutcome, VISION_IMAGE_ESTIMATE_TOKENS, build_cache_request_diagnostic,
    classify_cache_request, compaction_guard_tripped, context_soft_threshold_tokens,
    estimate_provider_request_input_tokens, presentation_for_haider_error, retry_backoff_ms,
    retry_jittered_backoff_ms, sanitized_failure_message,
};
pub use fake_store::MemoryStore;
pub use haider_protocol::interaction::{
    InteractionGate, InteractionResolution, InteractionResolutionPolicy,
};
pub use haider_store::{
    ACCOUNT_REMOVE_METHOD, ACCOUNT_SET_ACTIVE_METHOD, ACCOUNT_SET_DEFAULT_MODEL_METHOD,
    AbandonedGraph, AcceptedRunRetry, AcceptedShellExec, AcceptedTurn, AccountAddClaim,
    AccountAddReceiptResponse, AccountAddReceiptRow, AccountRemoveReceiptRow, AttachedChildGraph,
    BranchCreateCommand, BranchCreateOutcome, CachedModels, CancelledTurn, ChildGraphAttachCommand,
    ChildGraphAttachOutcome, ChildTemplateCacheEntry, ChildTemplateObservation,
    ChildTemplateObservationCommand, ComputerEvidenceCommand, ComputerEvidenceOutcome,
    ContextCompactionClaim, ContextCompactionReceiptResponse, CreatedBranch, CreatedSession,
    CreatedSessionFork, DelegationCreateOutcome, DelegationDescendant, DelegationDescendants,
    DelegationRecord, DelegationState, GraphAbandonCommand, GraphAbandonOutcome,
    GraphEvidenceCommand, GraphEvidenceOutcome, GraphFinalizationCommand, GraphFinalizationOutcome,
    GraphInspectResult, GraphPinCommand, GraphPinOutcome, GraphRunSetOpenCommand,
    GraphRunSetOpenOutcome, GraphSwitchCommand, GraphSwitchOutcome, HookTrustChange,
    HookTrustCommand, LoginClaim, LoginReceiptFailure, LoginReceiptResponse, LoginReceiptRow,
    ManagementClaim, ManagementReceiptRow, MenuResolutionCommand, MenuResolutionOutcome,
    OpenedGraphRunSet, OpenedTodoGraph, PROVIDER_CONFIGURE_METHOD, PROVIDER_REMOVE_METHOD,
    PinnedGraph, ProcessSignalCommand, ProcessSignalOutcome, QueueConsumeCommand,
    QueueConsumeOutcome, QueuePromoteCommand, QueuePromoteOutcome, QueueRemoveCommand,
    QueueRemoveOutcome, QueueSnapshot, RecordedGraphEvidence, RecordedProcessSignal,
    RenamedSession, RunRetryCommand, RunRetryOutcome, SUBAGENT_LIVE_LIMIT, SeenSession,
    SelectedAgentType, SelectedEffort, SelectedFast, SelectedModel, SessionCreateCommand,
    SessionCreateOutcome, SessionForkCommand, SessionForkOutcome, SessionMetaforkCommit,
    SessionProjectionCheckpoint, SessionRenameCommand, SessionRenameOutcome, SessionSeenCommand,
    SessionSeenOutcome, SessionSelectAgentTypeCommand, SessionSelectAgentTypeOutcome,
    SessionSelectEffortCommand, SessionSelectEffortOutcome, SessionSelectFastCommand,
    SessionSelectFastOutcome, SessionSelectModelCommand, SessionSelectModelOutcome,
    ShellExecAcceptCommand, ShellExecAcceptOutcome, SwitchedGraph, TurnAcceptCommand,
    TurnAcceptOutcome, TurnAdmissionDisposition, TurnCancelCommand, TurnCancelOutcome,
    TurnCancellationStatus,
};
pub use prompt_history::{
    ArtifactReader, CompiledPromptProjection, PromptHistoryCache, PromptHistoryCompiler,
    USER_COMMAND_OUTPUT_PREVIEW_BYTES, task_event_notice,
};
pub use recovery::{RecoveryReport, effect_recovery_evidence, reconcile_dispatched_effects};
pub use sqlite_store::{AppendGroupBatch, ProfileStoreFault, SqliteStoreHandle};

use async_trait::async_trait;
use haider_protocol::branch::BranchDescriptor;
use haider_protocol::envelope::RawEnvelope;
use haider_protocol::error::HaiderError;
use haider_protocol::ids::{BranchId, SessionId};
use std::time::{SystemTime, UNIX_EPOCH};

/// Crate marker used by the workspace self-test.
pub const CRATE_NAME: &str = "haider-core";

/// Sequence allocation returned by an atomic store append.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommittedRange {
    pub first_seq: u64,
    pub last_seq: u64,
}

/// Durability port consumed by the runtime.
///
/// The store assigns `seq` and `committed_at_ms` in place. Only after this
/// method returns successfully may the runtime publish the envelopes.
#[async_trait]
pub trait StoreHandle: Send + Sync {
    async fn append(&self, envelopes: &mut [RawEnvelope]) -> Result<CommittedRange, HaiderError>;

    async fn read(
        &self,
        session_id: &SessionId,
        since_seq: u64,
        limit: usize,
    ) -> Result<Vec<RawEnvelope>, HaiderError>;

    async fn latest_seq(&self, session_id: &SessionId) -> Result<u64, HaiderError>;

    /// Loads rebuildable projection state for one exact timeline. Stores that
    /// do not implement the optimization behave as an ordinary cache miss.
    async fn projection_checkpoint(
        &self,
        _session_id: &SessionId,
        _projection: &str,
        _timeline_key: &str,
    ) -> Result<Option<SessionProjectionCheckpoint>, HaiderError> {
        Ok(None)
    }

    /// Persists rebuildable projection state. The default deliberately does
    /// nothing; journal-only StoreHandle implementations remain complete.
    async fn put_projection_checkpoint(
        &self,
        _checkpoint: SessionProjectionCheckpoint,
    ) -> Result<(), HaiderError> {
        Ok(())
    }

    /// Resolves the durable named-ref registry from root to requested leaf.
    /// The implicit legacy/main branch returns an empty concrete lineage.
    async fn branch_lineage(
        &self,
        session_id: &SessionId,
        branch_id: Option<&BranchId>,
    ) -> Result<Vec<BranchDescriptor>, HaiderError>;
}

/// Wall-clock milliseconds since the Unix epoch, saturating at the extremes.
pub(crate) fn unix_time_ms() -> u64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    u64::try_from(millis).unwrap_or(u64::MAX)
}
