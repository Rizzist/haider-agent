//! Durable event journal and content-addressed artifact storage.
//!
//! [`EventStore`] is the durability boundary: an envelope is committed only
//! after [`EventStore::append`] returns successfully. Callers must publish
//! committed envelopes after that boundary. [`Cas`] provides the corresponding
//! durability boundary for opaque artifact bytes.
//!
//! Each module owns one invariant; see the module headers for the details:
//!
//! - `event_store` — per-session sequences are allocated at commit time,
//!   monotonic and gap-free; replay is byte-identical to what was appended.
//! - `cas` — artifacts are addressed `blake3:<64 lowercase hex>`, published
//!   without replacement via a hard link, and re-hashed on read.
//! - `profile_lock` — at most one process has a profile root open.
//! - `migrations` — the schema changes only through the numbered registry;
//!   profile metadata durably fences worker generations across opens.

mod cas;
mod event_store;
mod migrations;
mod profile_lock;

pub use cas::{FileCas, validate_image_block};
pub use event_store::{
    ACCOUNT_REMOVE_METHOD, ACCOUNT_SET_ACTIVE_METHOD, ACCOUNT_SET_DEFAULT_MODEL_METHOD,
    AbandonedGraph, AcceptedRunRetry, AcceptedShellExec, AcceptedTurn, AccountAddClaim,
    AccountAddReceiptResponse, AccountAddReceiptRow, AccountRemoveReceiptRow, AttachedChildGraph,
    BranchCreateCommand, BranchCreateOutcome, CachedModels, CancelledTurn, ChildGraphAttachCommand,
    ChildGraphAttachOutcome, ChildTemplateCacheEntry, ChildTemplateObservation,
    ChildTemplateObservationCommand, CommittedSeqRange, ComputerEvidenceCommand,
    ComputerEvidenceOutcome, ContextCompactionClaim, ContextCompactionReceiptResponse,
    CreatedBranch, CreatedSession, DelegationCreateOutcome, DelegationDescendant,
    DelegationDescendants, DelegationRecord, DelegationState, EventStore, GraphAbandonCommand,
    GraphAbandonOutcome, GraphEvidenceCommand, GraphEvidenceOutcome, GraphFinalizationCommand,
    GraphFinalizationOutcome, GraphInspectResult, GraphPinCommand, GraphPinOutcome,
    GraphRunSetOpenCommand, GraphRunSetOpenOutcome, GraphSwitchCommand, GraphSwitchOutcome,
    HookTrustChange, HookTrustCommand, JournalAppendBatch, LoginClaim, LoginReceiptFailure,
    LoginReceiptResponse, LoginReceiptRow, ManagementClaim, ManagementReceiptRow,
    MenuResolutionCommand, MenuResolutionOutcome, OpenedGraphRunSet, OpenedTodoGraph,
    PROVIDER_CONFIGURE_METHOD, PROVIDER_REMOVE_METHOD, PinnedGraph, ProcessSignalCommand,
    ProcessSignalOutcome, ProfileLease, RecordedGraphEvidence, RecordedProcessSignal,
    RenamedSession, RunRetryCommand, RunRetryOutcome, SUBAGENT_LIVE_LIMIT, SeenSession,
    SelectedAgentType, SelectedEffort, SelectedFast, SelectedModel, SessionCreateCommand,
    SessionCreateOutcome, SessionRenameCommand, SessionRenameOutcome, SessionSeenCommand,
    SessionSeenOutcome, SessionSelectAgentTypeCommand, SessionSelectAgentTypeOutcome,
    SessionSelectEffortCommand, SessionSelectEffortOutcome, SessionSelectFastCommand,
    SessionSelectFastOutcome, SessionSelectModelCommand, SessionSelectModelOutcome,
    ShellExecAcceptCommand, ShellExecAcceptOutcome, Store, SwitchedGraph, TurnAcceptCommand,
    TurnAcceptOutcome, TurnAdmissionDisposition, TurnCancelCommand, TurnCancelOutcome,
    TurnCancellationStatus,
};
pub use haider_protocol::error::{ErrorCode, HaiderError};

use haider_protocol::ids::ArtifactRef;
use haider_protocol::tool::ImageBlockRef;

/// Crate marker used by the workspace self-test.
pub const CRATE_NAME: &str = "haider-store";

/// Result type used by both durability ports.
pub type StoreResult<T> = Result<T, HaiderError>;

/// Content-addressed storage for immutable artifact bytes.
pub trait Cas: Send + Sync {
    /// Durably writes bytes and returns their BLAKE3 content address.
    fn put(&self, bytes: &[u8]) -> StoreResult<ArtifactRef>;

    /// Durably streams a file into the CAS without loading it as one buffer.
    fn put_file(&self, path: &std::path::Path) -> StoreResult<ArtifactRef>;

    /// Validates, bounds, and durably stores one PNG/JPEG image.
    fn put_image(&self, _bytes: &[u8], _media_type: &str) -> StoreResult<ImageBlockRef> {
        Err(HaiderError::new(
            ErrorCode::InvalidArgument,
            "this CAS implementation does not admit tool images",
            false,
        ))
    }

    /// Reads and verifies bytes. Hash mismatch is reported as store corruption.
    fn get(&self, artifact: &ArtifactRef) -> StoreResult<Vec<u8>>;

    /// Returns whether the artifact exists and its bytes match its address.
    fn verify(&self, artifact: &ArtifactRef) -> bool;
}

/// Crate-local shorthand for building a [`HaiderError`].
pub(crate) fn store_error(
    code: ErrorCode,
    message: impl Into<String>,
    retryable: bool,
) -> HaiderError {
    HaiderError::new(code, message, retryable)
}

/// Current wall-clock time as Unix milliseconds.
pub(crate) fn now_ms() -> StoreResult<u64> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| {
            store_error(
                ErrorCode::Internal,
                format!("system clock is before Unix epoch: {error}"),
                false,
            )
        })?;
    u64::try_from(duration.as_millis()).map_err(|_| {
        store_error(
            ErrorCode::Internal,
            "current timestamp does not fit u64",
            false,
        )
    })
}

/// Narrows a u64 to SQLite's signed 64-bit INTEGER column type.
pub(crate) fn to_sqlite_integer(value: u64) -> StoreResult<i64> {
    i64::try_from(value).map_err(|_| {
        store_error(
            ErrorCode::InvalidArgument,
            format!("value {value} does not fit SQLite INTEGER"),
            false,
        )
    })
}
