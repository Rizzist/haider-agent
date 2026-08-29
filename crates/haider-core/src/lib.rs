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
    ProviderAttemptDecision, ProviderAttemptResolver, ProviderDeadlineGuard,
    ProviderDerivedRequestState, ProviderPairSwitch, ProviderPairSwitchCause,
    ProviderPairSwitchCommitter, ProviderPairSwitchTarget, RealRetrySleeper,
    RequestInputCheckpoint, ResolvedProviderAttempt, RetrySleeper, STREAM_DELTA_COALESCE_WINDOW,
    SharedToolPacks, SubmitCheckpointTurn, SubmitChildWaitTurn, SubmitCommittedTurn,
    SubmitPartialStreamTurn, SubmitTurn, ToolDispatchResult, ToolDispatcher, TurnHandle,
    TurnOutcome, VISION_IMAGE_ESTIMATE_TOKENS, append_peer_message_to_provider_tail,
    build_cache_request_diagnostic, classify_cache_request, compaction_guard_tripped,
    context_soft_threshold_tokens, estimate_provider_request_input_tokens,
    peer_message_for_provider, presentation_for_haider_error, retry_backoff_ms,
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
    BranchCreateCommand, BranchCreateOutcome, CachedModels, CancelledTurn, CheckpointCommitCommand,
    CheckpointCommitOutcome, ChildGraphAttachCommand, ChildGraphAttachOutcome,
    ChildTemplateCacheEntry, ChildTemplateObservation, ChildTemplateObservationCommand,
    ComputerEvidenceCommand, ComputerEvidenceOutcome, ContextCompactionClaim,
    ContextCompactionReceiptResponse, CreatedBranch, CreatedSession, CreatedSessionFork,
    DelegationCreateOutcome, DelegationDescendant, DelegationDescendants, DelegationRecord,
    DelegationState, GraphAbandonCommand, GraphAbandonOutcome, GraphEvidenceCommand,
    GraphEvidenceOutcome, GraphFinalizationCommand, GraphFinalizationOutcome, GraphInspectResult,
    GraphPinCommand, GraphPinOutcome, GraphRunSetOpenCommand, GraphRunSetOpenOutcome,
    GraphSwitchCommand, GraphSwitchOutcome, HookTrustChange, HookTrustCommand, LoginClaim,
    LoginReceiptFailure, LoginReceiptResponse, LoginReceiptRow, LoomAgentTypeRegistration,
    LoomArchiveResult, LoomRegistryMutation, LoomRegistryWatchPage, ManagementClaim,
    ManagementReceiptRow, MenuResolutionCommand, MenuResolutionOutcome, MonitorControlClaim,
    OpenedGraphRunSet, OpenedTodoGraph, PROVIDER_CONFIGURE_METHOD, PROVIDER_REMOVE_METHOD,
    PROVIDER_SET_TRUST_METHOD, PinnedGraph, ProcessSignalCommand, ProcessSignalOutcome,
    QueueConsumeCommand, QueueConsumeOutcome, QueuePromoteCommand, QueuePromoteOutcome,
    QueueRemoveCommand, QueueRemoveOutcome, QueueSnapshot, RecordedGraphEvidence,
    RecordedProcessSignal, ReducerPage, RenamedSession, RunRetryCommand, RunRetryOutcome,
    SUBAGENT_LIVE_LIMIT, SeenSession, SelectedAgentType, SelectedEffort, SelectedFast,
    SelectedModel, SessionCreateCommand, SessionCreateOutcome, SessionForkCommand,
    SessionForkOutcome, SessionMetaforkCommit, SessionProjectionCheckpoint, SessionRenameCommand,
    SessionRenameOutcome, SessionSeenCommand, SessionSeenOutcome, SessionSelectAgentTypeCommand,
    SessionSelectAgentTypeOutcome, SessionSelectEffortCommand, SessionSelectEffortOutcome,
    SessionSelectFastCommand, SessionSelectFastOutcome, SessionSelectModelCommand,
    SessionSelectModelOutcome, ShellExecAcceptCommand, ShellExecAcceptOutcome, SwitchedGraph,
    TurnAcceptCommand, TurnAcceptOutcome, TurnAdmissionDisposition, TurnCancelCommand,
    TurnCancelOutcome, TurnCancellationStatus, TypedAgentInstallCancelResult, TypedAgentInstallCas,
    TypedAgentInstallItemCas, TypedAgentInstallRetryResult, TypedAgentInstallSnapshot,
    TypedAgentInstallWatchPage, TypedAgentInstallWatchResult,
};
pub use prompt_history::{
    ArtifactReader, CompiledPromptProjection, PromptHistoryCache, PromptHistoryCompiler,
    USER_COMMAND_OUTPUT_PREVIEW_BYTES, task_event_notice,
};
pub use recovery::{RecoveryReport, effect_recovery_evidence, reconcile_dispatched_effects};
pub use sqlite_store::{AppendGroupBatch, ProfileStoreFault, SqliteStoreHandle};

