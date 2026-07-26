//! Conservative filesystem UDS rendezvous ownership.

use crate::{DaemonConfig, DaemonError};
use std::fs;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use tokio::net::{UnixListener, UnixStream};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SocketIdentity {
    device: u64,
    inode: u64,
}

pub(crate) struct BoundEndpoint {
    listener: Option<UnixListener>,
    cleanup: SocketCleanup,
    pub(crate) owner_uid: u32,
}

impl BoundEndpoint {
    pub(crate) fn path(&self) -> &Path {
        &self.cleanup.path
    }

    pub(crate) fn cleanup(&mut self) -> Result<(), DaemonError> {
        self.cleanup.remove_owned()
    }

    pub(crate) async fn accept(
        &self,
    ) -> std::io::Result<(UnixStream, tokio::net::unix::SocketAddr)> {
        match &self.listener {
            Some(listener) => listener.accept().await,
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

struct SocketCleanup {
    path: PathBuf,
    identity: SocketIdentity,
    active: bool,
}

impl SocketCleanup {
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
