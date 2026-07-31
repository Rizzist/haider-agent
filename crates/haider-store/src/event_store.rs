//! SQLite-backed event journal. Owns the sequence-allocation law:
//!
//! - `seq` is per-session, starts at 1, and is allocated only at commit time
//!   as `MAX(seq) + 1` inside an IMMEDIATE transaction, so committed
//!   sequences are monotonic and gap-free even across processes.
//! - An envelope is TRUE only once [`EventStore::append`] returns. Publishing
//!   committed envelopes to live subscribers is the caller's duty.
//! - The `envelope_json` column is the authoritative byte-for-byte record;
//!   the `seq` / `event_id` / `committed_at_ms` columns are denormalized
//!   copies for indexing, cross-checked against the JSON on every read.
//! - `worker_generation` is profile-owned and advances once per successful
//!   open while the exclusive profile lock is held, fencing actor identities
//!   across process restarts even when the wall clock repeats.

use crate::cas::FileCas;
use crate::migrations;
use crate::profile_lock::ProfileLock;
use crate::{Cas, StoreResult, now_ms, store_error, to_sqlite_integer};
use haider_protocol::agent::{AgentManifest, ChildReport};
use haider_protocol::credential::CredentialDescriptor;
use haider_protocol::envelope::{
    EventEnvelope, PromptRender, RawEnvelope, RenderTargets, SCHEMA_VERSION, envelope_weight_bytes,
};
use haider_protocol::error::{ErrorCode, HaiderError};
use haider_protocol::history::{COMPACTION_INTENT_EXTENSION_KIND, NodeKind, TreeNode};
use haider_protocol::ids::{
    AgentId, ArtifactRef, DeviceId, EventId, ItemId, MenuId, NodeId, RunId, SessionId,
};
use haider_protocol::item::{ItemEvent, TurnItem};
use haider_protocol::menu::{Menu, MenuAnswer, MenuKind};
use haider_protocol::session::{SessionMetadataV1, SessionPermissionOverridesV1};
use haider_protocol::state::{RunState, SessionState};
use haider_protocol::tool::AttachmentBlock;
use haider_protocol::{DeliveryMode, EventPayload};
use rusqlite::{
    Connection, Error as SqliteError, ErrorCode as SqliteErrorCode, OptionalExtension, Transaction,
    TransactionBehavior, params,
};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const REPLAY_PAGE_SIZE: usize = 1_024;

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
    /// Internal recovery authority: only the daemon session actor may elevate
    /// this after registering the exact durable request_input checkpoint.
    /// Ordinary wire callers always enter with `false`.
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
    Committed { envelope: Box<RawEnvelope> },
    /// The same durable command was retried after its response was lost.
    IdempotentReplay { resolution_seq: u64 },
    /// A different command already resolved the menu.
    AlreadyResolved { resolution_seq: u64 },
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
}

/// Durable response coordinates stored in a committed `turn.submit` receipt.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AcceptedTurn {
    pub session_id: SessionId,
    pub run_id: RunId,
    pub accepted_seq: u64,
    pub worker_generation: u64,
    pub disposition: TurnAdmissionDisposition,
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

fn is_management_method(method: &str) -> bool {
    matches!(
        method,
        LOGIN_METHOD
            | ACCOUNT_ADD_METHOD
            | ACCOUNT_SET_ACTIVE_METHOD
            | ACCOUNT_REMOVE_METHOD
            | ACCOUNT_SET_DEFAULT_MODEL_METHOD
            | PROVIDER_CONFIGURE_METHOD
    )
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
}

