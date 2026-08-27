//! Async adapter for the synchronous SQLite journal and filesystem CAS.
//!
//! Every potentially blocking profile, SQLite, mutex, and filesystem operation
//! runs on Tokio's blocking pool. The wrapped [`Store`] owns one connection and
//! the profile lock until [`SqliteStoreHandle::close`] or final fallback drop.

use crate::{ArtifactReader, CommittedRange, ReducerPage, StoreHandle};
use async_trait::async_trait;
use haider_protocol::agent::ChildReport;
use haider_protocol::branch::BranchDescriptor;
use haider_protocol::cache::{ProviderViewBlobV1, ProviderViewBlockRefV1, ProviderViewLedgerV1};
use haider_protocol::checkpoint::{CheckpointCursor, CheckpointListPage, CheckpointRecorded};
use haider_protocol::envelope::RawEnvelope;
use haider_protocol::error::{ErrorAction, ErrorCode, ErrorPresentation, ErrorScope, HaiderError};
use haider_protocol::ids::{
    AgentId, ArtifactRef, BranchId, CheckpointId, GraphId, RunId, SessionId,
};
use haider_protocol::session::SessionMetadataV1;
use haider_store::{
    AcceptedRunRetry, AcceptedShellExec, AcceptedTurn, BranchCreateCommand, BranchCreateOutcome,
    CancelledTurn, Cas, CheckpointCommitCommand, CheckpointCommitOutcome, ChildGraphAttachCommand,
    ChildGraphAttachOutcome, ChildTemplateCacheEntry, ChildTemplateObservation,
    ChildTemplateObservationCommand, ComputerEvidenceCommand, ComputerEvidenceOutcome,
    ContextCompactionClaim, ContextCompactionReceiptResponse, DelegationCreateOutcome,
    DelegationRecord, EventStore, GraphAbandonCommand, GraphAbandonOutcome, GraphEvidenceCommand,
    GraphEvidenceOutcome, GraphFinalizationCommand, GraphFinalizationOutcome, GraphInspectResult,
    GraphPinCommand, GraphPinOutcome, GraphRunSetOpenCommand, GraphRunSetOpenOutcome,
    GraphSwitchCommand, GraphSwitchOutcome, HookTrustChange, HookTrustCommand, JournalAppendBatch,
    MenuResolutionCommand, MenuResolutionOutcome, MonitorControlClaim, ProcessSignalCommand,
    ProcessSignalOutcome, ProfileLease, QueueConsumeCommand, QueueConsumeOutcome,
    QueuePromoteCommand, QueuePromoteOutcome, QueuePromotePreview, QueueRemoveCommand,
    QueueRemoveOutcome, QueueSnapshot, RunRetryCommand, RunRetryOutcome, SessionCreateCommand,
    SessionCreateOutcome, SessionForkCommand, SessionForkOutcome, SessionProjectionCheckpoint,
    SessionRenameCommand, SessionRenameOutcome, SessionSeenCommand, SessionSeenOutcome,
    SessionSelectModelCommand, SessionSelectModelOutcome, ShellExecAcceptCommand,
    ShellExecAcceptOutcome, Store, TurnAcceptCommand, TurnAcceptOutcome, TurnCancelCommand,
    TurnCancelOutcome, TypedAgentInstallCas,
};
use haider_tools::{CasSink, ToolResult};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Async, cloneable owner of one locked, persistent SQLite profile.
///
/// Call [`close`](Self::close) on the owning path to move connection teardown,
/// WAL cleanup, and profile-lock release onto Tokio's blocking pool. If callers
/// omit it, dropping the final clone still closes the store synchronously as a
/// best-effort fallback.
#[derive(Clone)]
pub struct SqliteStoreHandle {
    owner: Arc<StoreOwner>,
}

struct StoreOwner {
    root: PathBuf,
    worker_generation: u64,
    store: Mutex<Option<Store>>,
    fault: tokio::sync::watch::Sender<Option<ProfileStoreFault>>,
    #[cfg(test)]
    injected_append_error: Mutex<Option<HaiderError>>,
    #[cfg(test)]
    injected_profile_write_error: Mutex<Option<HaiderError>>,
}

/// One logical session-actor append submitted to the profile group committer.
pub struct AppendGroupBatch {
    pub envelopes: Vec<RawEnvelope>,
    pub validate_worker_transitions: bool,
}

/// Out-of-band durable-store health. This deliberately does not use the
/// journal: the journal is the component reporting that it cannot write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileStoreFault {
    pub presentation: ErrorPresentation,
    /// Stable event/command ids whose durable write did not commit.
    pub failed_write_ids: Vec<String>,
}

impl SqliteStoreHandle {
    /// Acquires the profile lifetime lock before any SQLite open.
    ///
    /// W3b1 daemon seam (additive): `haider-daemon` needs the lock — the
    /// singleton authority — held *before* it inspects or cleans the stale
    /// rendezvous socket, which is earlier than it wants a store. Pairs with
    /// [`Self::open_locked`]; dropping the unconsumed lease releases the lock.
    pub async fn acquire_profile(root: impl AsRef<Path>) -> Result<ProfileLease, HaiderError> {
        let root = root.as_ref().to_path_buf();
        run_blocking(move || Store::acquire_profile(root)).await
    }

    /// Opens a store beneath an already-held profile lifetime lock.
    ///
    /// Second half of the [`Self::acquire_profile`] seam. [`Self::open`]
    /// stays the one-step acquire-and-open for standalone and tests; both
    /// paths consume one worker generation per successful open.
    pub async fn open_locked(lease: ProfileLease) -> Result<Self, HaiderError> {
        let store = run_blocking(move || Store::open_locked(lease)).await?;
        Ok(Self::from_store(store))
    }

    /// Opens or creates `root` without blocking the calling runtime worker.
    pub async fn open(root: impl AsRef<Path>) -> Result<Self, HaiderError> {
        let root = root.as_ref().to_path_buf();
        let store = run_blocking(move || Store::open(root)).await?;
        Ok(Self::from_store(store))
    }

    fn from_store(store: Store) -> Self {
        let root = store.root().to_path_buf();
        let worker_generation = store.worker_generation();
        let (fault, _) = tokio::sync::watch::channel(None);
        Self {
            owner: Arc::new(StoreOwner {
                root,
                worker_generation,
                store: Mutex::new(Some(store)),
                fault,
                #[cfg(test)]
                injected_append_error: Mutex::new(None),
                #[cfg(test)]
                injected_profile_write_error: Mutex::new(None),
            }),
        }
    }

    /// Subscribes to the profile-wide store health latch. Every clone shares
    /// this channel, so journal, account, hook, task, and CAS failures meet at
    /// one daemon-visible seam.
    pub fn subscribe_profile_fault(
        &self,
    ) -> tokio::sync::watch::Receiver<Option<ProfileStoreFault>> {
        self.owner.fault.subscribe()
    }

    /// Current latched store fault, if writes are fenced.
    pub fn profile_fault(&self) -> Option<ProfileStoreFault> {
        self.owner.fault.borrow().clone()
    }

