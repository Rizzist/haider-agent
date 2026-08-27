//! Shared client seam for the Haider daemon (report §6.2, R8/R9).
//!
//! Five cooperating pieces, deliberately below both binaries:
//!
//! - [`profile`] — the ONE profile resolver `haider` and `haiderd` share
//!   (store dir, profile id, runtime dir, endpoint path, release-owned
//!   defaults). `haider-daemon` delegates its endpoint derivation here so
//!   client and daemon can never disagree about the rendezvous path.
//! - [`client`] — the reusable UDS [`client::RpcClient`]: pending-request
//!   correlation, bounded writer/reader, R9 heartbeat, and reconnect
//!   primitives (typed disconnect reasons; the caller redials).
//! - [`spawn`] — bare-`haider` auto-spawn: connect first, spawn a detached
//!   sibling `haiderd` only on missing/refused endpoint, handshake-poll to a
//!   deadline, and report version/feature skew without ever killing a live
//!   daemon.
//! - [`headless`] — the reusable daemon-backed one-shot transaction used by
//!   non-interactive clients.
//! - [`surface`] — typed volatile input/status publication, watch, and
//!   injection helpers shared by TUI and embedding composers.
//!
//! This crate owns no TUI, daemon lifecycle, or persistence.

pub mod checkpoint;
pub mod client;
pub mod graph;
pub mod headless;
pub mod observe;
pub mod permission;
pub mod profile;
pub mod shell;
pub mod spawn;
pub mod surface;
pub mod transcription;
pub mod workflow_graph;
pub mod workflow_graph_rpc;

pub use checkpoint::{CheckpointClientError, checkpoints, redo, rollback_turn, undo};
pub use client::{
    ClientConfig, ClientError, ConnectError, Connected, ConnectionState, DisconnectReason,
    MenuAnswerRequest, PING_INTERVAL, PONG_DEADLINE, PeerCredentials, PendingResponse, RpcClient,
    connect,
};
pub use graph::{
    GraphAbandonResult, GraphClientError, GraphInspectPage, GraphPinResult, GraphRunSetOpenResult,
    GraphSwitchResult, graph_abandon, graph_inspect, graph_pin, graph_pin_template,
    graph_pin_template_fenced, graph_run_set_open, graph_status, graph_switch, graph_switch_fenced,
    workflow_instance,
};
pub use haider_rpc::{WorkflowInstanceSourceV1, WorkflowInstanceV1};
pub use headless::{
    DEFAULT_TERMINAL_GRACE, ERROR_CODE_NO_ACTIVE_ACCOUNT, ERROR_CODE_NO_DEFAULT_MODEL,
    HeadlessAttachment, HeadlessBackgroundTask, HeadlessBlockingReason, HeadlessEvent,
    HeadlessFailureCode, HeadlessFileAttachment, HeadlessImageAttachment, HeadlessOutcome,
    HeadlessPdfAttachment, HeadlessPermissionDenial, HeadlessRunError, HeadlessRunEventReader,
    HeadlessRunEvents, HeadlessRunFailure, HeadlessRunRequest, HeadlessRunResult,
    HeadlessRunStatus, HeadlessRunStopResult, HeadlessSessionConfig, headless_run_events,
    headless_run_status, load_attachment, load_image_attachment, load_pdf_attachment,
    load_text_attachment, required_headless_features, required_headless_features_with_attachments,
    required_headless_features_with_hook_trust, run_headless, run_headless_with_session_config,
    stop_headless_run,
};
pub use observe::{
    DescendantLiveAttachment, DescendantView, ObserveClient, ObserveError, observe_stream_all,
    observe_stream_session, observe_stream_session_after,
};
pub use permission::{
    ComputerPermissionClientError, open_permission_settings, open_permission_settings_request,
    restart_daemon_for_permission,
};
pub use profile::{
    DEFAULT_MAX_TOKENS, DEFAULT_PROVIDER, MODEL_ENV, PACKAGED_DEFAULT_MODEL, PROFILE_CONFIG_FILE,
    PROFILE_DIR_ENV, ProfileEnv, ProfileError, RUNTIME_DIR_ENV, ResolvedProfile, effective_uid,
    endpoint_path_for, resolve_default_model_for, resolve_profile,
};
pub use shell::{
    AcceptedShellExec, CancelledShellExec, ShellExecError, ShellExecRequest, cancel_shell_exec,
    required_user_command_features, shell_exec,
};
pub use spawn::{
    DAEMON_LOG_FILE, DaemonLifetime, DaemonOwnershipToken, EnsureError, EnsureOptions,
    EnsuredDaemon, RACE_LOSER_EXIT_CODE, STARTUP_DEADLINE, ensure_daemon, required_live_features,
    signal_authenticated_peer, spawn_daemon_retained,
};
pub use surface::{
    SurfaceClientError, SurfaceInjectAck, SurfaceInjectOp, SurfaceInputPublishWire,
    SurfaceInputWire, SurfacePublishAck, SurfaceStatusPublishWire, SurfaceStatusWire,
    SurfaceWatchSnapshot, input_inject_ack, input_inject_request, session_input_inject,
    session_surface_publish, session_surface_watch, surface_publish_ack, surface_publish_request,
    surface_watch_request, surface_watch_snapshot,
};
pub use workflow_graph::{
    WorkflowEvidenceRef, WorkflowGraphChange, WorkflowGraphEdge, WorkflowGraphEdgeKind,
    WorkflowGraphProjection, WorkflowGraphProjectionError, WorkflowGraphState,
    WorkflowGraphWatchPage, WorkflowNodeProjection, WorkflowNodeRejection, WorkflowNodeState,
};
pub use workflow_graph_rpc::{WorkflowGraphRpcAdapter, WorkflowGraphRpcAdapterError};

/// Crate marker used by the workspace self-test.
pub const CRATE_NAME: &str = "haider-client";

#[cfg(test)]
mod shell_tests;
