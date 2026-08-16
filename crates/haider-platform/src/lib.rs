//! Operating-system seams shared by Haider's client, daemon, and tools.
//!
//! Protocol framing deliberately stays outside this crate: [`IpcStream`]
//! transports the exact bytes its caller supplies. On Unix, endpoint ownership,
//! daemon detachment, descriptor hygiene, process signalling, and shutdown
//! delivery preserve the pre-abstraction implementations exactly.

mod directory;
mod fs;
mod ipc;
mod process;
mod shutdown;
mod spawn;
mod system;
mod user;

pub use directory::{
    WorkspaceDirectory, WorkspaceDirectoryError, duplicate_workspace_directory,
    open_workspace_directory,
};
pub use fs::{
    configure_directory_mode, configure_file_mode, metadata_is_current_user, metadata_link_count,
    metadata_mode, replace_file, set_mode, sync_directory,
};
pub use ipc::{
    BoundEndpoint, Endpoint, EndpointAddress, EndpointError, IpcReadHalf, IpcStream, IpcWriteHalf,
    PeerCredentials, connect, peer_credentials, peer_credentials_are_owner, peer_is_owner, split,
    write_immediate,
};
pub use process::{
    ProcessGroup, ProcessId, ProcessSignal, configure_background_process, configure_process_group,
    exit_signal, kill_process_tree, process_error_is_missing, process_error_is_permission,
    process_group, process_group_exists, process_id, process_leader_exited, signal_process,
    signal_process_group, signal_process_group_id,
};
pub use shutdown::{ShutdownInstallError, ShutdownSignal, ShutdownSignals, shutdown_signal};
pub use spawn::{DaemonSpawn, DaemonSpawnError, spawn_daemon};
pub use system::local_device_name;
pub use user::{effective_user_id, is_owner_private_directory};

/// Crate marker used by the workspace self-test.
pub const CRATE_NAME: &str = "haider-platform";
