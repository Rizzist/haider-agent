//! Conservative filesystem UDS rendezvous ownership (d1 report R2/R3).
//!
//! Callers may only reach a bound endpoint through [`bind`], which runs the
//! full probe → verified-unlink → bind → record-identity sequence. This is
//! rendezvous plumbing only; the singleton authority is the profile lock,
//! which `runtime.rs` acquires before this module ever touches the socket.

use crate::{DaemonConfig, DaemonError};
use std::fs;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use tokio::net::{UnixListener, UnixStream};

/// The one identity cleanup trusts: the `lstat` device+inode pair recorded
/// immediately after bind (R3). Path equality is never sufficient — a
/// successor daemon may have re-bound the same path with a new inode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SocketIdentity {
    device: u64,
    inode: u64,
}

/// A bound, permission-verified listener plus the cleanup guard for exactly
/// the socket inode it created.
pub(crate) struct BoundEndpoint {
    /// `None` after [`BoundEndpoint::close_listener`]: drain has begun and no
    /// further connections are accepted.
    listener: Option<UnixListener>,
    cleanup: SocketCleanup,
    /// Effective UID owning the runtime dir and socket; connections from any
    /// other peer UID are refused (R2).
    pub(crate) owner_uid: u32,
}

impl BoundEndpoint {
    pub(crate) fn path(&self) -> &Path {
        &self.cleanup.path
    }

    /// Removes the owned socket now (drain step); Drop remains as a backstop.
    pub(crate) fn cleanup(&mut self) -> Result<(), DaemonError> {
        self.cleanup.remove_owned()
    }

    pub(crate) async fn accept(
        &self,
    ) -> std::io::Result<(UnixStream, tokio::net::unix::SocketAddr)> {
        match &self.listener {
            Some(listener) => listener.accept().await,
            // Defensive: the accept loop exits before the listener closes,
            // so this arm only fires on a future misordering.
            None => Err(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "daemon listener is closed",
            )),
        }
    }

    pub(crate) fn close_listener(&mut self) {
        self.listener.take();
    }
}

/// Idempotent remove-exactly-what-we-bound guard (R3).
struct SocketCleanup {
    path: PathBuf,
    identity: SocketIdentity,
    active: bool,
}

impl SocketCleanup {
    /// Unlinks the socket only if the node at `path` still has the recorded
    /// device+inode. A replaced or already-removed node is left untouched —
    /// this is what keeps an old daemon from deleting its successor's socket
    /// (R22 named case: successor-socket-deletion).
    fn remove_owned(&mut self) -> Result<(), DaemonError> {
        if !self.active {
            return Ok(());
        }
        self.active = false;
        let metadata = match fs::symlink_metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(DaemonError::io("lstat socket", &self.path, error)),
        };
        let found = SocketIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        };
        if found != self.identity {
            return Ok(());
        }
        fs::remove_file(&self.path)
            .map_err(|error| DaemonError::io("remove owned socket", &self.path, error))
    }
}

impl Drop for SocketCleanup {
    fn drop(&mut self) {
        let _ = self.remove_owned();
    }
}

/// Owns the profile rendezvous socket, in this fixed order:
///
/// 1. create/verify the `0700` same-user runtime directory (R2);
/// 2. probe the endpoint, unlinking only a verified-stale socket (R3);
/// 3. bind, then immediately record the new node's device+inode identity;
/// 4. chmod the socket to `0600` and re-verify type + owner.
///
/// Precondition: the caller already holds the profile lifetime lock, so a
/// *live* endpoint here is an inconsistency worth failing on, not a race.
pub(crate) async fn bind(config: &DaemonConfig) -> Result<BoundEndpoint, DaemonError> {
    let runtime_dir = config.runtime_dir.clone();
    let owner_uid = tokio::task::spawn_blocking(move || prepare_runtime_dir(&runtime_dir))
        .await
        .map_err(|error| DaemonError::Task {
            message: format!("runtime directory preparation task failed: {error}"),
        })??;
    let socket_path = config.endpoint_path();
    preflight(&socket_path, &config.runtime_dir, owner_uid).await?;

    let listener = UnixListener::bind(&socket_path)
        .map_err(|error| DaemonError::io("bind Unix socket", &socket_path, error))?;
    let metadata = fs::symlink_metadata(&socket_path)
        .map_err(|error| DaemonError::io("lstat bound socket", &socket_path, error))?;
    let mut cleanup = SocketCleanup {
        path: socket_path.clone(),
        identity: SocketIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        },
        active: true,
    };
    if let Err(error) = fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600)) {
        let _ = cleanup.remove_owned();
        return Err(DaemonError::io("chmod Unix socket", &socket_path, error));
    }
    if !metadata.file_type().is_socket() || metadata.uid() != owner_uid {
        let _ = cleanup.remove_owned();
        return Err(DaemonError::Endpoint {
            message: format!(
                "bound endpoint {} is not an owner-matched socket",
                socket_path.display()
            ),
        });
    }
    Ok(BoundEndpoint {
        listener: Some(listener),
        cleanup,
        owner_uid,
    })
}

