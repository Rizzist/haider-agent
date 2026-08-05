//! Async adapter for the synchronous SQLite journal and filesystem CAS.
//!
//! Every potentially blocking profile, SQLite, mutex, and filesystem operation
//! runs on Tokio's blocking pool. The wrapped [`Store`] owns one connection and
//! the profile lock until [`SqliteStoreHandle::close`] or final fallback drop.

use crate::{ArtifactReader, CommittedRange, StoreHandle};
use async_trait::async_trait;
use haider_protocol::agent::ChildReport;
use haider_protocol::branch::BranchDescriptor;
use haider_protocol::envelope::RawEnvelope;
use haider_protocol::error::{ErrorCode, HaiderError};
use haider_protocol::ids::{AgentId, ArtifactRef, BranchId, RunId, SessionId};
use haider_protocol::session::SessionMetadataV1;
use haider_store::{
    AcceptedShellExec, AcceptedTurn, BranchCreateCommand, BranchCreateOutcome, CancelledTurn, Cas,
    ContextCompactionClaim, ContextCompactionReceiptResponse, DelegationCreateOutcome,
    DelegationRecord, EventStore, HookTrustChange, HookTrustCommand, MenuResolutionCommand,
    MenuResolutionOutcome, ProfileLease, SessionCreateCommand, SessionCreateOutcome,
    SessionRenameCommand, SessionRenameOutcome, SessionSelectModelCommand,
    SessionSelectModelOutcome, ShellExecAcceptCommand, ShellExecAcceptOutcome, Store,
    TurnAcceptCommand, TurnAcceptOutcome, TurnCancelCommand, TurnCancelOutcome,
};
use haider_tools::{CasSink, ToolResult};
use std::path::Path;
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
    worker_generation: u64,
    store: Mutex<Option<Store>>,
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
        let worker_generation = store.worker_generation();
        Self {
            owner: Arc::new(StoreOwner {
                worker_generation,
                store: Mutex::new(Some(store)),
            }),
        }
    }

    /// Profile-owned fencing generation allocated by this store open.
    pub fn worker_generation(&self) -> u64 {
        self.owner.worker_generation
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
        run_blocking(move || owner.with_store(Store::advance_management_revision)).await
    }

    /// Reads a provider's durable last-known model catalog.
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

    /// Checkpoints committed WAL pages before orderly close.
    ///
    /// W3b1 daemon seam: the R17 drain barrier flushes before removing the
    /// socket and closing the store. Committed data is durable without this;
    /// flushing just shrinks the successor's WAL replay.
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

    /// Durably streams an artifact file on the blocking pool.
    pub async fn put_file(&self, path: std::path::PathBuf) -> Result<ArtifactRef, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(|store| store.put_file(&path))).await
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
}

#[async_trait]
impl StoreHandle for SqliteStoreHandle {
    async fn append(&self, envelopes: &mut [RawEnvelope]) -> Result<CommittedRange, HaiderError> {
        let owner = Arc::clone(&self.owner);
        let mut owned = envelopes.to_vec();
        let (range, committed) = run_blocking(move || {
            owner.with_store(|store| {
                let range = store.append(&mut owned)?;
                Ok((range, owned))
            })
        })
        .await?;

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

    async fn latest_seq(&self, session_id: &SessionId) -> Result<u64, HaiderError> {
        let owner = Arc::clone(&self.owner);
        let session_id = session_id.clone();
        run_blocking(move || owner.with_store(|store| store.latest_seq(&session_id))).await
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

    async fn put_file(&mut self, path: &std::path::Path) -> ToolResult<ArtifactRef> {
        SqliteStoreHandle::put_file(self, path.to_path_buf())
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
        operation(store)
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