    /// Performs a harmless real SQLite write and clears the profile fault
    /// only after it commits. A flush is insufficient: a read-only database
    /// can flush successfully while mutations remain impossible.
    pub async fn probe_writable(&self) -> Result<(), HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            let result = owner.with_store(Store::probe_writable);
            if result.is_ok() {
                owner.fault.send_replace(None);
            }
            result
        })
        .await
    }

    /// Profile-owned fencing generation allocated by this store open.
    pub fn worker_generation(&self) -> u64 {
        self.owner.worker_generation
    }

    /// Profile root containing the journal and daemon-maintained native
    /// artifacts.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.owner.root
    }

    /// Advances and returns the daemon-process generation.
    ///
    /// W3b1 daemon seam: called once per daemon start, inside the R16 ready
    /// gate, before any listener exists. Deliberately distinct from
    /// [`Self::worker_generation`] — see `Store::advance_daemon_generation`.
    pub async fn advance_daemon_generation(&self) -> Result<u64, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(Store::advance_daemon_generation)).await
    }

    /// Lazily mints or reads the durable per-profile installation id used by
    /// usage-history provenance.
    pub async fn profile_installation_id(&self) -> Result<String, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(Store::profile_installation_id)).await
    }

    /// Completes the one-time journal backfill and reconciles closed slots
    /// before the daemon advertises the usage-history read door.
    pub async fn initialize_usage_history(&self) -> Result<(), HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(Store::initialize_usage_history)).await
    }

    /// Reconciles newly closed journal slots into append-only day files.
    pub async fn reconcile_usage_history(&self) -> Result<(), HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(Store::reconcile_usage_history)).await
    }

    pub async fn usage_history_day(
        &self,
        date: String,
    ) -> Result<Option<haider_protocol::usage::UsageHistoryDayV1>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(|store| store.usage_history_day(&date))).await
    }

    pub async fn usage_history_range(
        &self,
        through_date: String,
        days: u16,
    ) -> Result<Vec<haider_protocol::usage::UsageHistoryRangeDayV1>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| store.usage_history_range(&through_date, days))
        })
        .await
    }

    pub async fn append_usage_meter_sample(
        &self,
        sample: haider_protocol::usage::UsageHistoryMeterSampleV1,
    ) -> Result<(), HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(|store| store.append_usage_meter_sample(&sample)))
            .await
    }

    /// Lists every durable session in stable order.
    ///
    /// W3b1 daemon seam: drives the startup recovery scan
    /// (`reconcile_dispatched_effects`), which must visit every session.
    pub async fn session_ids(&self) -> Result<Vec<SessionId>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(Store::session_ids)).await
    }

    /// Deletes a session after the daemon has fenced new admission and
    /// stopped its actor.
    pub async fn delete_session(&self, session_id: SessionId) -> Result<(), HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(|store| store.delete_session(&session_id))).await
    }

    /// Loads typed live-session configuration; legacy `{}` rows return `None`.
    pub async fn session_metadata(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionMetadataV1>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        let session_id = session_id.clone();
        run_blocking(move || owner.with_store(|store| store.session_metadata(&session_id))).await
    }

    /// Loads one opaque, rebuildable session projection checkpoint.
    pub async fn projection_checkpoint(
        &self,
        session_id: &SessionId,
        projection: String,
        timeline_key: String,
    ) -> Result<Option<SessionProjectionCheckpoint>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        let session_id = session_id.clone();
        run_blocking(move || {
            owner.with_store(|store| {
                store.session_projection_checkpoint(&session_id, &projection, &timeline_key)
            })
        })
        .await
    }

    /// Persists one opaque checkpoint without changing journal rows.
    pub async fn put_projection_checkpoint(
        &self,
        checkpoint: SessionProjectionCheckpoint,
    ) -> Result<(), HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| store.put_session_projection_checkpoint(&checkpoint))
        })
        .await
    }

    pub async fn graph_status(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<haider_protocol::graph::GraphStatus>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        let session_id = session_id.clone();
        run_blocking(move || owner.with_store(|store| store.graph_status(&session_id))).await
    }

    pub async fn workflow_graph_state(
        &self,
        session_id: &SessionId,
        graph_id: Option<GraphId>,
    ) -> Result<Option<haider_protocol::graph::WorkflowGraphState>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        let session_id = session_id.clone();
        run_blocking(move || {
            owner.with_store(|store| store.workflow_graph_state(&session_id, graph_id.as_ref()))
        })
        .await
    }

    pub async fn workflow_graph_watch(
        &self,
        session_id: &SessionId,
        after_cursor: u64,
        limit: u32,
    ) -> Result<haider_protocol::graph::WorkflowGraphWatchPage, HaiderError> {
        let owner = Arc::clone(&self.owner);
        let session_id = session_id.clone();
        run_blocking(move || {
            owner.with_store(|store| store.workflow_graph_watch(&session_id, after_cursor, limit))
        })
        .await
    }

    pub async fn graph_status_by_id(
        &self,
        session_id: &SessionId,
        graph_id: &haider_protocol::ids::GraphId,
    ) -> Result<Option<haider_protocol::graph::GraphStatus>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        let session_id = session_id.clone();
        let graph_id = graph_id.clone();
        run_blocking(move || {
            owner.with_store(|store| store.graph_status_by_id(&session_id, &graph_id))
        })
        .await
    }

    pub async fn graph_reduction_by_id(
        &self,
        session_id: &SessionId,
        graph_id: &haider_protocol::ids::GraphId,
    ) -> Result<Option<haider_protocol::graph::GraphReduction>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        let session_id = session_id.clone();
        let graph_id = graph_id.clone();
        run_blocking(move || {
            owner.with_store(|store| store.graph_reduction_by_id(&session_id, &graph_id))
        })
        .await
    }

    pub async fn graph_runs(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<haider_protocol::graph::GraphRunRow>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        let session_id = session_id.clone();
        run_blocking(move || owner.with_store(|store| store.graph_runs(&session_id))).await
    }

    pub async fn graph_node_attempts(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<haider_protocol::graph::GraphNodeAttemptRow>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        let session_id = session_id.clone();
        run_blocking(move || owner.with_store(|store| store.graph_node_attempts(&session_id))).await
    }

    pub async fn graph_template_rollups(
        &self,
    ) -> Result<Vec<haider_protocol::graph::GraphTemplateRollup>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(Store::graph_template_rollups)).await
    }

    pub async fn graph_inspect(
        &self,
        session_id: &SessionId,
        cursor: Option<String>,
        limit: u32,
    ) -> Result<GraphInspectResult, HaiderError> {
        let owner = Arc::clone(&self.owner);
        let session_id = session_id.clone();
        run_blocking(move || {
            owner.with_store(|store| store.graph_inspect(&session_id, cursor.as_deref(), limit))
        })
        .await
    }

    pub async fn guard_graph_finalization(
        &self,
        command: GraphFinalizationCommand,
    ) -> Result<GraphFinalizationOutcome, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(|store| store.guard_graph_finalization(&command)))
            .await
    }

    pub async fn graph_pin_receipt(
        &self,
        command_id: String,
        request_digest: String,
        request_json: String,
    ) -> Result<Option<haider_store::PinnedGraph>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| {
                store.graph_pin_receipt(&command_id, &request_digest, &request_json)
            })
        })
        .await
    }

    pub async fn pin_graph(
        &self,
        command: GraphPinCommand,
    ) -> Result<GraphPinOutcome, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(|store| store.pin_graph(&command))).await
    }

    pub async fn pin_graph_matching_digest(
        &self,
        command: GraphPinCommand,
        expected_digest: String,
    ) -> Result<GraphPinOutcome, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| store.pin_graph_matching_digest(&command, &expected_digest))
        })
        .await
    }

    pub async fn attach_child_graph(
        &self,
        command: ChildGraphAttachCommand,
    ) -> Result<ChildGraphAttachOutcome, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(|store| store.attach_child_graph(&command))).await
    }

    pub async fn observe_child_template_success(
        &self,
        command: ChildTemplateObservationCommand,
    ) -> Result<ChildTemplateObservation, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| store.observe_child_template_success(&command))
        })
        .await
    }

    pub async fn child_template_cache_lookup(
        &self,
        key: haider_protocol::graph::ChildTemplateCacheKey,
    ) -> Result<Option<ChildTemplateCacheEntry>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(|store| store.child_template_cache_lookup(&key)))
            .await
    }

    pub async fn graph_run_set_open_receipt(
        &self,
        command_id: String,
        request_digest: String,
        request_json: String,
    ) -> Result<Option<haider_store::OpenedGraphRunSet>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| {
                store.graph_run_set_open_receipt(&command_id, &request_digest, &request_json)
            })
        })
        .await
    }

    pub async fn open_graph_run_set(
        &self,
        command: GraphRunSetOpenCommand,
    ) -> Result<GraphRunSetOpenOutcome, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(|store| store.open_graph_run_set(&command))).await
    }

    pub async fn graph_switch_receipt(
        &self,
        command_id: String,
        request_digest: String,
        request_json: String,
    ) -> Result<Option<haider_store::SwitchedGraph>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| {
                store.graph_switch_receipt(&command_id, &request_digest, &request_json)
            })
        })
        .await
    }

    pub async fn switch_graph(
        &self,
        command: GraphSwitchCommand,
    ) -> Result<GraphSwitchOutcome, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(|store| store.switch_graph(&command))).await
    }

    pub async fn switch_graph_matching_digest(
        &self,
        command: GraphSwitchCommand,
        expected_digest: String,
    ) -> Result<GraphSwitchOutcome, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| store.switch_graph_matching_digest(&command, &expected_digest))
        })
        .await
    }

    pub async fn graph_abandon_receipt(
        &self,
        command_id: String,
        request_digest: String,
        request_json: String,
    ) -> Result<Option<haider_store::AbandonedGraph>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| {
                store.graph_abandon_receipt(&command_id, &request_digest, &request_json)
            })
        })
        .await
    }

    pub async fn abandon_graph(
        &self,
        command: GraphAbandonCommand,
    ) -> Result<GraphAbandonOutcome, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(|store| store.abandon_graph(&command))).await
    }

    pub async fn record_graph_evidence(
        &self,
        command: GraphEvidenceCommand,
    ) -> Result<GraphEvidenceOutcome, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(|store| store.record_graph_evidence(&command))).await
    }

    pub async fn record_computer_evidence(
        &self,
        command: ComputerEvidenceCommand,
    ) -> Result<ComputerEvidenceOutcome, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(|store| store.record_computer_evidence(&command)))
            .await
    }

    pub async fn record_process_signal(
        &self,
        command: ProcessSignalCommand,
    ) -> Result<ProcessSignalOutcome, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(|store| store.record_process_signal(&command))).await
    }

    /// Preflights a durable session-create receipt before workspace I/O.
    pub async fn session_create_receipt(
        &self,
        command_id: String,
        request_digest: String,
        request_json: String,
    ) -> Result<Option<haider_store::CreatedSession>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| {
                store.session_create_receipt(&command_id, &request_digest, &request_json)
            })
        })
        .await
    }

    /// Atomically creates session metadata, `Created`, and its command receipt.
    pub async fn create_session(
        &self,
        command: SessionCreateCommand,
    ) -> Result<SessionCreateOutcome, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(|store| store.create_session(&command))).await
    }

    /// Atomically creates a session with an explicit durable interaction mode.
    pub async fn create_session_with_interaction_mode(
        &self,
        command: SessionCreateCommand,
        interaction_mode: haider_protocol::session::SessionInteractionModeV1,
    ) -> Result<SessionCreateOutcome, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| {
                store.create_session_with_interaction_mode(&command, interaction_mode)
            })
        })
        .await
    }

    pub async fn session_fork_receipt(
        &self,
        command_id: String,
        request_digest: String,
        request_json: String,
    ) -> Result<Option<haider_store::CreatedSessionFork>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| {
                store.session_fork_receipt(&command_id, &request_digest, &request_json)
            })
        })
        .await
    }

    pub async fn session_metafork_receipt(
        &self,
        command_id: String,
        request_digest: String,
        request_json: String,
    ) -> Result<Option<haider_store::CreatedSessionFork>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| {
                store.session_metafork_receipt(&command_id, &request_digest, &request_json)
            })
        })
        .await
    }

    pub async fn validate_session_fork_source(
        &self,
        worker_generation: u64,
        source_session_id: SessionId,
        source_branch_id: Option<haider_protocol::ids::BranchId>,
        fork_node_id: haider_protocol::ids::NodeId,
        fork_seq: u64,
    ) -> Result<(), HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| {
                store.validate_session_fork_source(
                    worker_generation,
                    &source_session_id,
                    source_branch_id.as_ref(),
                    &fork_node_id,
                    fork_seq,
                )
            })
        })
        .await
    }

    pub async fn fork_session(
        &self,
        command: SessionForkCommand,
    ) -> Result<SessionForkOutcome, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(|store| store.fork_session(&command))).await
    }

    pub async fn branch_create_receipt(
        &self,
        command_id: String,
        request_digest: String,
        request_json: String,
    ) -> Result<Option<haider_store::CreatedBranch>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| {
                store.branch_create_receipt(&command_id, &request_digest, &request_json)
            })
        })
        .await
    }

    pub async fn create_branch(
        &self,
        command: BranchCreateCommand,
    ) -> Result<BranchCreateOutcome, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(|store| store.create_branch(&command))).await
    }

    /// Reads only the generation coordinate of an existing method-fenced
    /// command receipt.
    pub async fn command_receipt_worker_generation(
        &self,
        command_id: String,
        expected_method: String,
    ) -> Result<Option<u64>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| {
                store.command_receipt_worker_generation(&command_id, &expected_method)
            })
        })
        .await
    }

    /// Preflights a durable `session.select_model` receipt (R2 replay).
    pub async fn session_select_model_receipt(
        &self,
        command_id: String,
        request_digest: String,
        request_json: String,
    ) -> Result<Option<haider_store::SelectedModel>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| {
                store.session_select_model_receipt(&command_id, &request_digest, &request_json)
            })
        })
        .await
    }

    /// Atomically applies one resolved live-session model selection.
    pub async fn select_session_model(
        &self,
        command: SessionSelectModelCommand,
    ) -> Result<SessionSelectModelOutcome, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(|store| store.select_session_model(&command))).await
    }

    /// Preflights a durable `session.rename` receipt (R2 replay).
    pub async fn session_rename_receipt(
        &self,
        command_id: String,
        request_digest: String,
        request_json: String,
    ) -> Result<Option<haider_store::RenamedSession>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| {
                store.session_rename_receipt(&command_id, &request_digest, &request_json)
            })
        })
        .await
    }

    /// Atomically applies one normalized live-session rename (G2).
    pub async fn rename_session(
        &self,
        command: SessionRenameCommand,
    ) -> Result<SessionRenameOutcome, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(|store| store.rename_session(&command))).await
    }

    /// Reads the durable shared attention acknowledgement for one session.
    pub async fn session_seen_at(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<u64>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        let session_id = session_id.clone();
        run_blocking(move || owner.with_store(|store| store.session_seen_at(&session_id))).await
    }

    /// Preflights a durable `session.seen` receipt (attention replay).
    pub async fn session_seen_receipt(
        &self,
        command_id: String,
        request_digest: String,
        request_json: String,
    ) -> Result<Option<haider_store::SeenSession>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| {
                store.session_seen_receipt(&command_id, &request_digest, &request_json)
            })
        })
        .await
    }

    /// Atomically advances one session's durable attention acknowledgement.
    pub async fn mark_session_seen(
        &self,
        command: SessionSeenCommand,
    ) -> Result<SessionSeenOutcome, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(|store| store.mark_session_seen(&command))).await
    }

    /// Preflights a durable `session.select_effort` receipt (R2 replay).
    pub async fn session_select_effort_receipt(
        &self,
        command_id: String,
        request_digest: String,
        request_json: String,
    ) -> Result<Option<haider_store::SelectedEffort>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| {
                store.session_select_effort_receipt(&command_id, &request_digest, &request_json)
            })
        })
        .await
    }

    /// Atomically applies one resolved live-session effort selection (G3).
    pub async fn select_session_effort(
        &self,
        command: haider_store::SessionSelectEffortCommand,
    ) -> Result<haider_store::SessionSelectEffortOutcome, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(|store| store.select_session_effort(&command))).await
    }

    /// Preflights a durable `session.select_agent_type` receipt (R2 replay).
    pub async fn session_select_agent_type_receipt(
        &self,
        command_id: String,
        request_digest: String,
        request_json: String,
    ) -> Result<Option<haider_store::SelectedAgentType>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| {
                store.session_select_agent_type_receipt(&command_id, &request_digest, &request_json)
            })
        })
        .await
    }

    /// Atomically applies one live-session agent-type binding (W-flow).
    pub async fn select_session_agent_type(
        &self,
        command: haider_store::SessionSelectAgentTypeCommand,
    ) -> Result<haider_store::SessionSelectAgentTypeOutcome, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(|store| store.select_session_agent_type(&command)))
            .await
    }

    /// Preflights a durable `session.select_fast` receipt (R2 replay).
    pub async fn session_select_fast_receipt(
        &self,
        command_id: String,
        request_digest: String,
        request_json: String,
    ) -> Result<Option<haider_store::SelectedFast>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| {
                store.session_select_fast_receipt(&command_id, &request_digest, &request_json)
            })
        })
        .await
    }

    /// Atomically applies one validated live-session fast-mode toggle (G3).
    pub async fn select_session_fast(
        &self,
        command: haider_store::SessionSelectFastCommand,
    ) -> Result<haider_store::SessionSelectFastOutcome, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(|store| store.select_session_fast(&command))).await
    }

    pub async fn create_delegation(
        &self,
        record: DelegationRecord,
    ) -> Result<DelegationCreateOutcome, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(|store| store.create_delegation(&record))).await
    }

    pub async fn delegation(
        &self,
        agent: AgentId,
    ) -> Result<Option<DelegationRecord>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(|store| store.delegation(&agent))).await
    }

    pub async fn delegation_for_child_session(
        &self,
        session_id: SessionId,
    ) -> Result<Option<DelegationRecord>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| store.delegation_for_child_session(&session_id))
        })
        .await
    }

    pub async fn delegations_for_parent_run(
        &self,
        session_id: SessionId,
        run_id: RunId,
    ) -> Result<Vec<DelegationRecord>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| store.delegations_for_parent_run(&session_id, &run_id))
        })
        .await
    }

    pub async fn delegation_descendants(
        &self,
        session_id: SessionId,
        max_nodes: usize,
        max_depth: u32,
    ) -> Result<haider_store::DelegationDescendants, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner
                .with_store(|store| store.delegation_descendants(&session_id, max_nodes, max_depth))
        })
        .await
    }

    pub async fn mark_delegation_running(
        &self,
        agent: AgentId,
    ) -> Result<DelegationRecord, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(|store| store.mark_delegation_running(&agent))).await
    }

    pub async fn record_delegation_report(
        &self,
        agent: AgentId,
        report: ChildReport,
    ) -> Result<DelegationRecord, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| store.record_delegation_report(&agent, &report))
        })
        .await
    }

    pub async fn mark_delegation_collected(
        &self,
        agent: AgentId,
    ) -> Result<DelegationRecord, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(|store| store.mark_delegation_collected(&agent)))
            .await
    }

    /// Blocking-pool adapter for `Store::turn_accept_receipt` (R2 replay
    /// lookup; unfenced by design — see the store doc).
    pub async fn turn_accept_receipt(
        &self,
        command_id: String,
        request_digest: String,
        request_json: String,
    ) -> Result<Option<AcceptedTurn>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| {
                store.turn_accept_receipt(&command_id, &request_digest, &request_json)
            })
        })
        .await
    }

    /// Blocking-pool adapter for `Store::accept_turn` (R3 atomic
    /// acceptance; see its caller contract on fence-vs-replay order).
    pub async fn accept_turn(
        &self,
        command: TurnAcceptCommand,
    ) -> Result<TurnAcceptOutcome, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(|store| store.accept_turn(&command))).await
    }

    pub async fn queue_snapshot(
        &self,
        session_id: SessionId,
    ) -> Result<QueueSnapshot, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(|store| store.queue_snapshot(&session_id))).await
    }

    pub async fn queue_remove(
        &self,
        command: QueueRemoveCommand,
    ) -> Result<QueueRemoveOutcome, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(|store| store.queue_remove(&command))).await
    }

    pub async fn queue_promote_steer(
        &self,
        command: QueuePromoteCommand,
    ) -> Result<QueuePromoteOutcome, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(|store| store.queue_promote_steer(&command))).await
    }

    pub async fn queue_promote_preview(
        &self,
        command: QueuePromoteCommand,
    ) -> Result<QueuePromotePreview, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(|store| store.queue_promote_preview(&command))).await
    }

    pub async fn queue_consume(
        &self,
        command: QueueConsumeCommand,
    ) -> Result<Option<QueueConsumeOutcome>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(|store| store.queue_consume(&command))).await
    }

    /// Unfenced `run.retry` receipt lookup for response-loss recovery.
    pub async fn run_retry_receipt(
        &self,
        command_id: String,
        request_digest: String,
        request_json: String,
    ) -> Result<Option<AcceptedRunRetry>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| {
                store.run_retry_receipt(&command_id, &request_digest, &request_json)
            })
        })
        .await
    }

    /// Blocking-pool adapter for atomic terminal-failure retry acceptance.
    pub async fn accept_run_retry(
        &self,
        command: RunRetryCommand,
    ) -> Result<RunRetryOutcome, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(|store| store.accept_run_retry(&command))).await
    }

    /// Unfenced direct-shell receipt lookup for response-loss recovery.
    pub async fn shell_exec_receipt(
        &self,
        command_id: String,
        request_digest: String,
        request_json: String,
    ) -> Result<Option<AcceptedShellExec>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| {
                store.shell_exec_receipt(&command_id, &request_digest, &request_json)
            })
        })
        .await
    }

    /// Blocking-pool adapter for atomic direct-shell acceptance.
    pub async fn accept_shell_exec(
        &self,
        command: ShellExecAcceptCommand,
    ) -> Result<ShellExecAcceptOutcome, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(|store| store.accept_shell_exec(&command))).await
    }

    /// Blocking-pool adapter for `Store::turn_cancel_receipt` (R2 replay
    /// lookup; unfenced by design — see the store doc).
    pub async fn turn_cancel_receipt(
        &self,
        command_id: String,
        request_digest: String,
        request_json: String,
    ) -> Result<Option<CancelledTurn>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| {
                store.turn_cancel_receipt(&command_id, &request_digest, &request_json)
            })
        })
        .await
    }

    /// Blocking-pool adapter for `Store::cancel_turn` (R5 durable
    /// cancellation intent; see its caller contract).
    pub async fn cancel_turn(
        &self,
        command: TurnCancelCommand,
    ) -> Result<TurnCancelOutcome, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(|store| store.cancel_turn(&command))).await
    }

    /// Blocking-pool adapter for `Store::login_claim_receipt` (transaction A
    /// of the R10 two-transaction login shape).
    pub async fn login_claim_receipt(
        &self,
        command_id: String,
        request_digest: String,
        request_json: String,
    ) -> Result<haider_store::LoginClaim, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| {
                store.login_claim_receipt(&command_id, &request_digest, &request_json)
            })
        })
        .await
    }

    /// Blocking-pool adapter for `Store::finalize_login_receipt`
    /// (transaction B: committed).
    pub async fn finalize_login_receipt(
        &self,
        command_id: String,
        response: haider_store::LoginReceiptResponse,
    ) -> Result<u64, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| store.finalize_login_receipt(&command_id, &response))
        })
        .await
    }

    /// Blocking-pool adapter for `Store::fail_login_receipt` (definitive
    /// 401/403 outcomes).
    pub async fn fail_login_receipt(
        &self,
        command_id: String,
        failure: haider_store::LoginReceiptFailure,
    ) -> Result<(), HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| store.fail_login_receipt(&command_id, &failure))
        })
        .await
    }

    /// Blocking-pool adapter for `Store::login_receipts` (startup
    /// reconciliation scan).
    pub async fn login_receipts(&self) -> Result<Vec<haider_store::LoginReceiptRow>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(|store| store.login_receipts())).await
    }

    pub async fn account_add_claim_receipt(
        &self,
        command_id: String,
        request_digest: String,
        request_json: String,
    ) -> Result<haider_store::AccountAddClaim, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| {
                store.account_add_claim_receipt(&command_id, &request_digest, &request_json)
            })
        })
        .await
    }

    pub async fn finalize_account_add_receipt(
        &self,
        command_id: String,
        response: haider_store::AccountAddReceiptResponse,
    ) -> Result<u64, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| store.finalize_account_add_receipt(&command_id, &response))
        })
        .await
    }

    pub async fn account_add_receipts(
        &self,
    ) -> Result<Vec<haider_store::AccountAddReceiptRow>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(|store| store.account_add_receipts())).await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn management_claim_receipt<T>(
        &self,
        command_id: String,
        method: String,
        request_digest: String,
        request_json: String,
        recovery_json: Option<String>,
        expected_revision: Option<u64>,
    ) -> Result<haider_store::ManagementClaim<T>, HaiderError>
    where
        T: serde::de::DeserializeOwned + Send + 'static,
    {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| {
                store.management_claim_receipt(
                    &command_id,
                    &method,
                    &request_digest,
                    &request_json,
                    recovery_json.as_deref(),
                    expected_revision,
                )
            })
        })
        .await
    }

    pub async fn management_receipt_preflight<T>(
        &self,
        command_id: String,
        method: String,
        request_digest: String,
        request_json: String,
    ) -> Result<Option<haider_store::ManagementClaim<T>>, HaiderError>
    where
        T: serde::de::DeserializeOwned + Send + 'static,
    {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| {
                store.management_receipt_preflight(
                    &command_id,
                    &method,
                    &request_digest,
                    &request_json,
                )
            })
        })
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn account_remove_claim_receipt<T>(
        &self,
        command_id: String,
        request_digest: String,
        request_json: String,
        recovery_json: String,
        expected_revision: Option<u64>,
        alias: String,
        provider: String,
        was_active: bool,
    ) -> Result<haider_store::ManagementClaim<T>, HaiderError>
    where
        T: serde::de::DeserializeOwned + Send + 'static,
    {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| {
                store.account_remove_claim_receipt(
                    &command_id,
                    &request_digest,
                    &request_json,
                    &recovery_json,
                    expected_revision,
                    &alias,
                    &provider,
                    was_active,
                )
            })
        })
        .await
    }

    pub async fn finalize_management_receipt<T>(
        &self,
        command_id: String,
        method: String,
        response: T,
    ) -> Result<u64, HaiderError>
    where
        T: serde::Serialize + Send + 'static,
    {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| {
                store.finalize_management_receipt(&command_id, &method, &response)
            })
        })
        .await
    }

    pub async fn finalize_account_remove_receipt<T>(
        &self,
        command_id: String,
        response: T,
    ) -> Result<u64, HaiderError>
    where
        T: serde::Serialize + Send + 'static,
    {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| store.finalize_account_remove_receipt(&command_id, &response))
        })
        .await
    }

    pub async fn finalize_provider_remove_receipt<T>(
        &self,
        command_id: String,
        provider: String,
        response: T,
    ) -> Result<u64, HaiderError>
    where
        T: serde::Serialize + Send + 'static,
    {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| {
                store.finalize_provider_remove_receipt(&command_id, &provider, &response)
            })
        })
        .await
    }

    pub async fn management_receipts(
        &self,
        method: String,
    ) -> Result<Vec<haider_store::ManagementReceiptRow>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(|store| store.management_receipts(&method))).await
    }

    pub async fn provider_management_receipts(
        &self,
    ) -> Result<Vec<haider_store::ManagementReceiptRow>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(Store::provider_management_receipts)).await
    }

    pub async fn account_remove_receipts(
        &self,
    ) -> Result<Vec<haider_store::AccountRemoveReceiptRow>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(Store::account_remove_receipts)).await
    }

    pub async fn reserved_account_aliases(&self) -> Result<Vec<String>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(Store::reserved_account_aliases)).await
    }

    /// Blocking-pool adapter for the coherent account/provider revision.
    pub async fn management_revision(&self) -> Result<u64, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(Store::management_revision)).await
    }

    /// Advances the revision for an actor-owned transition without a command
    /// receipt. Durable management commands use their atomic finalizers.
    pub async fn advance_management_revision(&self) -> Result<u64, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store_write(
                vec!["profile:management-revision".into()],
                Store::advance_management_revision,
            )
        })
        .await
    }

    /// Reads a provider's durable last-known model catalog.
    /// B1 — Loom registry reads/writes (table-backed, run on the blocking pool).
    pub async fn loom_agent_types(
        &self,
    ) -> Result<Vec<haider_protocol::loom::LoomAgentType>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(|store| store.loom_agent_types())).await
    }

    pub async fn loom_agent_types_including_archived(
        &self,
    ) -> Result<Vec<haider_protocol::loom::LoomAgentType>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(Store::loom_agent_types_including_archived)).await
    }

    pub async fn loom_workflows(
        &self,
    ) -> Result<Vec<haider_protocol::loom::LoomWorkflow>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(|store| store.loom_workflows())).await
    }

    pub async fn loom_workflows_including_archived(
        &self,
    ) -> Result<Vec<haider_protocol::loom::LoomWorkflow>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(Store::loom_workflows_including_archived)).await
    }

    pub async fn loom_archived_entries(
        &self,
    ) -> Result<Vec<haider_protocol::loom::LoomRegistryEntryRef>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(Store::loom_archived_entries)).await
    }

    pub async fn loom_workflow(
        &self,
        id: String,
    ) -> Result<Option<haider_protocol::loom::LoomWorkflow>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(|store| store.loom_workflow(&id))).await
    }

    pub async fn loom_workflow_revision(
        &self,
        id: String,
        template_digest: String,
    ) -> Result<Option<haider_protocol::loom::LoomWorkflow>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| store.loom_workflow_revision(&id, &template_digest))
        })
        .await
    }

    pub async fn loom_workflow_registered_revision(
        &self,
        id: String,
        rev: u32,
        digest: String,
    ) -> Result<Option<haider_protocol::loom::LoomWorkflow>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| store.loom_workflow_registered_revision(&id, rev, &digest))
        })
        .await
    }

    pub async fn loom_agent_type(
        &self,
        id: String,
    ) -> Result<Option<haider_protocol::loom::LoomAgentType>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(|store| store.loom_agent_type(&id))).await
    }

    pub async fn loom_agent_type_revision(
        &self,
        id: String,
        rev: u32,
        digest: String,
    ) -> Result<Option<haider_protocol::loom::LoomAgentType>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| store.loom_agent_type_revision(&id, rev, &digest))
        })
        .await
    }

    /// Compatibility create/idempotency helper. Existing changed content is
    /// refused; revisions must use the CAS variant.
    pub async fn loom_register_agent_type(
        &self,
        record: haider_protocol::loom::LoomAgentType,
    ) -> Result<haider_protocol::loom::LoomRegistration, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(|store| store.loom_register_agent_type(&record)))
            .await
    }

    /// Compatibility create/idempotency helper. Existing changed content is
    /// refused; revisions must use the CAS variant.
    pub async fn loom_register_agent_type_with_install(
        &self,
        record: haider_protocol::loom::LoomAgentType,
    ) -> Result<haider_store::LoomAgentTypeRegistration, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| store.loom_register_agent_type_with_install(&record))
        })
        .await
    }

    pub async fn loom_register_agent_type_with_install_cas(
        &self,
        record: haider_protocol::loom::LoomAgentType,
        expected: haider_protocol::loom::LoomRevisionExpectation,
    ) -> Result<
        haider_store::LoomRegistryMutation<haider_store::LoomAgentTypeRegistration>,
        HaiderError,
    > {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| {
                store.loom_register_agent_type_with_install_cas(&record, &expected)
            })
        })
        .await
    }

    pub async fn typed_agent_install_jobs(
        &self,
        job_id: Option<String>,
        agent_type_id: Option<String>,
    ) -> Result<Vec<haider_protocol::typed_agent::TypedAgentInstallJob>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| {
                store.typed_agent_install_jobs(job_id.as_deref(), agent_type_id.as_deref())
            })
        })
        .await
    }

    pub async fn typed_agent_install_items(
        &self,
        job_id: Option<String>,
        agent_type_id: Option<String>,
    ) -> Result<Vec<haider_protocol::typed_agent::TypedAgentInstallItem>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| {
                store.typed_agent_install_items(job_id.as_deref(), agent_type_id.as_deref())
            })
        })
        .await
    }

    pub async fn typed_agent_install_status(
        &self,
        job_id: Option<String>,
        agent_type_id: Option<String>,
    ) -> Result<haider_store::TypedAgentInstallSnapshot, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| {
                store.typed_agent_install_status(job_id.as_deref(), agent_type_id.as_deref())
            })
        })
        .await
    }

    pub async fn typed_agent_install_retry(
        &self,
        job_id: String,
    ) -> Result<haider_store::TypedAgentInstallRetryResult, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(|store| store.typed_agent_install_retry(&job_id)))
            .await
    }

    pub async fn typed_agent_install_cancel(
        &self,
        install_job_id: String,
    ) -> Result<haider_store::TypedAgentInstallCancelResult, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| store.typed_agent_install_cancel(&install_job_id))
        })
        .await
    }

    pub async fn typed_agent_install_watch(
        &self,
        job_id: String,
        after_cursor: u64,
    ) -> Result<haider_store::TypedAgentInstallWatchResult, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| store.typed_agent_install_watch(&job_id, after_cursor))
        })
        .await
    }

    pub async fn typed_agent_install_compare_and_swap(
        &self,
        update: TypedAgentInstallCas,
    ) -> Result<haider_protocol::typed_agent::TypedAgentInstallJob, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| store.typed_agent_install_compare_and_swap(&update))
        })
        .await
    }

    /// Compatibility create/idempotency helper. Existing changed content is
    /// refused; revisions must use the CAS variant.
    pub async fn loom_register_workflow(
        &self,
        source: String,
    ) -> Result<haider_protocol::loom::LoomRegistration, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(|store| store.loom_register_workflow(&source))).await
    }

    pub async fn loom_register_workflow_cas(
        &self,
        source: String,
        expected: haider_protocol::loom::LoomRevisionExpectation,
    ) -> Result<
        haider_store::LoomRegistryMutation<haider_protocol::loom::LoomRegistration>,
        HaiderError,
    > {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| store.loom_register_workflow_cas(&source, &expected))
        })
        .await
    }

    pub async fn loom_set_archived(
        &self,
        kind: haider_protocol::loom::LoomRegistryEntryKind,
        id: String,
        archived: bool,
        expected: haider_protocol::loom::LoomRevisionExpectation,
    ) -> Result<haider_store::LoomArchiveResult, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| store.loom_set_archived(kind, &id, archived, &expected))
        })
        .await
    }

    pub async fn loom_registry_snapshot(
        &self,
    ) -> Result<haider_protocol::loom::LoomRegistrySnapshot, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(Store::loom_registry_snapshot)).await
    }

    pub async fn loom_registry_head(&self) -> Result<u64, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(Store::loom_registry_head)).await
    }

    pub async fn loom_registry_watch_page(
        &self,
        after_cursor: u64,
        through_cursor: u64,
    ) -> Result<haider_store::LoomRegistryWatchPage, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| store.loom_registry_watch_page(after_cursor, through_cursor))
        })
        .await
    }

    pub async fn provider_models(
        &self,
        provider: String,
    ) -> Result<Option<haider_store::CachedModels>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(|store| store.provider_models(&provider))).await
    }

    /// Replaces a provider's durable last-known model catalog.
    pub async fn put_provider_models(
        &self,
        provider: String,
        models_json: String,
        etag: Option<String>,
        fetched_at_ms: u64,
    ) -> Result<(), HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| {
                store.put_provider_models(&provider, &models_json, etag.as_deref(), fetched_at_ms)
            })
        })
        .await
    }

    /// Atomically replaces a provider catalog and advances the management
    /// revision for publication.
    pub async fn put_provider_models_and_advance_management_revision(
        &self,
        provider: String,
        models_json: String,
        etag: Option<String>,
        fetched_at_ms: u64,
    ) -> Result<u64, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| {
                store.put_provider_models_and_advance_management_revision(
                    &provider,
                    &models_json,
                    etag.as_deref(),
                    fetched_at_ms,
                )
            })
        })
        .await
    }

    /// Idempotently repairs a committed pre-v6 management receipt that has no
    /// final revision yet.
    pub async fn ensure_committed_management_revision(
        &self,
        command_id: String,
        method: String,
    ) -> Result<u64, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| {
                store.ensure_committed_management_revision(&command_id, &method)
            })
        })
        .await
    }

    /// Conditionally commits aggregate `Idle` after a transactional durable
    /// quiescence check.
    pub async fn settle_session_idle(
        &self,
        envelope: RawEnvelope,
    ) -> Result<Option<RawEnvelope>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| {
                let mut envelope = envelope;
                if store.settle_session_idle(&mut envelope)? {
                    Ok(Some(envelope))
                } else {
                    Ok(None)
                }
            })
        })
        .await
    }

    /// Blocking-pool adapter for the live-worker transition gate. Unlike the
    /// general startup/test append seam, this validates terminal/Cancelling
    /// truth in the same SQLite transaction that appends the batch.
    pub async fn append_worker(
        &self,
        envelopes: Vec<RawEnvelope>,
    ) -> Result<Vec<RawEnvelope>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| {
                let mut envelopes = envelopes;
                store.append_worker(&mut envelopes)?;
                Ok(envelopes)
            })
        })
        .await
    }

    /// Commits multiple logical actor appends with one SQLite durability flush.
    ///
    /// The nested results preserve each request's semantic success or failure;
    /// no successful result is returned until the shared outer transaction has
    /// committed.
    pub async fn append_group(
        &self,
        batches: Vec<AppendGroupBatch>,
    ) -> Result<Vec<Result<Vec<RawEnvelope>, HaiderError>>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        let failed_write_ids = batches
            .iter()
            .flat_map(|batch| batch.envelopes.iter())
            .map(|envelope| envelope.event_id.as_str().to_owned())
            .collect::<Vec<_>>();
        let result = run_blocking(move || {
            #[cfg(test)]
            if batches
                .iter()
                .any(|batch| !batch.validate_worker_transitions)
                && let Some(error) = owner
                    .injected_append_error
                    .lock()
                    .map_err(|_| owner_lock_error())?
                    .take()
            {
                return Err(error);
            }
            owner.with_store(|store| {
                let mut batches = batches
                    .into_iter()
                    .map(|batch| JournalAppendBatch {
                        envelopes: batch.envelopes,
                        validate_worker_transitions: batch.validate_worker_transitions,
                    })
                    .collect::<Vec<_>>();
                let outcomes = store.append_group(&mut batches)?;
                Ok(batches
                    .into_iter()
                    .zip(outcomes)
                    .map(|(batch, outcome)| outcome.map(|_| batch.envelopes))
                    .collect())
            })
        })
        .await;
        if let Err(error) = &result {
            self.owner.note_failed_write(error, failed_write_ids);
        }
        result
    }

    pub async fn claim_context_compaction_receipt(
        &self,
        command_id: String,
        request_digest: String,
        request_json: String,
    ) -> Result<ContextCompactionClaim, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| {
                store.claim_context_compaction_receipt(&command_id, &request_digest, &request_json)
            })
        })
        .await
    }

    pub async fn finalize_context_compaction_receipt(
        &self,
        command_id: String,
        response: ContextCompactionReceiptResponse,
    ) -> Result<(), HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| {
                store.finalize_context_compaction_receipt(&command_id, &response)
            })
        })
        .await
    }

    pub async fn monitor_control_receipt(
        &self,
        command_id: String,
        method: String,
        request_digest: String,
        request_json: String,
    ) -> Result<Option<serde_json::Value>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| {
                store.monitor_control_receipt(&command_id, &method, &request_digest, &request_json)
            })
        })
        .await
    }

    pub async fn claim_monitor_control_receipt(
        &self,
        command_id: String,
        method: String,
        request_digest: String,
        request_json: String,
    ) -> Result<MonitorControlClaim, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| {
                store.claim_monitor_control_receipt(
                    &command_id,
                    &method,
                    &request_digest,
                    &request_json,
                )
            })
        })
        .await
    }

    pub async fn finalize_monitor_control_receipt(
        &self,
        command_id: String,
        session_id: SessionId,
        accepted_seq: u64,
        response: serde_json::Value,
    ) -> Result<(), HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| {
                store.finalize_monitor_control_receipt(
                    &command_id,
                    &session_id,
                    accepted_seq,
                    &response,
                )
            })
        })
        .await
    }

    /// Reads one true-weight-budgeted replay page (`Store::read_page` law:
    /// retained rows stop at the budget, but a non-empty result always
    /// contains at least one envelope).
    ///
    /// Additive daemon seam rather than part of [`StoreHandle`]: only the
    /// session hub's replay pipeline pages by transient bytes.
    pub async fn read_page(
        &self,
        session_id: &SessionId,
        since_seq: u64,
        max_envelopes: usize,
        byte_budget: usize,
    ) -> Result<Vec<RawEnvelope>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        let session_id = session_id.clone();
        run_blocking(move || {
            owner.with_store(|store| {
                store.read_page(&session_id, since_seq, max_envelopes, byte_budget)
            })
        })
        .await
    }

    /// Atomically resolves a durable menu and appends its authoritative event.
    ///
    /// This is an additive daemon seam rather than part of [`StoreHandle`]:
    /// ordinary harness appends do not carry compare-and-set coordinates.
    pub async fn resolve_menu(
        &self,
        command: MenuResolutionCommand,
    ) -> Result<MenuResolutionOutcome, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(|store| store.resolve_menu(&command))).await
    }

    /// Applies one receipt-backed hook trust mutation on the blocking pool.
    pub async fn apply_hook_trust_command(
        &self,
        command: HookTrustCommand,
    ) -> Result<HookTrustChange, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(|store| store.apply_hook_trust_command(&command)))
            .await
    }

    /// Reads committed hook trust mutations in durable commit order.
    pub async fn hook_trust_changes(&self) -> Result<Vec<HookTrustChange>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(Store::hook_trust_changes)).await
    }

    /// Loads durable hook-dispatch work left by committed journal appends.
    pub async fn pending_hook_dispatches(
        &self,
        limit: usize,
    ) -> Result<Vec<RawEnvelope>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(|store| store.pending_hook_dispatches(limit))).await
    }

    /// Idempotently acknowledges one committed hook-dispatch outbox row.
    pub async fn complete_hook_dispatch(
        &self,
        session_id: &SessionId,
        seq: u64,
    ) -> Result<(), HaiderError> {
        let owner = Arc::clone(&self.owner);
        let session_id = session_id.clone();
        run_blocking(move || {
            owner.with_store(|store| store.complete_hook_dispatch(&session_id, seq))
        })
        .await
    }

    /// Idempotently acknowledges one drain cycle's hook-dispatch outbox rows
    /// in a single durable transaction.
    pub async fn complete_hook_dispatches(
        &self,
        acks: Vec<(SessionId, u64)>,
    ) -> Result<(), HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(|store| store.complete_hook_dispatches(&acks))).await
    }

    /// Checkpoints committed WAL pages before orderly close.
    ///
    /// W3b1 daemon seam: the R17 drain barrier flushes before removing the
    /// socket and closing the store. Under the default `NORMAL` policy an OS
    /// crash can lose the most recent checkpoint window; this orderly flush
    /// persists committed WAL pages and shrinks the successor's replay.
    pub async fn flush(&self) -> Result<(), HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(Store::flush)).await
    }

    /// Closes the SQLite connection and releases the profile lock off runtime
    /// workers. All clones share the close state; calls after close fail.
    pub async fn close(self) -> Result<(), HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            let store = owner.take_store()?;
            drop(store);
            Ok(())
        })
        .await
    }

    /// Durably stores artifact bytes on the blocking pool.
    pub async fn put(&self, bytes: Vec<u8>) -> Result<ArtifactRef, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(|store| store.put(&bytes))).await
    }

    /// Durably stores one artifact-reference group with a single trailing
    /// full device-cache flush on platforms that distinguish it.
    pub async fn put_batch(&self, blobs: Vec<Vec<u8>>) -> Result<Vec<ArtifactRef>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(|store| store.put_batch(&blobs))).await
    }

    /// Durably streams an artifact file on the blocking pool.
    pub async fn put_file(&self, path: std::path::PathBuf) -> Result<ArtifactRef, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(|store| store.put_file(&path))).await
    }

    /// Validates, bounds, and durably stores one PNG/JPEG on the blocking
    /// pool, returning only ref metadata to the caller.
    pub async fn put_image(
        &self,
        bytes: Vec<u8>,
        media_type: String,
    ) -> Result<haider_protocol::tool::ImageBlockRef, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| haider_store::Cas::put_image(store, &bytes, &media_type))
        })
        .await
    }

    /// Reads and verifies an artifact on the blocking pool.
    pub async fn get(&self, artifact: &ArtifactRef) -> Result<Vec<u8>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        let artifact = artifact.clone();
        run_blocking(move || owner.with_store(|store| store.get(&artifact))).await
    }

    /// Verifies an artifact on the blocking pool.
    pub async fn verify(&self, artifact: &ArtifactRef) -> Result<bool, HaiderError> {
        let owner = Arc::clone(&self.owner);
        let artifact = artifact.clone();
        run_blocking(move || owner.with_store(|store| Ok(store.verify(&artifact)))).await
    }

    pub async fn list_checkpoints(
        &self,
        session_id: SessionId,
        branch_id: Option<BranchId>,
        cursor: Option<CheckpointCursor>,
        limit: u16,
    ) -> Result<CheckpointListPage, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| {
                store.list_checkpoints(&session_id, branch_id.as_ref(), cursor.as_ref(), limit)
            })
        })
        .await
    }

    pub async fn checkpoint(
        &self,
        session_id: SessionId,
        checkpoint_id: CheckpointId,
    ) -> Result<Option<CheckpointRecorded>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| store.checkpoint(&session_id, &checkpoint_id))
        })
        .await
    }

    pub async fn checkpoints_for_run(
        &self,
        session_id: SessionId,
        branch_id: Option<BranchId>,
        run_id: RunId,
    ) -> Result<Vec<CheckpointRecorded>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| {
                store.checkpoints_for_run(&session_id, branch_id.as_ref(), &run_id)
            })
        })
        .await
    }

    pub async fn checkpoint_command_receipt(
        &self,
        command_id: String,
        method: String,
        request_digest: String,
        request_json: String,
    ) -> Result<Option<haider_protocol::checkpoint::CheckpointMutationReceipt>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| {
                store.checkpoint_command_receipt(
                    &command_id,
                    &method,
                    &request_digest,
                    &request_json,
                )
            })
        })
        .await
    }

    pub async fn commit_checkpoint_command(
        &self,
        command: CheckpointCommitCommand,
    ) -> Result<CheckpointCommitOutcome, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store_write(vec![command.command_id.clone()], |store| {
                store.commit_checkpoint_command(command)
            })
        })
        .await
    }

    pub async fn persist_provider_view(
        &self,
        session_id: SessionId,
        ledger: ProviderViewLedgerV1,
        blobs: Vec<ProviderViewBlobV1>,
    ) -> Result<ProviderViewLedgerV1, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| store.persist_provider_view(&session_id, ledger, blobs))
        })
        .await
    }

    pub async fn verify_provider_view(
        &self,
        ledger: ProviderViewLedgerV1,
    ) -> Result<(), HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(|store| store.verify_provider_view(&ledger))).await
    }

    pub async fn read_provider_view_block(
        &self,
        ledger: ProviderViewLedgerV1,
        block: ProviderViewBlockRefV1,
    ) -> Result<Vec<u8>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| store.read_provider_view_block(&ledger, &block))
        })
        .await
    }

    pub async fn sweep_expired_provider_views(
        &self,
        through_ms: u64,
    ) -> Result<usize, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            owner.with_store(|store| store.sweep_expired_provider_views(through_ms))
        })
        .await
    }
}

