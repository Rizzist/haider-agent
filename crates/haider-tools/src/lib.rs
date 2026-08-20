//! Permissioned tool execution for the Haider Code harness.
//!
//! [`EffectBroker`] is the only route from a normalized operation to dispatch:
//! it binds approvals to canonical argument digests and journals the protocol's
//! four effect phases. Filesystem write attribution is retained in
//! [`ChangeLedger`] for the later verification gate.

mod broker;
mod computer;
mod error;
mod filesystem;
mod graph_evidence;
mod ledger;
mod message_subagent;
mod plan;
mod process;
mod request_input;
mod shell;
mod spawn_subagent;
mod tasks;
mod todo_write;
mod webfetch;
mod workflow_author;

pub use broker::{
    ALLOW_SCREEN_CONTROL_SESSION_GRANT, ALLOW_SCREEN_SESSION_GRANT, AlwaysAllowRule, EffectBroker,
    EffectBrokerCloseError, EffectBrokerCloseReport, EffectOperation, JournalSink,
    PermissionPolicy, PolicyDecision, SessionGrant, SessionGrantScope,
};
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
pub use computer::ExcludeRegionScreenshotRedaction;
pub use computer::{
    ComputerBackend, ComputerCancelToken, ComputerError, ComputerInspection,
    ComputerInspectionBounds, ComputerOperation, ComputerOutput, ComputerPermissionPoll,
    ComputerResult, PassthroughScreenshotRedaction, ScreenshotRedactionPolicy,
    ScreenshotRedactionRegion, UnavailableComputerBackend, computer_manifest,
    configured_screenshot_redaction_policy, open_system_permission_settings,
    platform_computer_backend,
};
pub use error::{FsEditAnchorMismatch, ToolError, ToolResult};
pub use filesystem::{
    CasSink, FsCaseMode, FsEdit, FsEditChange, FsGlob, FsPath, FsPathOperation, FsRead, FsSearch,
    FsSearchMode, FsWrite, ResultBounds, TurnAttribution, fs_edit_manifest, fs_glob_manifest,
    fs_path_manifest, fs_read_manifest, fs_search_manifest, fs_write_manifest,
};
pub use graph_evidence::{GraphEvidence, graph_evidence_manifest};
pub use haider_protocol::computer::{ComputerAction, ScreenPoint, ScrollDirection};
pub use ledger::{ChangeLedger, ChangeLedgerSink, FsWriteRecord, TurnChanges};
pub use message_subagent::{MessageSubagent, message_subagent_manifest};
pub use plan::{
    PLAN_BODY_MAX_BYTES, PLAN_DECISION_ACCEPT, PLAN_DECISION_REJECT, PLAN_DECISION_REVISE,
    PLAN_ORIGIN, PLAN_TITLE_MAX_BYTES, Plan,
};
pub use process::{
    CommandOutputSink, NoopCommandOutputSink, PROCESS_OUTPUT_CHUNK_BYTES, ProcessBounds,
    ProcessCancelHandle, ProcessControl, ProcessControlAction, ProcessControlResult, ProcessExec,
    ProcessExecution, ProcessLifecycleEvent, ProcessLimit, ProcessOutputChunk, ProcessResult,
    ProcessSignal, workspace_state_digest,
};
pub use request_input::{RequestInput, RequestInputAnswer, RequestInputKind, RequestInputOption};
pub use shell::{
    BuiltinResult, ComposerSubmission, EnvViewEntry, REDACTED_ENV_VALUE, ShellSession,
    UserProcessExec,
};
pub use spawn_subagent::{SpawnSubagent, spawn_subagent_manifest};
pub use tasks::{
    BACKGROUND_KILL_GRACE, BackgroundExec, BackgroundExitStatus, BackgroundSpawn,
    EvidencePidLiveness, MAX_TASK_NAME_CHARS, OrphanReap, PidLiveness, SharedTaskOutput,
    TaskKillHandle, TaskOutputBuffer, default_task_name, lock_task_output, probe_group_liveness,
    probe_group_liveness_evidence, reap_orphan_group, shared_task_output, supervise_background,
    task_kill_channel, task_kill_manifest, task_output_manifest,
};
pub use todo_write::{MAX_TODO_ITEMS, MAX_TODO_TEXT_CHARS, TodoWrite, todo_write_manifest};
pub use webfetch::{
    WEB_FETCH_TOOL_OUTPUT_CAP_BYTES, WebFetch, web_fetch_manifest, web_search_manifest,
};
pub use workflow_author::{WorkflowAuthor, workflow_author_manifest};

/// Crate marker used by the workspace self-test.
pub const CRATE_NAME: &str = "haider-tools";
