use std::path::{Path, PathBuf};

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use unix::{
    BoundEndpoint, EndpointAddress, IpcReadHalf, IpcShutdown, IpcStream, IpcWriteHalf, connect,
    peer_credentials, peer_is_owner, shutdown_handle, split, sweep_stale_endpoints,
    write_immediate,
};
#[cfg(windows)]
pub use windows::{
    BoundEndpoint, EndpointAddress, IpcReadHalf, IpcShutdown, IpcStream, IpcWriteHalf, connect,
    peer_credentials, peer_is_owner, shutdown_handle, split, sweep_stale_endpoints,
    write_immediate,
};

/// Creates and validates one profile's private runtime and temporary
/// directories without binding its endpoint.
#[cfg(unix)]
pub fn prepare_runtime_directory(runtime_dir: &Path) -> Result<(), EndpointError> {
    unix::prepare_runtime_directory(runtime_dir)
}

/// Windows named pipes do not use the filesystem runtime for rendezvous, but
/// the profile-local pid and temporary files still do.
#[cfg(windows)]
pub fn prepare_runtime_directory(runtime_dir: &Path) -> Result<(), EndpointError> {
    let temp = runtime_dir.join("tmp");
    std::fs::create_dir_all(&temp)
        .map_err(|error| EndpointError::io("create profile runtime directories", &temp, error))?;
    crate::set_mode(runtime_dir, 0o700).map_err(|error| {
        EndpointError::io("secure profile runtime directory", runtime_dir, error)
    })?;
    crate::set_mode(&temp, 0o700)
        .map_err(|error| EndpointError::io("secure daemon temporary directory", &temp, error))
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
        }
    }
}

impl std::error::Error for EndpointError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
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