#[async_trait]
impl StoreHandle for SqliteStoreHandle {
    async fn append(&self, envelopes: &mut [RawEnvelope]) -> Result<CommittedRange, HaiderError> {
        let owner = Arc::clone(&self.owner);
        let failed_write_ids = envelopes
            .iter()
            .map(|envelope| envelope.event_id.as_str().to_owned())
            .collect::<Vec<_>>();
        let mut owned = envelopes.to_vec();
        let result = run_blocking(move || {
            #[cfg(test)]
            if let Some(error) = owner
                .injected_append_error
                .lock()
                .map_err(|_| owner_lock_error())?
                .take()
            {
                return Err(error);
            }
            owner.with_store(|store| {
                let range = store.append(&mut owned)?;
                Ok((range, owned))
            })
        })
        .await;
        let (range, committed) = match result {
            Ok(committed) => committed,
            Err(error) => {
                self.owner.note_failed_write(&error, failed_write_ids);
                return Err(error);
            }
        };

        envelopes.clone_from_slice(&committed);
        Ok(CommittedRange {
            first_seq: range.first_seq,
            last_seq: range.last_seq,
        })
    }

    async fn read(
        &self,
        session_id: &SessionId,
        since_seq: u64,
        limit: usize,
    ) -> Result<Vec<RawEnvelope>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        let session_id = session_id.clone();
        run_blocking(move || owner.with_store(|store| store.read(&session_id, since_seq, limit)))
            .await
    }

    async fn read_reducer_page(
        &self,
        session_id: &SessionId,
        since_seq: u64,
        limit: usize,
        byte_budget: usize,
        payload_kinds: &'static [&'static str],
    ) -> Result<Vec<RawEnvelope>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        let session_id = session_id.clone();
        run_blocking(move || {
            owner.with_store(|store| {
                store.read_reducer_page(&session_id, since_seq, limit, byte_budget, payload_kinds)
            })
        })
        .await
    }

    async fn read_reducer_page_with_boundary(
        &self,
        session_id: &SessionId,
        since_seq: u64,
        limit: usize,
        byte_budget: usize,
        payload_kinds: &'static [&'static str],
    ) -> Result<ReducerPage, HaiderError> {
        let owner = Arc::clone(&self.owner);
        let session_id = session_id.clone();
        run_blocking(move || {
            owner.with_store(|store| {
                store.read_reducer_page_with_boundary(
                    &session_id,
                    since_seq,
                    limit,
                    byte_budget,
                    payload_kinds,
                )
            })
        })
        .await
    }

    async fn latest_seq(&self, session_id: &SessionId) -> Result<u64, HaiderError> {
        let owner = Arc::clone(&self.owner);
        let session_id = session_id.clone();
        run_blocking(move || owner.with_store(|store| store.latest_seq(&session_id))).await
    }

    async fn projection_checkpoint(
        &self,
        session_id: &SessionId,
        projection: &str,
        timeline_key: &str,
    ) -> Result<Option<SessionProjectionCheckpoint>, HaiderError> {
        SqliteStoreHandle::projection_checkpoint(
            self,
            session_id,
            projection.to_owned(),
            timeline_key.to_owned(),
        )
        .await
    }

    async fn put_projection_checkpoint(
        &self,
        checkpoint: SessionProjectionCheckpoint,
    ) -> Result<(), HaiderError> {
        SqliteStoreHandle::put_projection_checkpoint(self, checkpoint).await
    }

    async fn persist_provider_view(
        &self,
        session_id: &SessionId,
        ledger: ProviderViewLedgerV1,
        blobs: Vec<ProviderViewBlobV1>,
    ) -> Result<ProviderViewLedgerV1, HaiderError> {
        SqliteStoreHandle::persist_provider_view(self, session_id.clone(), ledger, blobs).await
    }

    async fn verify_provider_view(&self, ledger: &ProviderViewLedgerV1) -> Result<(), HaiderError> {
        SqliteStoreHandle::verify_provider_view(self, ledger.clone()).await
    }

    async fn read_provider_view_block(
        &self,
        ledger: &ProviderViewLedgerV1,
        block: &ProviderViewBlockRefV1,
    ) -> Result<Vec<u8>, HaiderError> {
        SqliteStoreHandle::read_provider_view_block(self, ledger.clone(), block.clone()).await
    }

    async fn branch_lineage(
        &self,
        session_id: &SessionId,
        branch_id: Option<&BranchId>,
    ) -> Result<Vec<BranchDescriptor>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        let session_id = session_id.clone();
        let branch_id = branch_id.cloned();
        run_blocking(move || {
            owner.with_store(|store| store.branch_lineage(&session_id, branch_id.as_ref()))
        })
        .await
    }
}

