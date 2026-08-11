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
    CancelToken, ChildWaitCheckpoint, ContextCompactor, DeferredTicket, DeferredToolCheckpoint,
    DeferredToolResult, EventIdGenerator, HarnessActor, HarnessConfig, HarnessHandle,
    ProviderAttemptDecision, ProviderAttemptResolver, RealRetrySleeper, RequestInputCheckpoint,
    ResolvedProviderAttempt, RetrySleeper, SubmitCheckpointTurn, SubmitChildWaitTurn,
    SubmitCommittedTurn, SubmitTurn, ToolDispatchResult, ToolDispatcher, TurnHandle, TurnOutcome,
    VISION_IMAGE_ESTIMATE_TOKENS, context_soft_threshold_tokens,
    estimate_provider_request_input_tokens, retry_backoff_ms, retry_jittered_backoff_ms,
    sanitized_failure_message,
};
pub use fake_store::MemoryStore;
pub use haider_store::{
    ACCOUNT_REMOVE_METHOD, ACCOUNT_SET_ACTIVE_METHOD, ACCOUNT_SET_DEFAULT_MODEL_METHOD,
    AcceptedShellExec, AcceptedTurn, AccountAddClaim, AccountAddReceiptResponse,
    AccountAddReceiptRow, AccountRemoveReceiptRow, BranchCreateCommand, BranchCreateOutcome,
    CachedModels, CancelledTurn, ContextCompactionClaim, ContextCompactionReceiptResponse,
    CreatedBranch, CreatedSession, DelegationCreateOutcome, DelegationRecord, DelegationState,
    HookTrustChange, HookTrustCommand, LoginClaim, LoginReceiptFailure, LoginReceiptResponse,
    LoginReceiptRow, ManagementClaim, ManagementReceiptRow, MenuResolutionCommand,
    MenuResolutionOutcome, PROVIDER_CONFIGURE_METHOD, PROVIDER_REMOVE_METHOD, RenamedSession,
    SelectedEffort, SelectedFast, SelectedModel, SessionCreateCommand, SessionCreateOutcome,
    SessionRenameCommand, SessionRenameOutcome, SessionSelectEffortCommand,
    SessionSelectEffortOutcome, SessionSelectFastCommand, SessionSelectFastOutcome,
    SessionSelectModelCommand, SessionSelectModelOutcome, ShellExecAcceptCommand,
    ShellExecAcceptOutcome, TurnAcceptCommand, TurnAcceptOutcome, TurnAdmissionDisposition,
    TurnCancelCommand, TurnCancelOutcome, TurnCancellationStatus,
};
pub use prompt_history::{
    ArtifactReader, CompiledPromptProjection, PromptHistoryCompiler, task_event_notice,
};
pub use recovery::{RecoveryReport, reconcile_dispatched_effects};
pub use sqlite_store::SqliteStoreHandle;

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