use async_trait::async_trait;
use haider_protocol::branch::BranchDescriptor;
use haider_protocol::cache::{
    ProviderViewBlobV1, ProviderViewBlockRefV1, ProviderViewLedgerV1, ProviderViewStorageV1,
};
use haider_protocol::envelope::{RawEnvelope, envelope_weight_bytes};
use haider_protocol::error::{ErrorCode, HaiderError};
use haider_protocol::ids::{BranchId, SessionId};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// Crate marker used by the workspace self-test.
pub const CRATE_NAME: &str = "haider-core";

/// Sequence allocation returned by an atomic store append.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommittedRange {
    pub first_seq: u64,
    pub last_seq: u64,
}

fn envelope_payload_kind(envelope: &RawEnvelope) -> &str {
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

/// Durability port consumed by the runtime.
///
/// The store assigns `seq` and `committed_at_ms` in place. Only after this
/// method returns successfully may the runtime publish the envelopes.
#[async_trait]
pub trait StoreHandle: Send + Sync {
    async fn append(&self, envelopes: &mut [RawEnvelope]) -> Result<CommittedRange, HaiderError>;

    /// Consumes one logical append and returns its stamped durable batch.
    ///
    /// Implementations with an owned storage path override this to avoid a
    /// deep copy. The default preserves the compatibility contract for test
    /// and embedding stores that only implement the in-place seam.
    async fn append_owned(
        &self,
        mut envelopes: Vec<RawEnvelope>,
    ) -> Result<Arc<[RawEnvelope]>, HaiderError> {
        self.append(&mut envelopes).await?;
        Ok(envelopes.into())
    }

    async fn read(
        &self,
        session_id: &SessionId,
        since_seq: u64,
        limit: usize,
    ) -> Result<Vec<RawEnvelope>, HaiderError>;

    /// Reads a reducer page containing only its declared outer payload kinds.
    ///
    /// Journal-only stores retain correct behavior through this default full
    /// scan. Indexed stores override it so irrelevant encoded envelopes never
    /// leave the database.
    async fn read_reducer_page(
        &self,
        session_id: &SessionId,
        since_seq: u64,
        limit: usize,
        byte_budget: usize,
        payload_kinds: &'static [&'static str],
    ) -> Result<Vec<RawEnvelope>, HaiderError> {
        if limit == 0 || payload_kinds.is_empty() {
            return Ok(Vec::new());
        }
        let mut cursor = since_seq;
        let mut selected = Vec::new();
        let mut spent = 0_usize;
        loop {
            let page = self.read(session_id, cursor, limit).await?;
            if page.is_empty() {
                return Ok(selected);
            }
            let scan_complete = page.len() < limit;
            let next_cursor = page.last().map_or(cursor, |envelope| envelope.seq);
            if next_cursor <= cursor {
                return Err(HaiderError::new(
                    ErrorCode::StoreCorrupt,
                    "journal reducer page did not advance its sequence cursor",
                    false,
                ));
            }
            cursor = next_cursor;
            for envelope in page
                .into_iter()
                .filter(|envelope| payload_kinds.contains(&envelope_payload_kind(envelope)))
            {
                let weight = envelope_weight_bytes(&envelope);
                if !selected.is_empty() && spent.saturating_add(weight) > byte_budget {
                    return Ok(selected);
                }
                spent = spent.saturating_add(weight);
                selected.push(envelope);
                if selected.len() >= limit || spent >= byte_budget {
                    return Ok(selected);
                }
            }
            if scan_complete {
                return Ok(selected);
            }
        }
    }

    /// Reads a filtered page together with a transactionally compatible
    /// journal-head observation when the store can provide one. Journal-only
    /// adapters return no head and remain correct by retaining their reducer
    /// cursor at the last decoded relevant envelope.
    async fn read_reducer_page_with_boundary(
        &self,
        session_id: &SessionId,
        since_seq: u64,
        limit: usize,
        byte_budget: usize,
        payload_kinds: &'static [&'static str],
    ) -> Result<ReducerPage, HaiderError> {
        self.read_reducer_page(session_id, since_seq, limit, byte_budget, payload_kinds)
            .await
            .map(|envelopes| ReducerPage {
                envelopes,
                observed_head: None,
            })
    }

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

    /// Persists exact provider-rendered blocks before the hashes-only ledger
    /// enters the journal. Journal-only test stores validate and discard the
    /// transient bytes; the SQLite implementation owns durable CAS storage.
    async fn persist_provider_view(
        &self,
        session_id: &SessionId,
        mut ledger: ProviderViewLedgerV1,
        blobs: Vec<ProviderViewBlobV1>,
    ) -> Result<ProviderViewLedgerV1, HaiderError> {
        use std::collections::HashSet;
        use std::sync::atomic::{AtomicU64, Ordering};

        static EPHEMERAL_REQUEST_ORDINAL: AtomicU64 = AtomicU64::new(1);
        let expected = std::iter::once(&ledger.system_block)
            .chain(std::iter::once(&ledger.tool_schema_block))
            .chain(ledger.history_blocks.iter())
            .cloned()
            .collect::<HashSet<_>>();
        let actual = blobs
            .into_iter()
            .map(|blob| {
                let computed = ProviderViewBlockRefV1::for_bytes(&blob.bytes);
                (computed == blob.block).then_some(computed).ok_or_else(|| {
                    HaiderError::new(
                        haider_protocol::error::ErrorCode::InvalidArgument,
                        "provider-view blob does not match its content address",
                        false,
                    )
                })
            })
            .collect::<Result<HashSet<_>, _>>()?;
        if actual != expected {
            return Err(HaiderError::new(
                haider_protocol::error::ErrorCode::InvalidArgument,
                "provider-view write does not exactly cover its ledger blocks",
                false,
            ));
        }
        ledger.storage = Some(ProviderViewStorageV1 {
            session_id: session_id.clone(),
            request_ordinal: EPHEMERAL_REQUEST_ORDINAL.fetch_add(1, Ordering::Relaxed),
            expires_at_ms: u64::MAX,
        });
        Ok(ledger)
    }

    /// Verifies the on-disk content addresses for one prior request view.
    async fn verify_provider_view(
        &self,
        _ledger: &ProviderViewLedgerV1,
    ) -> Result<(), HaiderError> {
        Ok(())
    }

    /// Lazily reads one prior request block for replay/keepalive work.
    async fn read_provider_view_block(
        &self,
        _ledger: &ProviderViewLedgerV1,
        _block: &ProviderViewBlockRefV1,
    ) -> Result<Vec<u8>, HaiderError> {
        Err(HaiderError::new(
            haider_protocol::error::ErrorCode::InvalidArgument,
            "this journal-only store has no provider-view blob reader",
            false,
        ))
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