#[async_trait]
impl ArtifactReader for SqliteStoreHandle {
    async fn read_artifact(&self, artifact: &ArtifactRef) -> Result<Vec<u8>, HaiderError> {
        self.get(artifact).await
    }
}

/// The durable profile CAS is the production overflow sink for bounded tool
/// results. This bridge keeps `haider-tools` independent of the runtime crate.
#[async_trait]
impl CasSink for SqliteStoreHandle {
    async fn put(&mut self, bytes: &[u8]) -> ToolResult<ArtifactRef> {
        SqliteStoreHandle::put(self, bytes.to_vec())
            .await
            .map_err(|error| haider_tools::ToolError::cas(error.message))
    }

    async fn put_batch(&mut self, blobs: &[Vec<u8>]) -> ToolResult<Vec<ArtifactRef>> {
        SqliteStoreHandle::put_batch(self, blobs.to_vec())
            .await
            .map_err(|error| haider_tools::ToolError::cas(error.message))
    }

    async fn put_file(&mut self, path: &std::path::Path) -> ToolResult<ArtifactRef> {
        SqliteStoreHandle::put_file(self, path.to_path_buf())
            .await
            .map_err(|error| haider_tools::ToolError::cas(error.message))
    }

    async fn put_image(
        &mut self,
        bytes: &[u8],
        media_type: &str,
    ) -> ToolResult<haider_protocol::tool::ImageBlockRef> {
        SqliteStoreHandle::put_image(self, bytes.to_vec(), media_type.to_owned())
            .await
            .map_err(|error| haider_tools::ToolError::cas(error.message))
    }
}

