use std::path::{Path, PathBuf};

/// A descriptor-backed receipt for one directory this process created.
///
/// The retained descriptor/handle keeps the created filesystem identity live,
/// so cleanup never has to infer ownership from a path or from a reusable
/// numeric identity alone.
#[derive(Debug)]
pub struct OwnedDirectoryReceipt {
    path: PathBuf,
    #[cfg(unix)]
    anchor: rustix::fd::OwnedFd,
    #[cfg(windows)]
    anchor: Option<std::fs::File>,
}

impl OwnedDirectoryReceipt {
    #[cfg(unix)]
    pub(super) fn new(path: PathBuf, anchor: rustix::fd::OwnedFd) -> Self {
        Self { path, anchor }
    }

    #[cfg(windows)]
    pub(super) fn new(path: PathBuf, anchor: std::fs::File) -> Self {
        Self {
            path,
            anchor: Some(anchor),
        }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Result of attempting to retire an empty directory through its ownership
/// receipt. `ReplacementPreserved` means the public pathname no longer named
/// the created directory and no replacement was removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnedDirectoryRemoval {
    Removed,
    /// Windows accepted a delete request for the exact handle; namespace
    /// retirement remains pending until all sharing handles close.
    RemovalRequested,
    AlreadyAbsent,
    ReplacementPreserved,
    /// The retained object is still linked, but no longer at the recorded
    /// coordinate. Cleanup cannot locate it without guessing.
    CoordinateLost,
    NotEmpty,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnedDirectoryPathState {
    Owned,
    OwnedObjectUnlinked,
    ReplacementPreserved,
    CoordinateLost,
}

/// A listing obtained through the retained handle for the exact directory
/// this process created, rather than through its reusable pathname.
#[derive(Debug, PartialEq, Eq)]
pub enum OwnedDirectoryInspection {
    Entries(Vec<PathBuf>),
    OwnedObjectUnlinked,
}

/// Exact runtime directories created by this preparation call.
///
/// Pre-existing runtime and `tmp` directories are validated and secured but
/// are not reported as owned, so cleanup never infers ownership from a layout
/// basename alone.
#[derive(Debug)]
pub struct PreparedRuntimeDirectory {
    created_directories: Vec<OwnedDirectoryReceipt>,
}

impl PreparedRuntimeDirectory {
    pub(super) fn new(created_directories: Vec<OwnedDirectoryReceipt>) -> Self {
        Self {
            created_directories,
        }
    }

    #[must_use]
    pub fn into_created_directories(self) -> Vec<OwnedDirectoryReceipt> {
        self.created_directories
    }
}

/// Atomically claims and removes the exact empty directory represented by a
/// retained ownership receipt. The platform implementation never removes a
/// path replacement, even when it appears between inspection and cleanup.
pub fn remove_owned_empty_directory(
    receipt: &mut OwnedDirectoryReceipt,
) -> std::io::Result<OwnedDirectoryRemoval> {
    #[cfg(unix)]
    {
        unix::remove_owned_empty_directory(receipt)
    }
    #[cfg(windows)]
    {
        windows::remove_owned_empty_directory(receipt)
    }
}

/// Compares the current pathname with the retained directory handle and also
/// distinguishes a proven unlink from a still-linked object moved elsewhere.
pub fn owned_directory_path_state(
    receipt: &OwnedDirectoryReceipt,
) -> std::io::Result<OwnedDirectoryPathState> {
    #[cfg(unix)]
    {
        unix::owned_directory_path_state(receipt)
    }
    #[cfg(windows)]
    {
        windows::owned_directory_path_state(receipt)
    }
}

/// Lists the exact created directory through its retained descriptor/handle.
/// A concurrent path replacement can therefore neither lend its entries to
/// the ownership decision nor hide entries in the owned object.
pub fn inspect_owned_directory(
    receipt: &OwnedDirectoryReceipt,
) -> std::io::Result<OwnedDirectoryInspection> {
    #[cfg(unix)]
    {
        unix::inspect_owned_directory(receipt)
    }
    #[cfg(windows)]
    {
        windows::inspect_owned_directory(receipt)
    }
}

/// Returns the hard-link count observed through an already-open file handle.
/// Cleanup uses this to distinguish a proven unlink from an owned object that
/// is still linked after losing its recorded pathname.
pub fn retained_file_link_count(file: &std::fs::File) -> std::io::Result<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        file.metadata().map(|metadata| metadata.nlink())
    }
    #[cfg(windows)]
    {
        windows::windows_handle_link_count(file).map(u64::from)
    }
}

/// Selects an unpredictable direct child of `<runtime>/tmp` before the daemon
/// creates worker threads or publishes process-wide temp environment values.
pub fn daemon_temp_directory_path(runtime_dir: &Path) -> Result<PathBuf, EndpointError> {
    use std::fmt::Write as _;

    let mut random = [0_u8; 12];
    getrandom::fill(&mut random).map_err(|error| EndpointError::Task {
        message: format!("cannot generate daemon-private temp name: {error}"),
    })?;
    let mut basename = String::from(".haiderd-tmp-");
    for byte in random {
        write!(&mut basename, "{byte:02x}").map_err(|error| EndpointError::Task {
            message: format!("cannot format daemon-private temp name: {error}"),
        })?;
    }
    Ok(runtime_dir.join("tmp").join(basename))
}

