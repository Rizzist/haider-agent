use std::path::{Path, PathBuf};

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use unix::{
    BoundEndpoint, EndpointAddress, IpcReadHalf, IpcStream, IpcWriteHalf, connect,
    peer_credentials, peer_is_owner, split, write_immediate,
};
#[cfg(windows)]
pub use windows::{
    BoundEndpoint, EndpointAddress, IpcReadHalf, IpcStream, IpcWriteHalf, connect,
    peer_credentials, peer_is_owner, split, write_immediate,
};

/// One deterministic per-profile rendezvous name.
///
/// The digest is deliberately the same on every operating system. Unix joins
/// the historical socket basename under the runtime directory; Windows maps
/// that digest to a named pipe.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Endpoint {
    address: PathBuf,
}

impl Endpoint {
    #[must_use]
    pub fn new(runtime_dir: impl AsRef<Path>, profile_id: &str) -> Self {
        let digest = blake3::hash(profile_id.as_bytes()).to_hex();
        let short = &digest.as_str()[..32];
        #[cfg(unix)]
        let address = runtime_dir.as_ref().join(format!("haider-{short}.sock"));
        #[cfg(windows)]
        let address = {
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

    #[test]
    #[cfg(unix)]
    fn unix_endpoint_keeps_the_historical_name() {
        let profile = "profile-alpha";
        let digest = blake3::hash(profile.as_bytes()).to_hex();
        assert_eq!(
            Endpoint::new("/tmp/haider-7", profile).address(),
            Path::new(&format!(
                "/tmp/haider-7/haider-{}.sock",
                &digest.as_str()[..32]
            ))
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