impl StoreOwner {
    fn with_store<T>(
        &self,
        operation: impl FnOnce(&Store) -> Result<T, HaiderError>,
    ) -> Result<T, HaiderError> {
        let store = self.store.lock().map_err(|_| owner_lock_error())?;
        let store = store.as_ref().ok_or_else(closed_error)?;
        let result = operation(store);
        if let Err(error) = &result {
            self.note_failed_write(error, Vec::new());
        }
        result
    }

    /// Executes a profile mutation with a stable identifier that can be
    /// shown even when the direct caller drops its returned error. Journal
    /// append has its own batch-aware variant; profile mutations use this
    /// seam whenever the operation has no event envelope to name it.
    fn with_store_write<T>(
        &self,
        failed_write_ids: Vec<String>,
        operation: impl FnOnce(&Store) -> Result<T, HaiderError>,
    ) -> Result<T, HaiderError> {
        #[cfg(test)]
        if let Some(error) = self
            .injected_profile_write_error
            .lock()
            .map_err(|_| owner_lock_error())?
            .take()
        {
            self.note_failed_write(&error, failed_write_ids);
            return Err(error);
        }
        let store = self.store.lock().map_err(|_| owner_lock_error())?;
        let store = store.as_ref().ok_or_else(closed_error)?;
        let result = operation(store);
        if let Err(error) = &result {
            self.note_failed_write(error, failed_write_ids);
        }
        result
    }

