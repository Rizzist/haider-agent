//! Permissioned tool execution for the Haider Code harness.
//!
//! [`EffectBroker`] is the only route from a normalized operation to dispatch:
//! it binds approvals to canonical argument digests and journals the protocol's
//! four effect phases. Filesystem write attribution is retained in
//! [`ChangeLedger`] for the later verification gate.

mod broker;
mod checkpoint;
#[cfg(test)]
#[path = "checkpoint_tests.rs"]
mod checkpoint_tests;
mod computer;
mod error;
mod filesystem;
mod graph_evidence;
mod ledger;
mod message_subagent;
mod mobile;
mod monitor;
mod plan;
mod process;
mod redact;
mod repo;
mod request_input;
mod shell;
mod spawn_subagent;
mod tasks;
mod todo_write;
mod webfetch;
mod workflow_author;

/// Default byte ceiling for a complete tool result to remain inline. A result
/// that crosses this boundary is frozen in CAS and represented by a bounded
/// preview plus its artifact reference.
pub const TOOL_RESULT_INLINE_MAX_BYTES: usize = 8 * 1024;

pub use broker::{
    ALLOW_SCREEN_CONTROL_SESSION_GRANT, ALLOW_SCREEN_SESSION_GRANT, AlwaysAllowRule, EffectBroker,
    EffectBrokerCloseError, EffectBrokerCloseReport, EffectOperation, JournalSink,
    PermissionPolicy, PolicyDecision, SessionGrant, SessionGrantScope,
};
pub use checkpoint::{
    CheckpointCapture, CheckpointCapturePath, CheckpointRestoreError, CheckpointRestorePlan,
    CheckpointRestoreTarget, FreezeCheckpointInput, freeze_checkpoint, restore_checkpoint_plan,
    verify_checkpoint_restore_plan,
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
    CasSink, FsCaseMode, FsEdit, FsEditChange, FsFileGlob, FsGlob, FsPath, FsPathOperation, FsRead,
    FsSearch, FsSearchContext, FsSearchMode, FsWrite, GLOB_ENTRY_LIMIT, GLOB_MAX_FILES_SCANNED,
    GLOB_PATTERN_MAX_BYTES, ResultBounds, SEARCH_BINARY_SNIFF_BYTES, SEARCH_MAX_ENUMERATED_FILES,
    SEARCH_MAX_FILES, SEARCH_MAX_LINE_BYTES, SEARCH_MAX_RESULT_BYTES, SEARCH_MAX_SCANNED_BYTES,
    SEARCH_PATTERN_MAX_BYTES, SEARCH_PREVIEW_MATCHES, SEARCH_REGEX_DFA_SIZE_LIMIT,
    SEARCH_REGEX_NEST_LIMIT, SEARCH_REGEX_PATTERN_MAX_BYTES, SEARCH_REGEX_SIZE_LIMIT,
    SEARCH_RESULT_ACCOUNTING_OVERHEAD, SEARCH_SIMPLE_PATTERN_MAX_BYTES,
    SEARCH_STRUCTURED_LINE_BYTES, SEARCH_WALL_TIME_BUDGET, TurnAttribution, fs_edit_manifest,
    fs_glob_manifest, fs_path_manifest, fs_read_manifest, fs_search_manifest, fs_write_manifest,
};
pub use graph_evidence::{GraphEvidence, graph_evidence_manifest};
pub use haider_protocol::computer::{ComputerAction, ScreenPoint, ScrollDirection};
pub use haider_protocol::mobile::{
    A11yNode, AppEntry, MobileAction, MobileKey, MobileOutput, MobilePermission, Point, Point4,
    SmsMessage,
};
pub use ledger::{ChangeLedger, ChangeLedgerSink, FsWriteRecord, TurnChanges};
pub use message_subagent::{MessageSubagent, message_subagent_manifest};
pub use mobile::{
    FakeMobileBackend, MobileBackend, MobileCancelToken, MobileError, MobileOperation,
    MobileResult, UnavailableMobileBackend, mobile_manifest, platform_mobile_backend,
};
pub use monitor::{
    MAX_MONITOR_FILTER_CHARS, MAX_MONITOR_FOLLOW_UP_CHARS, MAX_MONITOR_ID_CHARS, MonitorAction,
    MonitorFilter, MonitorFilterField, MonitorFilterOperator, MonitorLifetime, MonitorOccurrence,
    MonitorRequest, MonitorSource, MonitorSourceKind, monitor_manifest,
};
pub use plan::{
    PLAN_BODY_MAX_BYTES, PLAN_DECISION_ACCEPT, PLAN_ORIGIN, PLAN_TITLE_MAX_BYTES, Plan, PlanResult,
};
pub use process::{
    CommandOutputSink, NoopCommandOutputSink, PROCESS_ADAPTER_INPUT_BYTES,
    PROCESS_MAX_OUTPUT_BYTES, PROCESS_OUTPUT_CHUNK_BYTES, ProcessBounds, ProcessCancelHandle,
    ProcessControl, ProcessControlAction, ProcessControlResult, ProcessExec, ProcessExecution,
    ProcessLifecycleEvent, ProcessLimit, ProcessOutputChunk, ProcessResult, ProcessSignal,
    workspace_state_digest,
};
pub use redact::redact_lockdown_text;
pub use request_input::{RequestInput, RequestInputAnswer, RequestInputKind, RequestInputOption};
pub use shell::{
    BuiltinResult, ComposerSubmission, EnvViewEntry, OutputAdapter, REDACTED_ENV_VALUE,
    REDUCED_TOOL_OUTPUT_MAX_BYTES, ReducedToolOutput, ShellSession, UserProcessExec,
    estimated_tokens, reduce_tool_output,
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
