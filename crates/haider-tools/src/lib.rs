//! Permissioned tool execution for the Haider Code harness.
//!
//! [`EffectBroker`] is the only route from a normalized operation to dispatch:
//! it binds approvals to canonical argument digests and journals the protocol's
//! four effect phases. Filesystem write attribution is retained in
//! [`ChangeLedger`] for the later verification gate.

mod broker;
mod error;
mod filesystem;
mod ledger;
mod process;
mod request_input;
mod shell;

pub use broker::{
    AlwaysAllowRule, EffectBroker, EffectBrokerCloseError, EffectBrokerCloseReport,
    EffectOperation, JournalSink, PermissionPolicy, PolicyDecision,
};
pub use error::{FsPatchConflict, ToolError, ToolResult};
pub use filesystem::{
    CasSink, FsList, FsPatch, FsRead, FsSearch, FsWrite, ResultBounds, TurnAttribution,
};
pub use ledger::{ChangeLedger, ChangeLedgerSink, FsWriteRecord, TurnChanges};
pub use process::{
    CommandOutputSink, NoopCommandOutputSink, PROCESS_OUTPUT_CHUNK_BYTES, ProcessBounds,
    ProcessControl, ProcessControlAction, ProcessControlResult, ProcessExec, ProcessExecution,
    ProcessLifecycleEvent, ProcessOutputChunk, ProcessResult, ProcessSignal,
};
pub use request_input::{RequestInput, RequestInputAnswer, RequestInputKind, RequestInputOption};
pub use shell::{
    BuiltinResult, ComposerSubmission, EnvViewEntry, REDACTED_ENV_VALUE, ShellSession,
    UserProcessExec,
};

/// Crate marker used by the workspace self-test.
pub const CRATE_NAME: &str = "haider-tools";