    fn note_failed_write(&self, error: &HaiderError, failed_write_ids: Vec<String>) {
        if !matches!(
            error.code,
            ErrorCode::StoreFull | ErrorCode::StoreReadOnly | ErrorCode::StoreUnavailable
        ) {
            return;
        }
        let reason = match error.code {
            ErrorCode::StoreFull => "profile disk is full",
            ErrorCode::StoreReadOnly => "profile filesystem is read-only",
            ErrorCode::StoreUnavailable => "profile storage is unavailable",
            _ => unreachable!(),
        };
        self.fault.send_modify(move |current| {
            let fault = current.get_or_insert_with(|| ProfileStoreFault {
                presentation: ErrorPresentation::new(
                    error.code.as_subcode(),
                    "Store unwritable",
                    format!(
                        "Store unwritable — {reason}. Free space or restore write access, then retry."
                    ),
                    ErrorScope::Profile,
                    [ErrorAction::Retry],
                ),
                failed_write_ids: Vec::new(),
            });
            for id in failed_write_ids {
                if fault.failed_write_ids.len() < 32 && !fault.failed_write_ids.contains(&id) {
                    fault.failed_write_ids.push(id);
                }
            }
            let detail = if fault.failed_write_ids.is_empty() {
                format!(
                    "Store unwritable — {reason}. Free space or restore write access, then retry."
                )
            } else {
                format!(
                    "Store unwritable — {reason}. Not committed: {}. Free space or restore write access, then retry.",
                    fault.failed_write_ids.join(", ")
                )
            };
            fault.presentation = ErrorPresentation::new(
                error.code.as_subcode(),
                "Store unwritable",
                detail,
                ErrorScope::Profile,
                [ErrorAction::Retry],
            );
        });
    }