/// Claim result for one manual context-compaction command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextCompactionClaim {
    Fresh,
    ResumePending,
    Committed(Box<ContextCompactionReceiptResponse>),
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
    cas: FileCas,
    _lock: ProfileLock,
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
        connection.set_prepared_statement_cache_capacity(16);
        let cas = FileCas::open(&root)?;
        let worker_generation = next_worker_generation(&mut connection)?;

        Ok(Self {
            root,
            database_path,
            worker_generation,
            connection: Mutex::new(connection),
            cas,
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
        let now = now_ms()?;
        transaction
            .execute(
                "INSERT INTO delegations(
                    agent_id, child_session_id, child_run_id,
                    parent_session_id, parent_run_id, call_id, tool_item_id,
                    parent_agent_id, root_session_id, depth, task, prompt,
                    manifest_json, state, report_json, created_at_ms, updated_at_ms
                 ) VALUES (
                    ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                    ?13, 'spawned', NULL, ?14, ?14
                 )",
                params![
                    record.agent_id.as_str(),
                    record.child_session_id.as_str(),
                    record.child_run_id.as_str(),
                    record.parent_session_id.as_str(),
                    record.parent_run_id.as_str(),
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
    /// `Cancelling` may transition only to `Cancelled`.
    pub fn append_worker(&self, envelopes: &mut [RawEnvelope]) -> StoreResult<CommittedSeqRange> {
        append_envelopes(self, envelopes, true)
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
        let envelope_json = serde_json::to_string(&envelope).map_err(|error| {
            store_error(
                ErrorCode::InvalidArgument,
                format!("cannot serialize session-created envelope: {error}"),
                false,
            )
        })?;
        transaction
            .execute(
                "INSERT INTO events(
                    session_id, seq, envelope_json, event_id, committed_at_ms
                 ) VALUES (?1, 1, ?2, ?3, ?4)",
                params![
                    command.session_id.as_str(),
                    envelope_json,
                    command.event_id.as_str(),
                    created_at_sql,
                ],
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
        if latest_run_states(&transaction, &command.session_id)?
            .values()
            .any(|(state, _)| !state.is_terminal())
        {
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
        let mut envelopes = vec![
            unstamped_command_envelope(
                command.running_event_id.clone(),
                &command.session_id,
                Some(command.run_id.clone()),
                command.device_id.clone(),
                self.worker_generation,
                EventPayload::RunState(RunState::RunningTool),
                PromptRender::Omit,
            )?,
            unstamped_command_envelope(
                command.item_event_id.clone(),
                &command.session_id,
                Some(command.run_id.clone()),
                command.device_id.clone(),
                self.worker_generation,
                EventPayload::Item(ItemEvent::Started {
                    item_id: command.item_id.clone(),
                    item: started,
                }),
                PromptRender::Omit,
            )?,
            unstamped_command_envelope(
                command.active_event_id.clone(),
                &command.session_id,
                Some(command.run_id.clone()),
                command.device_id.clone(),
                self.worker_generation,
                EventPayload::SessionState(SessionState::ActiveRun),
                PromptRender::Omit,
            )?,
        ];
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
        let states = latest_run_states(&transaction, &command.session_id)?;
        let same_run_steer = states.get(&command.run_id).is_some_and(|(state, _)| {
            command.mode == DeliveryMode::Steer
                && !state.is_terminal()
                && *state != RunState::Cancelling
        });
        if states.contains_key(&command.run_id) && !same_run_steer {
            return Err(corrupt("daemon-minted turn run id already exists"));
        }
        let has_active = states.values().any(|(state, _)| !state.is_terminal());
        let disposition = if same_run_steer {
            // W6c activates the reserved same-run steer shape: the durable
            // user message commits here, then the manager delivers it to the
            // active harness at its next provider-request boundary.
            TurnAdmissionDisposition::SteerPending
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

        let parent = latest_tree_head(
            &transaction,
            &command.session_id,
            None,
            command.agent_id.as_ref(),
        )?;
        let user_node = TreeNode {
            node: NodeId::new(format!("node-{}", command.user_event_id)),
            parent,
            kind: NodeKind::UserTurn {
                text: command.text.clone(),
                attachments: command.attachments.clone(),
            },
        };
        let mut envelopes = if same_run_steer {
            vec![
                unstamped_command_envelope(
                    command.user_event_id.clone(),
                    &command.session_id,
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
                    Some(command.run_id.clone()),
                    command.device_id.clone(),
                    self.worker_generation,
                    EventPayload::RunState(RunState::Queued),
                    PromptRender::Omit,
                )?,
                unstamped_command_envelope(
                    command.user_event_id.clone(),
                    &command.session_id,
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
                Some(command.run_id.clone()),
                command.device_id.clone(),
                self.worker_generation,
                EventPayload::SessionState(SessionState::ActiveRun),
                PromptRender::Omit,
            )?);
        }
        for envelope in &mut envelopes {
            envelope.agent_id = command.agent_id.clone();
        }
        append_transaction_envelopes(&transaction, &command.session_id, now, &mut envelopes)?;
        let accepted_seq = if same_run_steer {
            envelopes[0].seq
        } else {
            envelopes[1].seq
        };
        let accepted = AcceptedTurn {
            session_id: command.session_id.clone(),
            run_id: command.run_id.clone(),
            accepted_seq,
            worker_generation: self.worker_generation,
            disposition,
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
        let states = latest_run_states(&transaction, &command.session_id)?;
        let Some((state, state_seq)) = states.get(&command.run_id) else {
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
                    terminal_seq: Some(*state_seq),
                },
                None,
            )
        } else if *state == RunState::Cancelling {
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
    /// command identity and must never contain secret material.
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
        let claim = claim_management_receipt_in_transaction(
            &transaction,
            command_id,
            method,
            request_digest,
            request_json,
            recovery_json,
            expected_revision,
        )?;
        transaction.commit().map_err(map_sqlite_error)?;
        Ok(claim)
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

    /// Finalizes a generic W5 management receipt and allocates its revision
    /// in the same SQLite transaction.
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
        let states = latest_run_states(&transaction, &session_id)?;
        if states.values().any(|(state, _)| !state.is_terminal()) {
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
    /// W3b1 seam (additive), used by the daemon drain barrier. Committed data
    /// is durable without this; checkpointing shrinks the WAL a successor
    /// must replay. A busy checkpoint surfaces as retryable `StoreLocked`.
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
        let outcome = resolve_menu_transaction(&transaction, command, self.worker_generation)?;
        transaction.commit().map_err(map_sqlite_error)?;
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
            let envelope_json: String = row.get(1).map_err(map_sqlite_error)?;
            let stored_event_id: String = row.get(2).map_err(map_sqlite_error)?;
            let stored_committed_at_ms: i64 = row.get(3).map_err(map_sqlite_error)?;
            let envelope: RawEnvelope = serde_json::from_str(&envelope_json).map_err(|error| {
                corrupt(format!(
                    "invalid envelope JSON for session {session}, seq {stored_seq}: {error}"
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

fn latest_run_states(
    connection: &Connection,
    session_id: &SessionId,
) -> StoreResult<HashMap<RunId, (RunState, u64)>> {
    let mut statement = connection
        .prepare_cached(
            "SELECT seq, envelope_json FROM events
             WHERE session_id = ?1 ORDER BY seq ASC",
        )
        .map_err(map_sqlite_error)?;
    let mut rows = statement
        .query([session_id.as_str()])
        .map_err(map_sqlite_error)?;
    let mut states = HashMap::new();
    while let Some(row) = rows.next().map_err(map_sqlite_error)? {
        let seq = sql_u64(row.get(0).map_err(map_sqlite_error)?).map_err(map_sqlite_error)?;
        let json: String = row.get(1).map_err(map_sqlite_error)?;
        let envelope: RawEnvelope = serde_json::from_str(&json).map_err(|error| {
            corrupt(format!(
                "invalid envelope JSON for session {session_id}, seq {seq}: {error}"
            ))
        })?;
        let Some(run_id) = envelope.run_id else {
            continue;
        };
        if let Ok(EventPayload::RunState(state)) =
            serde_json::from_value::<EventPayload>(envelope.payload)
        {
            states.insert(run_id, (state, seq));
        }
    }
    Ok(states)
}

fn latest_tree_head(
    connection: &Connection,
    session_id: &SessionId,
    branch_id: Option<&haider_protocol::ids::BranchId>,
    agent_id: Option<&AgentId>,
) -> StoreResult<Option<NodeId>> {
    let mut statement = connection
        .prepare_cached(
            "SELECT seq, envelope_json FROM events
             WHERE session_id = ?1 ORDER BY seq DESC",
        )
        .map_err(map_sqlite_error)?;
    let mut rows = statement
        .query([session_id.as_str()])
        .map_err(map_sqlite_error)?;
    while let Some(row) = rows.next().map_err(map_sqlite_error)? {
        let seq: i64 = row.get(0).map_err(map_sqlite_error)?;
        let json: String = row.get(1).map_err(map_sqlite_error)?;
        let envelope: RawEnvelope = serde_json::from_str(&json).map_err(|error| {
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

fn unstamped_command_envelope(
    event_id: EventId,
    session_id: &SessionId,
    run_id: Option<RunId>,
    device_id: DeviceId,
    worker_generation: u64,
    payload: EventPayload,
    prompt: PromptRender,
) -> StoreResult<RawEnvelope> {
    Ok(EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id,
        seq: 0,
        session_id: session_id.clone(),
        branch_id: None,
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
        payload: serde_json::to_value(payload).map_err(|error| {
            store_error(
                ErrorCode::InvalidArgument,
                format!("cannot serialize command envelope payload: {error}"),
                false,
            )
        })?,
    })
}

fn append_transaction_envelopes(
    transaction: &Transaction<'_>,
    session_id: &SessionId,
    committed_at_ms: u64,
    envelopes: &mut [RawEnvelope],
) -> StoreResult<()> {
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
                session_id, seq, envelope_json, event_id, committed_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .map_err(map_sqlite_error)?;
    for (offset, envelope) in envelopes.iter_mut().enumerate() {
        let offset = u64::try_from(offset).map_err(|_| corrupt("event batch is too large"))?;
        envelope.seq = first
            .checked_add(offset)
            .ok_or_else(|| corrupt("event sequence space is exhausted"))?;
        envelope.committed_at_ms = committed_at_ms;
        let json = serde_json::to_string(envelope).map_err(|error| {
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
                json,
                envelope.event_id.as_str(),
                to_sqlite_integer(committed_at_ms)?,
            ])
            .map_err(map_sqlite_error)?;
    }
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
    let revision = next_management_revision_in_transaction(transaction)?;
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
    if command.worker_generation != current_worker_generation && !command.allow_prior_generation {
        return Err(stale_generation(
            command.worker_generation,
            current_worker_generation,
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

    let latest: i64 = transaction
        .prepare_cached("SELECT COALESCE(MAX(seq), 0) FROM events WHERE session_id = ?1")
        .and_then(|mut statement| {
            statement.query_row([command.session_id.as_str()], |row| row.get(0))
        })
        .map_err(map_sqlite_error)?;
    let resolution_seq = u64::try_from(latest)
        .map_err(|_| corrupt("database contains a negative event sequence"))?
        .checked_add(1)
        .ok_or_else(|| corrupt("event sequence space is exhausted"))?;
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
        seq: resolution_seq,
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
    let envelope_json = serde_json::to_string(&envelope).map_err(|error| {
        store_error(
            ErrorCode::InvalidArgument,
            format!("cannot serialize menu resolution envelope: {error}"),
            false,
        )
    })?;
    transaction
        .execute(
            "INSERT INTO events(
                session_id, seq, envelope_json, event_id, committed_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                command.session_id.as_str(),
                to_sqlite_integer(resolution_seq)?,
                envelope_json,
                event_id.as_str(),
                to_sqlite_integer(committed_at_ms)?,
            ],
        )
        .map_err(map_sqlite_error)?;
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
        envelope: Box::new(envelope),
    })
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
    transaction: &Transaction<'_>,
    session_id: &SessionId,
    seq: u64,
) -> StoreResult<Option<RawEnvelope>> {
    let envelope_json = transaction
        .query_row(
            "SELECT envelope_json FROM events WHERE session_id = ?1 AND seq = ?2",
            params![session_id.as_str(), to_sqlite_integer(seq)?],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(map_sqlite_error)?;
    envelope_json
        .map(|json| {
            serde_json::from_str(&json).map_err(|error| {
                corrupt(format!(
                    "invalid envelope JSON for session {session_id}, seq {seq}: {error}"
                ))
            })
        })
        .transpose()
}

fn opened_menu(opening: &RawEnvelope, menu_id: &MenuId) -> StoreResult<Menu> {
    let payload =
        serde_json::from_value::<EventPayload>(opening.payload.clone()).map_err(|_| {
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
        let json: String = row.get(1).map_err(map_sqlite_error)?;
        let envelope: RawEnvelope = serde_json::from_str(&json).map_err(|error| {
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
            let envelope_json: String = row.get(1).map_err(map_sqlite_error)?;
            let stored_event_id: String = row.get(2).map_err(map_sqlite_error)?;
            let stored_committed_at_ms: i64 = row.get(3).map_err(map_sqlite_error)?;
            let envelope: RawEnvelope = serde_json::from_str(&envelope_json).map_err(|error| {
                corrupt(format!(
                    "invalid envelope JSON for session {session}, seq {stored_seq}: {error}"
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

fn append_envelopes(
    store: &Store,
    envelopes: &mut [RawEnvelope],
    validate_worker_transitions: bool,
) -> StoreResult<CommittedSeqRange> {
    let (session, batch_len) = same_session_batch(envelopes)?;
    let mut connection = store.connection()?;
    // IMMEDIATE makes durable-head validation, sequence allocation, and the
    // batch insert one indivisible write critical section.
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sqlite_error)?;
    if validate_worker_transitions {
        validate_worker_run_transitions(&transaction, &session, envelopes)?;
    }
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
                    session_id, seq, envelope_json, event_id, committed_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .map_err(map_sqlite_error)?;
        for (seq, envelope) in (first_seq..=last_seq).zip(envelopes.iter()) {
            let mut envelope = envelope.clone();
            envelope.seq = seq;
            envelope.committed_at_ms = committed_at_ms;
            let envelope_json = serde_json::to_string(&envelope).map_err(|error| {
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
                    envelope_json,
                    envelope.event_id.as_str(),
                    committed_at_sql,
                ])
                .map_err(map_sqlite_error)?;
            stamped.push(envelope);
        }
    }
    transaction.commit().map_err(map_sqlite_error)?;
    for (envelope, stamped) in envelopes.iter_mut().zip(stamped) {
        *envelope = stamped;
    }
    Ok(CommittedSeqRange {
        session_id: session,
        first_seq,
        last_seq,
    })
}

fn validate_worker_run_transitions(
    transaction: &Transaction<'_>,
    session_id: &SessionId,
    envelopes: &[RawEnvelope],
) -> StoreResult<()> {
    let mut states = latest_run_states(transaction, session_id)?;
    for envelope in envelopes {
        let Some(run_id) = envelope.run_id.as_ref() else {
            continue;
        };
        let payload =
            serde_json::from_value::<EventPayload>(envelope.payload.clone()).map_err(|error| {
                store_error(
                    ErrorCode::InvalidArgument,
                    format!("worker envelope payload is invalid: {error}"),
                    false,
                )
            })?;
        if !states.contains_key(run_id)
            && matches!(
                &payload,
                EventPayload::Item(ItemEvent::Started {
                    item: TurnItem::Extension { kind, .. },
                    ..
                }) if kind == COMPACTION_INTENT_EXTENSION_KIND
            )
        {
            // A compaction intent is the accepted prefix of the daemon's
            // internal job kind. It deliberately has no synthetic user row.
            states.insert(run_id.clone(), (RunState::Compacting, 0));
            continue;
        }
        let Some((durable, _)) = states.get(run_id).cloned() else {
            return Err(store_error(
                ErrorCode::RunNotActive,
                format!("worker run {run_id} has no durable accepted state"),
                false,
            ));
        };
        if durable.is_terminal() {
            return Err(store_error(
                ErrorCode::RunNotActive,
                format!("worker run {run_id} is already terminal"),
                false,
            ));
        }
        if let EventPayload::RunState(next) = payload {
            if durable == RunState::Cancelling && next != RunState::Cancelled {
                return Err(store_error(
                    ErrorCode::RunNotActive,
                    format!("worker run {run_id} is durably cancelling; only Cancelled may follow"),
                    false,
                ));
            }
            states.insert(run_id.clone(), (next, 0));
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

    fn get(&self, artifact: &ArtifactRef) -> StoreResult<Vec<u8>> {
        self.cas.get(artifact)
    }

    fn verify(&self, artifact: &ArtifactRef) -> bool {
        self.cas.verify(artifact)
    }
}

struct StoredDelegation {
    agent_id: String,
    child_session_id: String,
    child_run_id: String,
    parent_session_id: String,
    parent_run_id: String,
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
            parent_run_id, call_id, tool_item_id, parent_agent_id,
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
        call_id: row.get(5)?,
        tool_item_id: row.get(6)?,
        parent_agent_id: row.get(7)?,
        root_session_id: row.get(8)?,
        depth: row.get(9)?,
        task: row.get(10)?,
        prompt: row.get(11)?,
        manifest_json: row.get(12)?,
        state: row.get(13)?,
        report_json: row.get(14)?,
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

fn validate_delegation(record: &DelegationRecord) -> StoreResult<()> {
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
/// (WAL, FULL synchronous, foreign keys, busy timeout).
fn open_connection(path: &Path) -> StoreResult<Connection> {
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
        .pragma_update(None, "synchronous", "FULL")
        .map_err(map_sqlite_error)?;
    Ok(connection)
}

/// Cross-checks the denormalized row columns against the fields embedded in
/// the envelope JSON. Any disagreement means the journal was tampered with or
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
fn map_sqlite_error(error: SqliteError) -> HaiderError {
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

fn corrupt(message: impl Into<String>) -> HaiderError {
    store_error(ErrorCode::StoreCorrupt, message, false)
}