/// What a synchronous IPC shutdown request proved at the transport boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpcShutdownOutcome {
    /// The peer was synchronously notified that this connection is closing.
    PeerNotified,
    /// The peer had already closed its side, so there was nobody left to
    /// notify and the connection is already effectively closed.
    PeerAlreadyGone,
    /// The local async-stream slot was emptied, but peer visibility still
    /// depends on the owning Tokio runtime polling cancelled I/O.
    LocalSlotEmptiedOnly,
    /// A prior request had already consumed the shutdown authority.
    AlreadyRequested,
}

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use unix::{
    BoundEndpoint, EndpointAddress, IpcReadHalf, IpcShutdown, IpcStream, IpcWriteHalf,
    PeerExitWatcher, connect, peer_credentials, peer_credentials_and_exit_watcher, peer_is_owner,
    shutdown_handle, split, sweep_stale_endpoints, write_immediate,
};
#[cfg(windows)]
pub use windows::{
    BoundEndpoint, EndpointAddress, IpcReadHalf, IpcShutdown, IpcStream, IpcWriteHalf,
    PeerExitWatcher, connect, peer_credentials, peer_credentials_and_exit_watcher, peer_is_owner,
    shutdown_handle, split, sweep_stale_endpoints, write_immediate,
};

/// Creates and validates one profile's private runtime and temporary
/// directories without binding its endpoint.
#[cfg(unix)]
pub fn prepare_runtime_directory(
    runtime_dir: &Path,
) -> Result<PreparedRuntimeDirectory, EndpointError> {
    unix::prepare_runtime_directory(runtime_dir, None)
}

/// Creates the runtime layout plus one unpredictable daemon-private temp
/// child selected before worker threads start.
#[cfg(unix)]
pub fn prepare_runtime_directory_with_temp(
    runtime_dir: &Path,
    daemon_temp_dir: &Path,
) -> Result<PreparedRuntimeDirectory, EndpointError> {
    unix::prepare_runtime_directory(runtime_dir, Some(daemon_temp_dir))
}

/// Windows named pipes do not use the filesystem runtime for rendezvous, but
/// the profile-local pid and temporary files still do.
#[cfg(windows)]
pub fn prepare_runtime_directory(
    runtime_dir: &Path,
) -> Result<PreparedRuntimeDirectory, EndpointError> {
    windows::prepare_runtime_directory(runtime_dir)
}

/// Windows counterpart of [`prepare_runtime_directory_with_temp`].
#[cfg(windows)]
pub fn prepare_runtime_directory_with_temp(
    runtime_dir: &Path,
    daemon_temp_dir: &Path,
) -> Result<PreparedRuntimeDirectory, EndpointError> {
    windows::prepare_runtime_directory_with_temp(runtime_dir, daemon_temp_dir)
}

/// One deterministic per-profile rendezvous name.
///
/// Unix uses a fixed basename because the containing directory is already
/// profile-scoped. Windows has no filesystem rendezvous directory, so it maps
/// a profile digest to a named pipe instead.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Endpoint {
    address: PathBuf,
}

impl Endpoint {
    #[must_use]
    pub fn new(runtime_dir: impl AsRef<Path>, profile_id: &str) -> Self {
        #[cfg(unix)]
        let address = {
            let _ = profile_id;
            runtime_dir.as_ref().join("h.sock")
        };
        #[cfg(windows)]
        let address = {
            let digest = blake3::hash(profile_id.as_bytes()).to_hex();
            let short = &digest.as_str()[..32];
            let _ = runtime_dir;
            PathBuf::from(format!(r"\\.\pipe\haider-{short}"))
        };
        Self { address }
    }

    /// Adopts an already-resolved address. This compatibility seam lets tests
    /// and explicit callers supply a Unix socket path or Windows pipe name.
    #[must_use]
    pub fn from_address(address: impl Into<PathBuf>) -> Self {
        Self {
            address: address.into(),
        }
    }

    #[must_use]
    pub fn address(&self) -> &Path {
        &self.address
    }

    #[must_use]
    pub fn into_address(self) -> PathBuf {
        self.address
    }

    /// Validates the longest platform address used while binding this
    /// endpoint before the daemon touches its store or runtime directory.
    pub fn validate_for_bind(&self, runtime_dir: &Path) -> Result<(), EndpointError> {
        #[cfg(unix)]
        {
            unix::validate_endpoint_budget(self, runtime_dir)
        }
        #[cfg(windows)]
        {
            windows::validate_endpoint_budget(self, runtime_dir)
        }
    }
}

impl AsRef<Path> for Endpoint {
    fn as_ref(&self) -> &Path {
        self.address()
    }
}