    fn take_store(&self) -> Result<Option<Store>, HaiderError> {
        self.store
            .lock()
            .map(|mut store| store.take())
            .map_err(|_| owner_lock_error())
    }
}

fn owner_lock_error() -> HaiderError {
    HaiderError::new(
        ErrorCode::Internal,
        "SQLite store owner lock is poisoned",
        false,
    )
}

fn closed_error() -> HaiderError {
    HaiderError::new(ErrorCode::Internal, "SQLite store handle is closed", false)
}

async fn run_blocking<T, F>(operation: F) -> Result<T, HaiderError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, HaiderError> + Send + 'static,
{
    let queued_at = Instant::now();
    tokio::task::spawn_blocking(move || {
        let queue_wait = queued_at.elapsed();
        let started_at = Instant::now();
        let result = operation();
        tracing::trace!(
            target: "haider.store",
            queue_wait_micros = queue_wait.as_micros(),
            operation_micros = started_at.elapsed().as_micros(),
            "store blocking operation completed"
        );
        result
    })
    .await
    .map_err(|error| {
        HaiderError::new(
            ErrorCode::Internal,
            format!("SQLite store blocking task failed: {error}"),
            false,
        )
    })?
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod error_wave_tests {
    use super::*;

    /// E5a profile-level pin: this operation has no journal envelope, but a
    /// swallowed write error must still identify the durable write that was
    /// lost. Replacing `with_store_write` with `with_store` makes this fail.
    #[tokio::test]
    async fn e5a_swallowed_profile_write_surfaces_stable_lost_id() {
        let root = tempfile::tempdir().expect("profile");
        let handle = SqliteStoreHandle::open(root.path()).await.expect("store");
        let mut faults = handle.subscribe_profile_fault();
        *handle
            .owner
            .injected_profile_write_error
            .lock()
            .expect("inject lock") = Some(HaiderError::new(
            ErrorCode::StoreReadOnly,
            "injected SQLITE_READONLY outside the journal",
            true,
        ));

        // Simulates a profile-level `let _ = mutation(...)` call site.
        let _ = handle.advance_management_revision().await;
        tokio::time::timeout(std::time::Duration::from_secs(1), faults.changed())
            .await
            .expect("profile write fault must surface promptly")
            .expect("fault edge");
        let fault = faults.borrow().clone().expect("latched fault");
        assert_eq!(fault.presentation.subcode.as_str(), "store-read-only");
        assert_eq!(
            fault.failed_write_ids,
            vec!["profile:management-revision".to_owned()]
        );
        assert!(
            fault
                .presentation
                .detail
                .contains("profile:management-revision")
        );

        handle.probe_writable().await.expect("same store recovers");
        assert!(handle.profile_fault().is_none());
        handle.close().await.expect("close");
    }

    /// E5a mutation law: deleting `note_failed_write` from `with_store` or
    /// append makes the watch stay empty even though the caller deliberately
    /// swallows the returned mutation error.
    #[tokio::test]
    async fn e5a_swallowed_store_write_still_surfaces_and_names_failed_id() {
        let root = tempfile::tempdir().expect("profile");
        let handle = SqliteStoreHandle::open(root.path()).await.expect("store");
        let mut faults = handle.subscribe_profile_fault();
        *handle
            .owner
            .injected_append_error
            .lock()
            .expect("inject lock") = Some(HaiderError::new(
            ErrorCode::StoreFull,
            "injected SQLITE_FULL during append",
            true,
        ));
        let mut failed = [haider_protocol::envelope::EventEnvelope {
            schema_version: haider_protocol::envelope::SCHEMA_VERSION,
            event_id: haider_protocol::ids::EventId::new("event-e5a-not-committed"),
            seq: 0,
            session_id: SessionId::new("e5a-session"),
            branch_id: None,
            run_id: None,
            agent_id: None,
            device_id: haider_protocol::ids::DeviceId::new("e5a-device"),
            authority_epoch: 0,
            worker_generation: handle.worker_generation(),
            causation_id: None,
            correlation_id: None,
            committed_at_ms: 0,
            render: haider_protocol::envelope::RenderTargets {
                ui: true,
                durable: true,
                prompt: haider_protocol::envelope::PromptRender::Omit,
            },
            payload: serde_json::to_value(haider_protocol::EventPayload::IdleDecayed)
                .expect("payload"),
        }];
        // Simulates the historical `let _ = append(...)` hole. Publication
        // must happen inside the shared store owner before the caller drops
        // the typed mutation error.
        let _ = StoreHandle::append(&handle, &mut failed).await;
        tokio::time::timeout(std::time::Duration::from_secs(1), faults.changed())
            .await
            .expect("journal write fault must surface promptly")
            .expect("fault edge");
        let fault = faults.borrow().clone().expect("latched fault");
        assert_eq!(fault.presentation.subcode.as_str(), "store-full");
        assert_eq!(
            fault.failed_write_ids,
            vec!["event-e5a-not-committed".to_owned()]
        );
        assert!(fault.presentation.detail.starts_with("Store unwritable"));

        handle.probe_writable().await.expect("same store recovers");
        assert!(
            handle.profile_fault().is_none(),
            "healthy write clears latch"
        );
        handle.close().await.expect("close");
    }
}
