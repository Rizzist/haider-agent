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
pub mod lockdown;
pub mod observe;
pub mod peer;
pub mod permission;
pub mod profile;
pub mod session_fork;
pub mod shell;
pub mod shell_registry;
pub mod spawn;
pub mod ssh_profiles;
pub mod stop_receipt;
pub mod surface;
pub mod transcription;
pub mod workflow_graph;
pub mod workflow_graph_rpc;

pub use checkpoint::{CheckpointClientError, checkpoints, redo, rollback_turn, undo};
pub use client::{
    ClientCloseOutcome, ClientConfig, ClientError, ConnectError, Connected, ConnectionState,
    ConnectionUsage, DisconnectReason, MenuAnswerRequest, PING_INTERVAL, PONG_DEADLINE,
    PeerCredentials, PendingResponse, RpcClient, connect,
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
    HEADLESS_EVENT_MEMORY_THRESHOLD_BYTES, HeadlessAttachment, HeadlessBackgroundTask,
    HeadlessBlockingReason, HeadlessEvent, HeadlessEventMode, HeadlessFailureCode,
    HeadlessFileAttachment, HeadlessImageAttachment, HeadlessOutcome, HeadlessPdfAttachment,
    HeadlessPermissionDenial, HeadlessRunError, HeadlessRunEventReader, HeadlessRunEvents,
    HeadlessRunFailure, HeadlessRunRequest, HeadlessRunResult, HeadlessRunStatus,
    HeadlessRunStopResult, HeadlessSessionConfig, HeadlessTerminalEvent, HeadlessTerminalKind,
    headless_run_events, headless_run_status, load_attachment, load_image_attachment,
    load_pdf_attachment, load_text_attachment, required_headless_features,
    required_headless_features_with_attachments, required_headless_features_with_hook_trust,
    run_headless, run_headless_with_session_config,
    run_headless_with_session_config_and_event_mode, stop_headless_run,
};
pub use lockdown::{
    LockdownClientError, ProviderLockdown, lockdown_set_quota_response, lockdown_status_response,
    provider_lockdown, provider_lockdown_available, provider_set_trust_response,
};
pub use observe::{
    DescendantLiveAttachment, DescendantView, ObserveClient, ObserveError, ObserveStatusSnapshot,
    SessionReadinessSnapshot, SessionResumeSnapshot, observe_stream_all, observe_stream_session,
    observe_stream_session_after, wait_for_session_resume, wait_for_sessions_ready,
};
pub use peer::{
    PeerClientError, PeerDelivery, PeerDeliveryReason, PeerDescriptor, PeerEvent,
    PeerEventSubscription, PeerKind, PeerMessage, PeerMessaging, PeerReceipt, PeerSender,
    PeerState, PeerTrust, peer_event_from_frame, peer_list_response, peer_messaging,
    peer_messaging_available, peer_name_response, peer_send_response,
};
pub use permission::{
    ComputerPermissionClientError, open_permission_settings, open_permission_settings_request,
    restart_daemon_for_permission,
};
pub use profile::{
    DEFAULT_MAX_TOKENS, DEFAULT_PROVIDER, MODEL_ENV, PACKAGED_DEFAULT_MODEL, PROFILE_CONFIG_FILE,
    PROFILE_DIR_ENV, ProfileEnv, ProfileError, RUNTIME_DIR_ENV, ResolvedProfile,
    canonicalize_path_allow_missing, effective_uid, endpoint_path_for, resolve_default_model_for,
    resolve_profile, resolve_profile_read_only,
};
pub use session_fork::{
    FORKABLE_PROMPT_PAGE, ForkablePrompt, PromptFork, SessionForkClientError, fork_at_prompt,
    forkable_prompts, forkable_prompts_in, prompt_fork_available, prompt_fork_response,
};
pub use shell::{
    AcceptedShellExec, CancelledShellExec, ShellExecError, ShellExecRequest, cancel_shell_exec,
    required_user_command_features, shell_exec,
};
pub use shell_registry::{
    ShellEvent, ShellEventSubscription, ShellRegistry, ShellRegistryClientError,
    shell_event_from_frame, shell_registry, shell_registry_available,
};
pub use spawn::{
    DAEMON_LOG_FILE, DaemonLifetime, DaemonOwnershipToken, EnsureError, EnsureOptions,
    EnsuredDaemon, RACE_LOSER_EXIT_CODE, STARTUP_DEADLINE, ensure_daemon, required_live_features,
    signal_authenticated_peer, spawn_daemon_retained,
};
pub use ssh_profiles::{
    SshProfiles, SshProfilesClientError, ssh_list_response, ssh_profiles, ssh_profiles_available,
};
pub use stop_receipt::{
    DAEMON_STOP_CLIENT_NAME, DAEMON_STOP_COMPLETION_SCHEMA, DaemonStopCompletion,
    DaemonStopReceipt, daemon_stop_receipt_path,
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
#[path = "lockdown_tests.rs"]
mod lockdown_tests;
#[cfg(test)]
mod peer_tests;
#[cfg(test)]
#[path = "session_fork_tests.rs"]
mod session_fork_tests;
#[cfg(test)]
#[path = "shell_registry_tests.rs"]
mod shell_registry_tests;
#[cfg(test)]
mod shell_tests;
#[cfg(test)]
#[path = "ssh_profiles_tests.rs"]
mod ssh_profiles_tests;