/// Endpoint setup/ownership failure with the same categories and diagnostic
/// fields used by the daemon before this abstraction existed.
#[derive(Debug)]
pub enum EndpointError {
    Io {
        operation: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    /// A deterministic endpoint or its longest staging address cannot be
    /// represented by the platform IPC API.
    AddressTooLong {
        path: PathBuf,
        length: usize,
        limit: usize,
        unit: &'static str,
    },
    Endpoint {
        message: String,
    },
    Task {
        message: String,
    },
    /// Endpoint setup created this exact filesystem artifact and then failed
    /// to remove it. The daemon uses the path as ownership evidence for its
    /// typed runtime-residual report.
    OwnedResidual {
        path: PathBuf,
        source: Box<EndpointError>,
    },
}

impl EndpointError {
    pub(crate) fn io(
        operation: &'static str,
        path: impl Into<PathBuf>,
        source: std::io::Error,
    ) -> Self {
        Self::Io {
            operation,
            path: path.into(),
            source,
        }
    }

    #[must_use]
    pub fn owned_residual_path(&self) -> Option<&Path> {
        match self {
            Self::OwnedResidual { path, .. } => Some(path),
            _ => None,
        }
    }
}

impl std::fmt::Display for EndpointError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io {
                operation,
                path,
                source,
            } => write!(formatter, "{operation} {}: {source}", path.display()),
            Self::AddressTooLong {
                path,
                length,
                limit,
                unit,
            } => write!(
                formatter,
                "endpoint path {} is {length} {unit}; platform IPC limit is {limit} {unit}",
                path.display()
            ),
            Self::Endpoint { message } | Self::Task { message } => formatter.write_str(message),
            Self::OwnedResidual { path, source } => write!(
                formatter,
                "owned endpoint residual {} remained after setup failure: {source}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for EndpointError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::OwnedResidual { source, .. } => Some(source.as_ref()),
            _ => None,
        }
    }
}

/// Kernel-authenticated facts available for an IPC peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerCredentials {
    pub pid: Option<u32>,
    pub uid: u32,
    pub gid: u32,
    /// Windows has no Unix UID on a pipe. The platform backend compares the
    /// peer process TokenUser SID with the daemon process SID and stores only
    /// the authenticated equality result.
    #[cfg(windows)]
    pub(crate) same_user: bool,
}

/// Kernel-observed reason a platform peer can no longer own its connection.
/// No process identity or pipe name is retained in the diagnostic value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerExitReason {
    ProcessExited,
    ConnectionClosed,
}

#[cfg(unix)]
#[must_use]
pub fn peer_credentials_are_owner(credentials: &PeerCredentials, owner_uid: u32) -> bool {
    credentials.uid == owner_uid
}

#[cfg(windows)]
#[must_use]
pub fn peer_credentials_are_owner(credentials: &PeerCredentials, _owner_uid: u32) -> bool {
    credentials.same_user
}

#[cfg(test)]
mod tests {
    use super::Endpoint;
    use std::path::Path;

    #[cfg(unix)]
    #[test]
    #[allow(clippy::expect_used)]
    fn runtime_directory_creation_is_recursive_private_and_idempotent() {
        use std::os::unix::fs::PermissionsExt as _;

        let base = tempfile::tempdir().expect("temporary base");
        let root = base.path().join("missing").join("runtime-root");
        let runtime = root.join("profile");

        super::prepare_runtime_directory(&runtime).expect("prepare nested runtime directory");
        super::prepare_runtime_directory(&runtime).expect("prepare runtime directory again");

        let temp = runtime.join("tmp");
        for path in [&root, &runtime, &temp] {
            let metadata = std::fs::symlink_metadata(path).expect("runtime metadata");
            assert!(metadata.is_dir(), "{} must be a directory", path.display());
            assert_eq!(
                metadata.permissions().mode() & 0o777,
                0o700,
                "{} must be owner-private",
                path.display()
            );
        }
    }

    #[test]
    #[cfg(unix)]
    fn unix_endpoint_uses_the_short_profile_scoped_name() {
        let profile = "profile-alpha";
        assert_eq!(
            Endpoint::new("/tmp/haider-7", profile).address(),
            Path::new("/tmp/haider-7/h.sock")
        );
    }

    #[test]
    #[cfg(windows)]
    fn windows_endpoint_uses_the_same_profile_hash() {
        let profile = "profile-alpha";
        let digest = blake3::hash(profile.as_bytes()).to_hex();
        assert_eq!(
            Endpoint::new("ignored", profile).address(),
            Path::new(&format!(r"\\.\pipe\haider-{}", &digest.as_str()[..32]))
        );
    }

    #[test]
    #[cfg(windows)]
    fn windows_owner_predicate_fails_closed_on_a_mismatched_sid() {
        let credentials = super::PeerCredentials {
            pid: Some(7),
            uid: u32::MAX,
            gid: u32::MAX,
            same_user: false,
        };
        assert!(!super::peer_credentials_are_owner(&credentials, u32::MAX));
    }
}
