//! Operating-system seams shared by Haider's client, daemon, and tools.
//!
//! Protocol framing deliberately stays outside this crate: [`IpcStream`]
//! transports the exact bytes its caller supplies. On Unix, endpoint ownership,
//! daemon detachment, descriptor hygiene, process signalling, and shutdown
//! delivery preserve the pre-abstraction implementations exactly.

mod directory;
pub mod fs;
mod ipc;
mod process;
mod shutdown;
mod spawn;
mod system;
mod user;

#[cfg(windows)]
pub use directory::{
    WindowsFileIdentity, open_workspace_subdirectory, windows_file_identity,
    workspace_directory_contains_identity, workspace_directory_identity,
};
pub use directory::{
    WorkspaceDirectory, WorkspaceDirectoryError, duplicate_workspace_directory,
    open_workspace_directory,
};
#[cfg(windows)]
pub use fs::replace_file_with_backup;
pub use fs::{
    SyncPolicy, configure_directory_mode, configure_file_mode, metadata_is_current_user,
    metadata_link_count, metadata_mode, replace_file, set_mode, sync_file,
};
pub use ipc::{
    BoundEndpoint, Endpoint, EndpointAddress, EndpointError, IpcReadHalf, IpcShutdown,
    IpcShutdownOutcome, IpcStream, IpcWriteHalf, OwnedDirectoryInspection, OwnedDirectoryPathState,
    OwnedDirectoryReceipt, OwnedDirectoryRemoval, PeerCredentials, PeerExitReason, PeerExitWatcher,
    PreparedRuntimeDirectory, connect, daemon_temp_directory_path, inspect_owned_directory,
    owned_directory_path_state, peer_credentials, peer_credentials_and_exit_watcher,
    peer_credentials_are_owner, peer_is_owner, prepare_runtime_directory,
    prepare_runtime_directory_with_temp, remove_owned_empty_directory, retained_file_link_count,
    shutdown_handle, split, sweep_stale_endpoints, write_immediate,
};
pub use process::program_on_path;
pub use process::{
    ProcessGroup, ProcessId, ProcessSignal, configure_background_process,
    configure_process_environment, configure_process_group, exit_signal, kill_process_tree,
    process_error_is_missing, process_error_is_permission, process_group, process_group_exists,
    process_id, process_leader_exited, register_process_group, release_process_group,
    signal_process, signal_process_group, signal_process_group_id, wait_for_child_exit,
};
#[cfg(windows)]
pub use process::{windows_command_interpreter, windows_powershell};
pub use shutdown::{ShutdownInstallError, ShutdownSignal, ShutdownSignals, shutdown_signal};
pub use spawn::{
    DAEMON_LIVENESS_ARG, DAEMON_LOG_DIRECTORY, DAEMON_LOG_FILE, DAEMON_LOG_PATH_ENV,
    DAEMON_LOG_RETENTION, DAEMON_READINESS_ARG, DaemonLivenessGuard, DaemonLivenessWatcher,
    DaemonReadiness, DaemonReadyNotifier, DaemonSpawn, DaemonSpawnError, SpawnedDaemon,
    allocate_daemon_log_path, publish_active_daemon_log, spawn_daemon,
    spawn_daemon_with_piped_stderr, spawn_daemon_with_readiness,
    spawn_daemon_with_readiness_and_liveness,
};
pub use system::local_device_name;
pub use user::{effective_user_id, is_owner_private_directory, owner_scoped_runtime_directory};

/// Compatibility boundary for unchanged callers; new side-file code should
/// select its durability contract through [`fs::sync_directory`].
pub fn sync_directory(path: &std::path::Path) -> std::io::Result<()> {
    fs::sync_directory(path, SyncPolicy::Full)
}

/// Crate marker used by the workspace self-test.
pub const CRATE_NAME: &str = "haider-platform";