/// Creates the runtime directory if needed and enforces R2: a real (non-
/// symlink) directory owned by the current user, permissions forced to
/// `0700`. Returns the owner UID used for all later peer checks.
fn prepare_runtime_dir(runtime_dir: &Path) -> Result<u32, DaemonError> {
    fs::create_dir_all(runtime_dir)
        .map_err(|error| DaemonError::io("create runtime directory", runtime_dir, error))?;
    let metadata = fs::symlink_metadata(runtime_dir)
        .map_err(|error| DaemonError::io("lstat runtime directory", runtime_dir, error))?;
    let expected_uid = rustix::process::geteuid().as_raw();
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(DaemonError::Endpoint {
            message: format!(
                "runtime path {} is not a real directory",
                runtime_dir.display()
            ),
        });
    }
    if metadata.uid() != expected_uid {
        return Err(DaemonError::Endpoint {
            message: format!(
                "runtime directory {} is owned by uid {}, expected {}",
                runtime_dir.display(),
                metadata.uid(),
                expected_uid
            ),
        });
    }
    fs::set_permissions(runtime_dir, fs::Permissions::from_mode(0o700))
        .map_err(|error| DaemonError::io("chmod runtime directory", runtime_dir, error))?;
    Ok(expected_uid)
}

/// Probe-then-verified-unlink (R3): connect first; only `ECONNREFUSED`
/// (a socket node with no listener) may lead to an unlink, and even then
/// only after [`remove_verified_stale`] proves ownership. `NotFound` is the
/// clean cold-start path; any live listener is an error because the caller
/// holds the profile lock.
async fn preflight(
    socket_path: &Path,
    runtime_dir: &Path,
    expected_uid: u32,
) -> Result<(), DaemonError> {
    match UnixStream::connect(socket_path).await {
        Ok(_) => Err(DaemonError::Endpoint {
            message: format!(
                "a live endpoint exists despite holding the profile lock: {}",
                socket_path.display()
            ),
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {
            remove_verified_stale(socket_path, runtime_dir, expected_uid)
        }
        Err(error) => Err(DaemonError::io("probe Unix socket", socket_path, error)),
    }
}

/// The conservative half of R3: unlink only an `lstat`-verified socket node
/// owned by the expected user, directly inside the expected (same-owner,
/// non-symlink) runtime directory. Anything else is refused rather than
/// removed — a wrong unlink here could take down a healthy daemon's endpoint.
fn remove_verified_stale(
    socket_path: &Path,
    runtime_dir: &Path,
    expected_uid: u32,
) -> Result<(), DaemonError> {
    let directory = fs::symlink_metadata(runtime_dir)
        .map_err(|error| DaemonError::io("lstat runtime directory", runtime_dir, error))?;
    let socket = fs::symlink_metadata(socket_path)
        .map_err(|error| DaemonError::io("lstat stale socket", socket_path, error))?;
    if !directory.file_type().is_dir()
        || directory.file_type().is_symlink()
        || directory.uid() != expected_uid
        || !socket.file_type().is_socket()
        || socket.uid() != expected_uid
        || socket_path.parent() != Some(runtime_dir)
    {
        return Err(DaemonError::Endpoint {
            message: format!(
                "refusing to remove unverified endpoint {}",
                socket_path.display()
            ),
        });
    }
    fs::remove_file(socket_path)
        .map_err(|error| DaemonError::io("remove stale Unix socket", socket_path, error))
}
