//! Conservative filesystem UDS rendezvous ownership.
//!
//! This is the pre-abstraction daemon endpoint implementation moved intact:
//! directory-relative verification, unpredictable staging names, non-replacing
//! publication/restoration, stale probing, and device/inode cleanup identity.

use super::{
    Endpoint, EndpointError, IpcShutdownOutcome, OwnedDirectoryInspection, OwnedDirectoryPathState,
    OwnedDirectoryReceipt, OwnedDirectoryRemoval, PeerCredentials, PeerExitReason,
    PreparedRuntimeDirectory,
};
use rustix::fs::{AtFlags, FileType, Mode, OFlags, Stat};
use rustix::io::Errno;
use std::fs;
use std::os::fd::OwnedFd;
use std::os::unix::fs::DirBuilderExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::net::{UnixListener, UnixStream};

pub type IpcStream = UnixStream;
pub type IpcReadHalf = tokio::net::unix::OwnedReadHalf;
pub type IpcWriteHalf = tokio::net::unix::OwnedWriteHalf;
pub type EndpointAddress = tokio::net::unix::SocketAddr;

// sockaddr_un::sun_path has 108 bytes on Linux/Android and 104 on Apple
// platforms. Filesystem addresses also need a trailing NUL, so these are the
// maximum path-byte counts, not the array capacities.
#[cfg(any(target_os = "linux", target_os = "android"))]
const UNIX_SOCKET_PATH_BYTES: usize = 107;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
const UNIX_SOCKET_PATH_BYTES: usize = 103;

// Keep the 96-bit random coordinate while fitting the portable runtime
// artifact basename ceiling. This name exists only until publication.
const STAGING_PREFIX: &str = ".hd-";
const STAGING_RANDOM_BYTES: usize = 12;
const STAGING_RANDOM_CHARS: usize = 16;
const STAGING_BASENAME_BYTES: usize = STAGING_PREFIX.len() + STAGING_RANDOM_CHARS;
const BASE64_URL_SAFE: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// A duplicate of a connected socket retained outside its async tasks.
///
/// On Unix, `shutdown(2)` applies to the shared socket, so it wakes both halves
/// and makes a synchronous client close visible even while a current-thread
/// Tokio runtime is not being driven. Windows does not share this guarantee;
/// see the platform-specific [`crate::IpcShutdown`] contract there.
pub struct IpcShutdown {
    socket: OwnedFd,
    requested: AtomicBool,
}

/// Unix sockets report peer closure through their ordinary read path, so the
/// daemon never constructs this platform-neutral watcher coordinate.
pub struct PeerExitWatcher;

impl PeerExitWatcher {
    pub async fn wait(self) -> std::io::Result<PeerExitReason> {
        std::future::pending().await
    }
}

impl IpcShutdown {
    pub fn request(&self) -> std::io::Result<IpcShutdownOutcome> {
        if self.requested.swap(true, Ordering::AcqRel) {
            return Ok(IpcShutdownOutcome::AlreadyRequested);
        }
        match rustix::net::shutdown(&self.socket, rustix::net::Shutdown::Both) {
            Ok(()) => Ok(IpcShutdownOutcome::PeerNotified),
            Err(error) => match shutdown_error_outcome(std::io::Error::from(error)) {
                Ok(outcome) => Ok(outcome),
                Err(error) => {
                    self.requested.store(false, Ordering::Release);
                    Err(error)
                }
            },
        }
    }
}

/// Maps only the POSIX error that proves there is no peer left to notify.
///
/// macOS/BSD returns `ENOTCONN` when `shutdown(2)` races with or follows peer
/// closure, while Linux commonly returns success for that connected-then-
/// closed socket state. On both platforms `ENOTCONN` has the same semantic
/// meaning: shutdown is already effective. The other documented errors —
/// `EBADF`, `EINVAL`, and `ENOTSOCK` — indicate a bad local descriptor or call
/// and must remain visible to callers rather than being blanket-swallowed.
fn shutdown_error_outcome(error: std::io::Error) -> std::io::Result<IpcShutdownOutcome> {
    match error.raw_os_error() {
        Some(libc::ENOTCONN) => Ok(IpcShutdownOutcome::PeerAlreadyGone),
        _ => Err(error),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SocketIdentity {
    device: u64,
    inode: u64,
}

#[cfg(target_os = "linux")]
type SocketAnchor = OwnedFd;

#[cfg(not(target_os = "linux"))]
struct SocketAnchor;

/// A bound, permission-verified listener plus the cleanup guard for exactly
/// the socket inode it created.
pub struct BoundEndpoint {
    listener: Option<UnixListener>,
    cleanup: SocketCleanup,
    owner_uid: u32,
}

impl BoundEndpoint {
    pub async fn bind(endpoint: &Endpoint, runtime_dir: &Path) -> Result<Self, EndpointError> {
        validate_endpoint_budget(endpoint, runtime_dir)?;
        let runtime_dir_owned = runtime_dir.to_path_buf();
        let (directory, owner_uid, _) =
            tokio::task::spawn_blocking(move || prepare_runtime_dir(&runtime_dir_owned, None))
                .await
                .map_err(|error| EndpointError::Task {
                    message: format!("runtime directory preparation task failed: {error}"),
                })??;
        let socket_path = endpoint.address().to_path_buf();
        super::validate_unix_socket_path(&socket_path)?;
        let name = endpoint_name(&socket_path)?;
        preflight(&directory, &socket_path, &name, owner_uid).await?;

        let staging = staging_name()?;
        let staging_path = runtime_dir.join(&staging);
        super::validate_runtime_artifact_basename(&staging_path)?;
        super::validate_unix_socket_path(&staging_path)?;
        let listener = UnixListener::bind(&staging_path)
            .map_err(|error| EndpointError::io("bind Unix socket", &staging_path, error))?;
        let (identity, inode_anchor) = identify_staged_socket(&directory, &staging, owner_uid)?;
        let mut cleanup = SocketCleanup {
            directory,
            name: staging.clone(),
            path: staging_path.clone(),
            identity,
            owned_path_candidates: vec![staging_path],
            _inode_anchor: inode_anchor,
            active: true,
        };
        if let Err(error) = chmod_staged_socket(&cleanup.directory, &staging) {
            return Err(retire_failed_staging(cleanup, error));
        }
        if let Err(publication_error) = publish(&cleanup.directory, &staging, &name) {
            return Err(retire_failed_staging(cleanup, publication_error));
        }
        cleanup.name = name;
        cleanup.path = socket_path.clone();
        cleanup.owned_path_candidates.push(socket_path);
        Ok(Self {
            listener: Some(listener),
            cleanup,
            owner_uid,
        })
    }

    #[must_use]
    pub fn endpoint(&self) -> Endpoint {
        Endpoint::from_address(self.cleanup.path.clone())
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.cleanup.path
    }

    #[must_use]
    pub fn owner_uid(&self) -> u32 {
        self.owner_uid
    }

    pub fn cleanup(&mut self) -> Result<(), EndpointError> {
        self.cleanup.remove_owned()
    }

    /// Filesystem paths that still name the exact socket inode this endpoint
    /// created. Cleanup can temporarily move that inode under a random claim
    /// name; retaining the generated path and checking its device/inode keeps
    /// runtime-directory cleanup from guessing ownership from a basename.
    pub fn owned_runtime_paths(&self) -> Result<Vec<PathBuf>, EndpointError> {
        self.cleanup.owned_runtime_paths()
    }

    pub async fn accept(&self) -> std::io::Result<(IpcStream, EndpointAddress)> {
        match &self.listener {
            Some(listener) => listener.accept().await,
            None => Err(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "daemon listener is closed",
            )),
        }
    }

    pub fn close_listener(&mut self) {
        self.listener.take();
    }
}

struct SocketCleanup {
    directory: OwnedFd,
    name: String,
    path: PathBuf,
    identity: SocketIdentity,
    owned_path_candidates: Vec<PathBuf>,
    // Linux can immediately recycle a Unix socket's inode after its pathname
    // is replaced. Keep the staged inode referenced so the device/inode
    // cleanup identity cannot suffer an ABA collision with its replacement.
    _inode_anchor: SocketAnchor,
    active: bool,
}

impl SocketCleanup {
    fn remove_owned(&mut self) -> Result<(), EndpointError> {
        if !self.active {
            return Ok(());
        }
        match rustix::fs::statat(
            &self.directory,
            self.name.as_str(),
            AtFlags::SYMLINK_NOFOLLOW,
        ) {
            Ok(stat) if identity_of(&stat) != self.identity => {
                return self.finish_if_owned_socket_unlinked();
            }
            Ok(_) => {}
            Err(Errno::NOENT) => return self.finish_if_owned_socket_unlinked(),
            Err(error) => {
                return Err(EndpointError::io(
                    "lstat owned socket",
                    &self.path,
                    error.into(),
                ));
            }
        }
        let claim = staging_name()?;
        self.owned_path_candidates
            .push(self.path.with_file_name(&claim));
        match rename_no_replace(&self.directory, self.name.as_str(), claim.as_str()) {
            Ok(()) => {}
            Err(Errno::NOENT) => return self.finish_if_owned_socket_unlinked(),
            Err(Errno::EXIST) => {
                return Err(EndpointError::Endpoint {
                    message: format!(
                        "cannot claim owned endpoint {} without replacing an existing node",
                        self.path.display()
                    ),
                });
            }
            Err(error) => {
                return Err(EndpointError::io(
                    "claim owned socket",
                    &self.path,
                    error.into(),
                ));
            }
        }
        let stat =
            match rustix::fs::statat(&self.directory, claim.as_str(), AtFlags::SYMLINK_NOFOLLOW) {
                Ok(stat) => stat,
                Err(Errno::NOENT) => return self.finish_if_owned_socket_unlinked(),
                Err(error) => {
                    restore(&self.directory, &claim, &self.name)?;
                    return Err(EndpointError::io(
                        "lstat claimed socket",
                        &self.path,
                        error.into(),
                    ));
                }
            };
        if identity_of(&stat) != self.identity {
            restore(&self.directory, &claim, &self.name)?;
            return self.finish_if_owned_socket_unlinked();
        }
        match rustix::fs::unlinkat(&self.directory, claim.as_str(), AtFlags::empty()) {
            Ok(()) => {
                self.active = false;
                Ok(())
            }
            Err(Errno::NOENT) => self.finish_if_owned_socket_unlinked(),
            Err(error) => Err(EndpointError::io(
                "remove owned socket",
                &self.path,
                error.into(),
            )),
        }
    }

    fn finish_if_owned_socket_unlinked(&mut self) -> Result<(), EndpointError> {
        #[cfg(target_os = "linux")]
        {
            let stat = rustix::fs::fstat(&self._inode_anchor).map_err(|error| {
                EndpointError::io("fstat retained socket inode", &self.path, error.into())
            })?;
            if stat.st_nlink == 0 {
                self.active = false;
                return Ok(());
            }
        }
        #[cfg(target_os = "linux")]
        {
            Err(self.owned_socket_coordinate_lost())
        }
        #[cfg(not(target_os = "linux"))]
        {
            // Apple/BSD provide no O_PATH-equivalent anchor for a filesystem
            // socket inode. A replacement at the public coordinate is still
            // preserved; the listener owns the old socket object, and losing
            // its directory entry is the expected controlled-handover case.
            self.active = false;
            Ok(())
        }
    }

    fn owned_socket_coordinate_lost(&self) -> EndpointError {
        EndpointError::OwnedResidual {
            path: self.path.clone(),
            source: Box::new(EndpointError::Endpoint {
                message: format!(
                    "daemon-owned endpoint remains linked outside its recorded coordinate: {}",
                    self.path.display()
                ),
            }),
        }
    }

    fn owned_runtime_paths(&self) -> Result<Vec<PathBuf>, EndpointError> {
        use std::os::unix::fs::MetadataExt as _;

        let mut owned_paths = Vec::new();
        for path in &self.owned_path_candidates {
            match std::fs::symlink_metadata(path) {
                Ok(metadata) => {
                    let identity = SocketIdentity {
                        device: metadata.dev(),
                        inode: metadata.ino(),
                    };
                    if identity == self.identity {
                        owned_paths.push(path.clone());
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(EndpointError::io(
                        "inspect owned endpoint residual",
                        path,
                        error,
                    ));
                }
            }
        }
        if owned_paths.is_empty() && self.active {
            #[cfg(target_os = "linux")]
            {
                let stat = rustix::fs::fstat(&self._inode_anchor).map_err(|error| {
                    EndpointError::io("fstat retained socket residual", &self.path, error.into())
                })?;
                if stat.st_nlink == 0 {
                    return Ok(owned_paths);
                }
            }
            return Err(self.owned_socket_coordinate_lost());
        }
        Ok(owned_paths)
    }
}

impl Drop for SocketCleanup {
    fn drop(&mut self) {
        let _ = self.remove_owned();
    }
}

fn identify_staged_socket(
    directory: &OwnedFd,
    staging: &str,
    owner_uid: u32,
) -> Result<(SocketIdentity, SocketAnchor), EndpointError> {
    let stat =
        rustix::fs::statat(directory, staging, AtFlags::SYMLINK_NOFOLLOW).map_err(|error| {
            EndpointError::io("lstat bound socket", Path::new(staging), error.into())
        })?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::Socket || stat.st_uid != owner_uid {
        return Err(EndpointError::Endpoint {
            message: format!("bound endpoint {staging} is not an owner-matched socket"),
        });
    }
    let identity = identity_of(&stat);
    let inode_anchor = anchor_socket(directory, staging, identity)?;
    Ok((identity, inode_anchor))
}

fn chmod_staged_socket(directory: &OwnedFd, staging: &str) -> Result<(), EndpointError> {
    rustix::fs::chmodat(
        directory,
        staging,
        Mode::from_bits_truncate(0o600),
        AtFlags::empty(),
    )
    .map_err(|error| EndpointError::io("chmod Unix socket", Path::new(staging), error.into()))
}

fn retire_failed_staging(mut cleanup: SocketCleanup, setup_error: EndpointError) -> EndpointError {
    let Err(cleanup_error) = cleanup.remove_owned() else {
        return setup_error;
    };
    let cleanup_message = cleanup_error.to_string();
    if let Ok(mut paths) = cleanup.owned_runtime_paths()
        && let Some(path) = paths.pop()
    {
        cleanup.active = false;
        return EndpointError::OwnedResidual {
            path,
            source: Box::new(EndpointError::Endpoint {
                message: format!(
                    "{setup_error}; failed to remove the exact staged socket: {cleanup_message}"
                ),
            }),
        };
    }
    EndpointError::Endpoint {
        message: format!(
            "{setup_error}; staged-socket cleanup failed without a verified residual coordinate: \
             {cleanup_message}"
        ),
    }
}

#[cfg(target_os = "linux")]
fn anchor_socket(
    directory: &OwnedFd,
    staging: &str,
    identity: SocketIdentity,
) -> Result<SocketAnchor, EndpointError> {
    let anchor = rustix::fs::openat(
        directory,
        staging,
        OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| {
        EndpointError::io(
            "anchor bound socket inode",
            Path::new(staging),
            error.into(),
        )
    })?;
    let stat = rustix::fs::fstat(&anchor).map_err(|error| {
        EndpointError::io(
            "fstat bound socket inode anchor",
            Path::new(staging),
            error.into(),
        )
    })?;
    if identity_of(&stat) != identity {
        return Err(EndpointError::Endpoint {
            message: format!("bound endpoint {staging} changed before publication"),
        });
    }
    Ok(anchor)
}

#[cfg(not(target_os = "linux"))]
fn anchor_socket(
    _directory: &OwnedFd,
    _staging: &str,
    _identity: SocketIdentity,
) -> Result<SocketAnchor, EndpointError> {
    Ok(SocketAnchor)
}

fn publish(directory: &OwnedFd, staging: &str, name: &str) -> Result<(), EndpointError> {
    match rename_no_replace(directory, staging, name) {
        Ok(()) => Ok(()),
        Err(Errno::EXIST) => Err(EndpointError::Endpoint {
            message: format!("endpoint {name} appeared while binding under the profile lock"),
        }),
        Err(error) => Err(EndpointError::io(
            "publish Unix socket",
            Path::new(name),
            error.into(),
        )),
    }
}

#[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android"))]
fn rename_no_replace(directory: &OwnedFd, from: &str, to: &str) -> Result<(), Errno> {
    use rustix::fs::RenameFlags;
    rustix::fs::renameat_with(directory, from, directory, to, RenameFlags::NOREPLACE)
}

#[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android")))]
fn rename_no_replace(directory: &OwnedFd, from: &str, to: &str) -> Result<(), Errno> {
    match rustix::fs::statat(directory, to, AtFlags::SYMLINK_NOFOLLOW) {
        Err(Errno::NOENT) => {}
        Ok(_) => return Err(Errno::EXIST),
        Err(error) => return Err(error),
    }
    rustix::fs::renameat(directory, from, directory, to)
}

fn prepare_runtime_dir(
    runtime_dir: &Path,
    daemon_temp_dir: Option<&Path>,
) -> Result<(OwnedFd, u32, Vec<OwnedDirectoryReceipt>), EndpointError> {
    let root_path = runtime_dir
        .parent()
        .ok_or_else(|| EndpointError::Endpoint {
            message: format!(
                "profile runtime directory {} has no containing root",
                runtime_dir.display()
            ),
        })?;
    // An explicit HAIDER_RUNTIME_DIR may name a nested root whose parents do
    // not exist yet. Create only that ancestor chain recursively; the final
    // root remains a separate mkdir/open/fstat sequence below so ownership and
    // file type are validated through a NOFOLLOW descriptor before an
    // owner-controlled loose mode is tightened.
    if let Some(root_parent) = root_path.parent()
        && !root_parent.as_os_str().is_empty()
    {
        let mut parent_builder = fs::DirBuilder::new();
        parent_builder.recursive(true).mode(0o700);
        parent_builder.create(root_parent).map_err(|error| {
            EndpointError::io("create private runtime root ancestors", root_parent, error)
        })?;
    }
    let mut root_builder = fs::DirBuilder::new();
    root_builder.mode(0o700);
    match root_builder.create(root_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(EndpointError::io(
                "create private runtime root",
                root_path,
                error,
            ));
        }
    }
    let root = rustix::fs::open(
        root_path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| EndpointError::io("open private runtime root", root_path, error.into()))?;
    let root_stat = rustix::fs::fstat(&root).map_err(|error| {
        EndpointError::io("fstat private runtime root", root_path, error.into())
    })?;
    let expected_uid = rustix::process::geteuid().as_raw();
    if FileType::from_raw_mode(root_stat.st_mode) != FileType::Directory
        || root_stat.st_uid != expected_uid
    {
        return Err(EndpointError::Endpoint {
            message: format!(
                "runtime root {} is not an owner directory (uid {}, expected {}; mode {:04o})",
                root_path.display(),
                root_stat.st_uid,
                expected_uid,
                root_stat.st_mode & 0o7777,
            ),
        });
    }
    rustix::fs::fchmod(&root, Mode::from_bits_truncate(0o700)).map_err(|error| {
        EndpointError::io("chmod private runtime root", root_path, error.into())
    })?;
    let name = runtime_dir
        .file_name()
        .ok_or_else(|| EndpointError::Endpoint {
            message: format!(
                "profile runtime directory {} has no basename",
                runtime_dir.display()
            ),
        })?;
    let (directory, runtime_receipt) = prepare_child_directory(
        &root,
        name,
        runtime_dir,
        expected_uid,
        "runtime directory",
        true,
    )?;
    let temp_path = runtime_dir.join("tmp");
    let (temp, temp_receipt) = match prepare_child_directory(
        &directory,
        std::ffi::OsStr::new("tmp"),
        &temp_path,
        expected_uid,
        "daemon temporary directory",
        true,
    ) {
        Ok(prepared) => prepared,
        Err(error) => {
            if let Some(mut receipt) = runtime_receipt {
                return match remove_owned_empty_directory(&mut receipt) {
                    Ok(
                        OwnedDirectoryRemoval::Removed
                        | OwnedDirectoryRemoval::RemovalRequested
                        | OwnedDirectoryRemoval::AlreadyAbsent,
                    ) => Err(error),
                    Ok(OwnedDirectoryRemoval::ReplacementPreserved) => Err(error),
                    Ok(OwnedDirectoryRemoval::CoordinateLost | OwnedDirectoryRemoval::NotEmpty)
                    | Err(_) => Err(EndpointError::OwnedResidual {
                        path: receipt.path().to_path_buf(),
                        source: Box::new(error),
                    }),
                };
            }
            return Err(error);
        }
    };
    let mut receipts = Vec::with_capacity(2);
    if let Some(receipt) = runtime_receipt {
        receipts.push(receipt);
    }
    if let Some(receipt) = temp_receipt {
        receipts.push(receipt);
    }
    if let Some(daemon_temp_dir) = daemon_temp_dir {
        if daemon_temp_dir.parent() != Some(temp_path.as_path()) {
            return rollback_created_directories(
                receipts,
                EndpointError::Endpoint {
                    message: format!(
                        "daemon temp directory {} is not a direct child of {}",
                        daemon_temp_dir.display(),
                        temp_path.display()
                    ),
                },
            );
        }
        let daemon_temp_name = match daemon_temp_dir.file_name() {
            Some(name) => name,
            None => {
                return rollback_created_directories(
                    receipts,
                    EndpointError::Endpoint {
                        message: format!(
                            "daemon temp directory {} has no basename",
                            daemon_temp_dir.display()
                        ),
                    },
                );
            }
        };
        match prepare_child_directory(
            &temp,
            daemon_temp_name,
            daemon_temp_dir,
            expected_uid,
            "daemon-private temporary directory",
            false,
        ) {
            Ok((_, Some(receipt))) => receipts.push(receipt),
            Ok((_, None)) => {
                return rollback_created_directories(
                    receipts,
                    EndpointError::Endpoint {
                        message: format!(
                            "daemon-private temp path unexpectedly pre-existed: {}",
                            daemon_temp_dir.display()
                        ),
                    },
                );
            }
            Err(error) => return rollback_created_directories(receipts, error),
        }
    }
    Ok((directory, expected_uid, receipts))
}

pub(super) fn prepare_runtime_directory(
    runtime_dir: &Path,
    daemon_temp_dir: Option<&Path>,
) -> Result<PreparedRuntimeDirectory, EndpointError> {
    let (_, _, receipts) = prepare_runtime_dir(runtime_dir, daemon_temp_dir)?;
    Ok(PreparedRuntimeDirectory::new(receipts))
}

fn prepare_child_directory(
    parent: &OwnedFd,
    name: &std::ffi::OsStr,
    path: &Path,
    expected_uid: u32,
    description: &'static str,
    allow_existing: bool,
) -> Result<(OwnedFd, Option<OwnedDirectoryReceipt>), EndpointError> {
    let name_string = name.to_str().ok_or_else(|| EndpointError::Endpoint {
        message: format!("{description} basename is not UTF-8: {}", path.display()),
    })?;
    match open_validated_directory(parent, name, path, expected_uid, description) {
        Ok(directory) if allow_existing => return Ok((directory, None)),
        Ok(_) => {
            return Err(EndpointError::Endpoint {
                message: format!("{description} already exists: {}", path.display()),
            });
        }
        Err(EndpointError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    loop {
        let staging = staging_name()?;
        match rustix::fs::mkdirat(parent, staging.as_str(), Mode::from_bits_truncate(0o700)) {
            Ok(()) => {}
            Err(Errno::EXIST) => continue,
            Err(error) => {
                return Err(EndpointError::io(
                    "create staged private directory",
                    path,
                    error.into(),
                ));
            }
        }
        let staging_path = path.with_file_name(&staging);
        let anchor = match open_validated_directory(
            parent,
            std::ffi::OsStr::new(&staging),
            &staging_path,
            expected_uid,
            description,
        ) {
            Ok(anchor) => anchor,
            Err(error) => {
                // The staged name can be replaced after mkdir. Without the
                // descriptor returned by successful validation there is no
                // exact object we may safely unlink, so surface the residual.
                return Err(EndpointError::OwnedResidual {
                    path: staging_path,
                    source: Box::new(error),
                });
            }
        };
        let mut receipt = OwnedDirectoryReceipt::new(staging_path, anchor);
        let working = match rustix::io::dup(&receipt.anchor) {
            Ok(working) => working,
            Err(error) => {
                let source = EndpointError::io(
                    "duplicate staged private directory",
                    receipt.path(),
                    error.into(),
                );
                return match remove_owned_empty_directory(&mut receipt) {
                    Ok(
                        OwnedDirectoryRemoval::Removed
                        | OwnedDirectoryRemoval::RemovalRequested
                        | OwnedDirectoryRemoval::AlreadyAbsent,
                    ) => Err(source),
                    _ => Err(EndpointError::OwnedResidual {
                        path: receipt.path().to_path_buf(),
                        source: Box::new(source),
                    }),
                };
            }
        };
        match rename_no_replace(parent, staging.as_str(), name_string) {
            Ok(()) => {
                receipt.path = path.to_path_buf();
                return Ok((working, Some(receipt)));
            }
            Err(Errno::EXIST) => {
                match remove_owned_empty_directory(&mut receipt) {
                    Ok(
                        OwnedDirectoryRemoval::Removed
                        | OwnedDirectoryRemoval::RemovalRequested
                        | OwnedDirectoryRemoval::AlreadyAbsent,
                    ) => {}
                    _ => {
                        return Err(EndpointError::OwnedResidual {
                            path: receipt.path().to_path_buf(),
                            source: Box::new(EndpointError::Endpoint {
                                message: format!(
                                    "{description} appeared while its staging directory could not be retired: {}",
                                    path.display()
                                ),
                            }),
                        });
                    }
                }
                if !allow_existing {
                    return Err(EndpointError::Endpoint {
                        message: format!("{description} appeared concurrently: {}", path.display()),
                    });
                }
                return open_validated_directory(parent, name, path, expected_uid, description)
                    .map(|directory| (directory, None));
            }
            Err(error) => {
                let remove_result = remove_owned_empty_directory(&mut receipt);
                return match remove_result {
                    Ok(
                        OwnedDirectoryRemoval::Removed
                        | OwnedDirectoryRemoval::RemovalRequested
                        | OwnedDirectoryRemoval::AlreadyAbsent,
                    ) => Err(EndpointError::io(
                        "publish private directory",
                        path,
                        error.into(),
                    )),
                    _ => Err(EndpointError::OwnedResidual {
                        path: receipt.path().to_path_buf(),
                        source: Box::new(EndpointError::io(
                            "publish private directory",
                            path,
                            error.into(),
                        )),
                    }),
                };
            }
        }
    }
}

fn rollback_created_directories(
    mut receipts: Vec<OwnedDirectoryReceipt>,
    error: EndpointError,
) -> Result<(OwnedFd, u32, Vec<OwnedDirectoryReceipt>), EndpointError> {
    while let Some(mut receipt) = receipts.pop() {
        match remove_owned_empty_directory(&mut receipt) {
            Ok(
                OwnedDirectoryRemoval::Removed
                | OwnedDirectoryRemoval::RemovalRequested
                | OwnedDirectoryRemoval::AlreadyAbsent
                | OwnedDirectoryRemoval::ReplacementPreserved,
            ) => {}
            Ok(OwnedDirectoryRemoval::CoordinateLost | OwnedDirectoryRemoval::NotEmpty)
            | Err(_) => {
                return Err(EndpointError::OwnedResidual {
                    path: receipt.path().to_path_buf(),
                    source: Box::new(error),
                });
            }
        }
    }
    Err(error)
}

fn open_validated_directory(
    parent: &OwnedFd,
    name: &std::ffi::OsStr,
    path: &Path,
    expected_uid: u32,
    description: &'static str,
) -> Result<OwnedFd, EndpointError> {
    let directory = rustix::fs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| EndpointError::io("open private directory", path, error.into()))?;
    let stat = rustix::fs::fstat(&directory)
        .map_err(|error| EndpointError::io("fstat private directory", path, error.into()))?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::Directory || stat.st_uid != expected_uid {
        return Err(EndpointError::Endpoint {
            message: format!(
                "{description} {} is not an owner-private real directory",
                path.display()
            ),
        });
    }
    rustix::fs::fchmod(&directory, Mode::from_bits_truncate(0o700))
        .map_err(|error| EndpointError::io("chmod private directory", path, error.into()))?;
    Ok(directory)
}

pub(super) fn remove_owned_empty_directory(
    receipt: &mut OwnedDirectoryReceipt,
) -> std::io::Result<OwnedDirectoryRemoval> {
    match owned_directory_path_state(receipt)? {
        OwnedDirectoryPathState::Owned => {}
        OwnedDirectoryPathState::OwnedObjectUnlinked => {
            return Ok(OwnedDirectoryRemoval::AlreadyAbsent);
        }
        OwnedDirectoryPathState::ReplacementPreserved => {
            return Ok(OwnedDirectoryRemoval::ReplacementPreserved);
        }
        OwnedDirectoryPathState::CoordinateLost => {
            return Ok(OwnedDirectoryRemoval::CoordinateLost);
        }
    }
    let expected = identity_of(&rustix::fs::fstat(&receipt.anchor)?);
    let original = receipt.path.clone();
    let claim = original.with_file_name(staging_name().map_err(endpoint_error_to_io)?);
    match rustix::fs::renameat_with(
        rustix::fs::CWD,
        &original,
        rustix::fs::CWD,
        &claim,
        rustix::fs::RenameFlags::NOREPLACE,
    ) {
        Ok(()) => {}
        Err(Errno::NOENT) => {
            return match owned_directory_path_state(receipt)? {
                OwnedDirectoryPathState::OwnedObjectUnlinked => {
                    Ok(OwnedDirectoryRemoval::AlreadyAbsent)
                }
                _ => Ok(OwnedDirectoryRemoval::CoordinateLost),
            };
        }
        Err(error) => return Err(error.into()),
    }
    receipt.path = claim.clone();
    let claimed = match rustix::fs::statat(rustix::fs::CWD, &claim, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => identity_of(&stat),
        Err(Errno::NOENT) => {
            return match owned_directory_path_state(receipt)? {
                OwnedDirectoryPathState::OwnedObjectUnlinked => {
                    Ok(OwnedDirectoryRemoval::AlreadyAbsent)
                }
                _ => Ok(OwnedDirectoryRemoval::CoordinateLost),
            };
        }
        Err(error) => return Err(error.into()),
    };
    if claimed != expected {
        // The public source was replaced between the coordinate check and
        // rename. Put that unowned object back with NOREPLACE; never unlink it
        // merely because it reached our unpredictable claim coordinate.
        restore_owned_directory(receipt, &original)?;
        return match owned_directory_path_state(receipt)? {
            OwnedDirectoryPathState::OwnedObjectUnlinked
            | OwnedDirectoryPathState::ReplacementPreserved => {
                Ok(OwnedDirectoryRemoval::ReplacementPreserved)
            }
            OwnedDirectoryPathState::Owned | OwnedDirectoryPathState::CoordinateLost => {
                Ok(OwnedDirectoryRemoval::CoordinateLost)
            }
        };
    }
    match rustix::fs::unlinkat(rustix::fs::CWD, &claim, AtFlags::REMOVEDIR) {
        Ok(()) => Ok(OwnedDirectoryRemoval::Removed),
        Err(Errno::NOENT) => match owned_directory_path_state(receipt)? {
            OwnedDirectoryPathState::OwnedObjectUnlinked => Ok(OwnedDirectoryRemoval::Removed),
            OwnedDirectoryPathState::Owned
            | OwnedDirectoryPathState::ReplacementPreserved
            | OwnedDirectoryPathState::CoordinateLost => Ok(OwnedDirectoryRemoval::CoordinateLost),
        },
        Err(Errno::NOTEMPTY) => {
            restore_owned_directory(receipt, &original)?;
            Ok(OwnedDirectoryRemoval::NotEmpty)
        }
        Err(error) => {
            let restore_result = restore_owned_directory(receipt, &original);
            match restore_result {
                Ok(()) => Err(error.into()),
                Err(restore_error) => Err(std::io::Error::other(format!(
                    "remove owned directory failed: {error}; restore failed: {restore_error}"
                ))),
            }
        }
    }
}

pub(super) fn owned_directory_path_state(
    receipt: &OwnedDirectoryReceipt,
) -> std::io::Result<OwnedDirectoryPathState> {
    let anchored = rustix::fs::fstat(&receipt.anchor)?;
    let linked = anchored.st_nlink > 0;
    match rustix::fs::statat(rustix::fs::CWD, &receipt.path, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(found) if identity_of(&found) == identity_of(&anchored) => {
            Ok(OwnedDirectoryPathState::Owned)
        }
        Ok(_) if linked => Ok(OwnedDirectoryPathState::CoordinateLost),
        Ok(_) => Ok(OwnedDirectoryPathState::ReplacementPreserved),
        Err(Errno::NOENT) if linked => Ok(OwnedDirectoryPathState::CoordinateLost),
        Err(Errno::NOENT) => Ok(OwnedDirectoryPathState::OwnedObjectUnlinked),
        Err(error) => Err(error.into()),
    }
}

pub(super) fn inspect_owned_directory(
    receipt: &OwnedDirectoryReceipt,
) -> std::io::Result<OwnedDirectoryInspection> {
    use std::os::unix::ffi::OsStrExt as _;

    if rustix::fs::fstat(&receipt.anchor)?.st_nlink == 0 {
        return Ok(OwnedDirectoryInspection::OwnedObjectUnlinked);
    }
    let mut directory = rustix::fs::Dir::read_from(&receipt.anchor)?;
    let mut entries = Vec::new();
    for entry in &mut directory {
        let entry = entry?;
        let name = entry.file_name().to_bytes();
        if name != b"." && name != b".." {
            entries.push(receipt.path.join(std::ffi::OsStr::from_bytes(name)));
        }
    }
    entries.sort();
    Ok(OwnedDirectoryInspection::Entries(entries))
}

fn restore_owned_directory(
    receipt: &mut OwnedDirectoryReceipt,
    original: &Path,
) -> std::io::Result<()> {
    rustix::fs::renameat_with(
        rustix::fs::CWD,
        &receipt.path,
        rustix::fs::CWD,
        original,
        rustix::fs::RenameFlags::NOREPLACE,
    )
    .map_err(std::io::Error::from)?;
    receipt.path = original.to_path_buf();
    Ok(())
}

fn endpoint_error_to_io(error: EndpointError) -> std::io::Error {
    std::io::Error::other(error.to_string())
}

async fn preflight(
    directory: &OwnedFd,
    socket_path: &Path,
    name: &str,
    expected_uid: u32,
) -> Result<(), EndpointError> {
    match probe(socket_path).await {
        Ok(_) => Err(EndpointError::Endpoint {
            message: format!(
                "a live endpoint exists despite holding the profile lock: {}",
                socket_path.display()
            ),
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match rustix::fs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW) {
                Err(Errno::NOENT) => Ok(()),
                Ok(_) => Err(EndpointError::Endpoint {
                    message: format!(
                        "refusing to bind over unverified endpoint {}",
                        socket_path.display()
                    ),
                }),
                Err(error) => Err(EndpointError::io(
                    "lstat endpoint name",
                    socket_path,
                    error.into(),
                )),
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {
            remove_verified_stale(directory, socket_path, name, expected_uid).await
        }
        // Linux can report ECONNRESET when a listener is replaced or dropped
        // while connect(2) is resolving the pathname. Only ENOENT or
        // ECONNREFUSED proves staleness there; preserve every other platform's
        // historical I/O error exactly.
        Err(error) => Err(probe_failure("probe Unix socket", socket_path, error)),
    }
}

async fn remove_verified_stale(
    directory: &OwnedFd,
    socket_path: &Path,
    name: &str,
    expected_uid: u32,
) -> Result<(), EndpointError> {
    let claim = staging_name()?;
    match rename_no_replace(directory, name, claim.as_str()) {
        Ok(()) => {}
        Err(Errno::NOENT) => return Ok(()),
        Err(Errno::EXIST) => {
            return Err(EndpointError::Endpoint {
                message: format!(
                    "cannot claim stale endpoint {} without replacing an existing node",
                    socket_path.display()
                ),
            });
        }
        Err(error) => {
            return Err(EndpointError::io(
                "claim stale Unix socket",
                socket_path,
                error.into(),
            ));
        }
    }
    let stat = match rustix::fs::statat(directory, claim.as_str(), AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(Errno::NOENT) => return Ok(()),
        Err(error) => {
            restore(directory, &claim, name)?;
            return Err(EndpointError::io(
                "lstat stale Unix socket",
                socket_path,
                error.into(),
            ));
        }
    };
    if FileType::from_raw_mode(stat.st_mode) != FileType::Socket || stat.st_uid != expected_uid {
        restore(directory, &claim, name)?;
        return Err(EndpointError::Endpoint {
            message: format!(
                "refusing to remove unverified endpoint {}",
                socket_path.display()
            ),
        });
    }
    let claim_path = socket_path.with_file_name(&claim);
    match probe(&claim_path).await {
        Ok(_) => {
            restore(directory, &claim, name)?;
            Err(EndpointError::Endpoint {
                message: format!(
                    "a live endpoint appeared while claiming the stale node: {}",
                    socket_path.display()
                ),
            })
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {
            match rustix::fs::unlinkat(directory, claim.as_str(), AtFlags::empty()) {
                Ok(()) | Err(Errno::NOENT) => Ok(()),
                Err(error) => Err(EndpointError::io(
                    "remove stale Unix socket",
                    socket_path,
                    error.into(),
                )),
            }
        }
        Err(error) => {
            restore(directory, &claim, name)?;
            Err(probe_failure(
                "probe claimed Unix socket",
                &claim_path,
                error,
            ))
        }
    }
}

fn probe_failure(operation: &'static str, path: &Path, error: std::io::Error) -> EndpointError {
    if is_synthesized_probe_timeout(&error) {
        // This timeout is synthesized by our probe wrapper, not reported by
        // the kernel, so historical platform errno preservation does not apply.
        return ambiguous_probe_failure(operation, path, error);
    }
    platform_probe_failure(operation, path, error)
}

fn is_synthesized_probe_timeout(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::TimedOut && error.raw_os_error().is_none()
}

fn ambiguous_probe_failure(
    operation: &'static str,
    path: &Path,
    error: std::io::Error,
) -> EndpointError {
    EndpointError::Endpoint {
        message: format!(
            "{operation} for endpoint {} was ambiguous ({error}); treating it as live",
            path.display()
        ),
    }
}

#[cfg(target_os = "linux")]
fn platform_probe_failure(
    operation: &'static str,
    path: &Path,
    error: std::io::Error,
) -> EndpointError {
    ambiguous_probe_failure(operation, path, error)
}

#[cfg(not(target_os = "linux"))]
fn platform_probe_failure(
    operation: &'static str,
    path: &Path,
    error: std::io::Error,
) -> EndpointError {
    EndpointError::io(operation, path, error)
}

const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// The primary endpoint name this profile-scoped module owns (see
/// [`super::Endpoint::new`]). Peer v1 adds only its fixed 17-byte socket and
/// manifest families to this allowlist.
const ENDPOINT_NAME: &str = "h.sock";
/// Upper bound on endpoints examined per sweep, so a runtime directory that
/// has accumulated thousands of nodes cannot stall daemon startup. The sweep
/// is opportunistic hygiene, never a correctness requirement.
const SWEEP_BUDGET: usize = 256;

/// Remove endpoint nodes left behind by daemons that died without running
/// their cleanup (SIGKILL, panic, power loss). Each candidate is proven dead
/// exactly the way [`remove_verified_stale`] proves it — connect refused,
/// then claim-verify-reprobe — so a LIVE endpoint is never removed and a
/// name whose ownership cannot be verified is left alone.
///
/// `keep` is this daemon's own address, which is skipped even though it is
/// live: never depend on ordering between binding and sweeping.
///
/// Returns how many nodes were removed. Errors on individual candidates are
/// deliberately swallowed: hygiene must never fail a daemon start.
pub async fn sweep_stale_endpoints(runtime_dir: &Path, keep: Option<&Path>) -> usize {
    let runtime_dir_owned = runtime_dir.to_path_buf();
    let Ok(Ok((directory, owner_uid, _))) =
        tokio::task::spawn_blocking(move || prepare_runtime_dir(&runtime_dir_owned, None)).await
    else {
        return 0;
    };
    let keep_name = keep
        .and_then(|path| path.file_name())
        .and_then(|name| name.to_str())
        .map(str::to_owned);
    let listing_dir = runtime_dir.to_path_buf();
    let Ok(Ok(names)) = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in fs::read_dir(&listing_dir)? {
            let Ok(entry) = entry else { continue };
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if name == ENDPOINT_NAME || is_peer_socket_name(&name) || is_peer_manifest_name(&name) {
                names.push(name);
            }
        }
        Ok(names)
    })
    .await
    else {
        return 0;
    };

    let mut removed = 0;
    for name in names.into_iter().take(SWEEP_BUDGET) {
        if keep_name.as_deref() == Some(name.as_str()) {
            continue;
        }
        if is_peer_manifest_name(&name) {
            let socket_name = format!("{}.s", &name[..name.len() - 2]);
            if rustix::fs::statat(&directory, socket_name.as_str(), AtFlags::SYMLINK_NOFOLLOW)
                .is_err_and(|error| error == Errno::NOENT)
                && remove_verified_owner_file(&directory, &name, owner_uid).is_ok()
            {
                removed += 1;
            }
            continue;
        }
        let socket_path = runtime_dir.join(&name);
        // Only a refused connect proves death. A live endpoint, a timeout, or
        // any other error leaves the node exactly as it is.
        match probe(&socket_path).await {
            Err(error)
                if error.kind() == std::io::ErrorKind::ConnectionRefused
                    && remove_verified_stale(&directory, &socket_path, &name, owner_uid)
                        .await
                        .is_ok() =>
            {
                removed += 1;
                if is_peer_socket_name(&name) {
                    let manifest_name = format!("{}.j", &name[..name.len() - 2]);
                    let _ = remove_verified_owner_file(&directory, &manifest_name, owner_uid);
                }
            }
            _ => {}
        }
    }
    removed
}

fn is_peer_socket_name(name: &str) -> bool {
    is_peer_artifact_name(name, ".s")
}

fn is_peer_manifest_name(name: &str) -> bool {
    is_peer_artifact_name(name, ".j")
}

fn is_peer_artifact_name(name: &str, extension: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() == 17
        && name.ends_with(extension)
        && (name.starts_with("ph-") || name.starts_with("px-"))
        && bytes[3..15].iter().all(u8::is_ascii_hexdigit)
}

fn remove_verified_owner_file(
    directory: &OwnedFd,
    name: &str,
    expected_uid: u32,
) -> Result<(), EndpointError> {
    let stat = match rustix::fs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(Errno::NOENT) => return Ok(()),
        Err(error) => {
            return Err(EndpointError::io(
                "lstat stale peer manifest",
                Path::new(name),
                error.into(),
            ));
        }
    };
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile || stat.st_uid != expected_uid
    {
        return Err(EndpointError::Endpoint {
            message: format!("refusing to remove unverified peer manifest {name}"),
        });
    }
    match rustix::fs::unlinkat(directory, name, AtFlags::empty()) {
        Ok(()) | Err(Errno::NOENT) => Ok(()),
        Err(error) => Err(EndpointError::io(
            "remove stale peer manifest",
            Path::new(name),
            error.into(),
        )),
    }
}

async fn probe(path: &Path) -> std::io::Result<UnixStream> {
    match tokio::time::timeout(PROBE_TIMEOUT, UnixStream::connect(path)).await {
        Ok(result) => result,
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "endpoint probe did not settle; treating the node as live",
        )),
    }
}

fn restore(directory: &OwnedFd, claim: &str, name: &str) -> Result<(), EndpointError> {
    match rename_no_replace(directory, claim, name) {
        Ok(()) => Ok(()),
        Err(Errno::EXIST) => Err(EndpointError::Endpoint {
            message: format!(
                "cannot restore claimed endpoint {claim}: another node now occupies {name}"
            ),
        }),
        Err(Errno::NOENT) => Ok(()),
        Err(error) => Err(EndpointError::io(
            "restore claimed endpoint",
            Path::new(claim),
            error.into(),
        )),
    }
}

fn staging_name() -> Result<String, EndpointError> {
    // Ninety-six random bits retain a wide pre-knowledge margin while their
    // unpadded URL-safe base64 encoding keeps the full basename to 20 bytes.
    // The old 128-bit hex form used 41 bytes and overflowed macOS sun_path
    // beneath its ordinary per-user TMPDIR.
    let mut bytes = [0_u8; STAGING_RANDOM_BYTES];
    getrandom::fill(&mut bytes).map_err(|error| EndpointError::Task {
        message: format!("cannot generate endpoint staging name: {error}"),
    })?;
    let mut staging = String::with_capacity(STAGING_BASENAME_BYTES);
    staging.push_str(STAGING_PREFIX);
    for chunk in bytes.chunks_exact(3) {
        let indexes = [
            chunk[0] >> 2,
            ((chunk[0] & 0x03) << 4) | (chunk[1] >> 4),
            ((chunk[1] & 0x0f) << 2) | (chunk[2] >> 6),
            chunk[2] & 0x3f,
        ];
        for index in indexes {
            staging.push(char::from(BASE64_URL_SAFE[usize::from(index)]));
        }
    }
    Ok(staging)
}

fn longest_staging_path(runtime_dir: &Path) -> PathBuf {
    runtime_dir.join(format!(
        "{STAGING_PREFIX}{}",
        "A".repeat(STAGING_RANDOM_CHARS)
    ))
}

pub(super) fn validate_endpoint_budget(
    endpoint: &Endpoint,
    runtime_dir: &Path,
) -> Result<(), EndpointError> {
    use std::os::unix::ffi::OsStrExt as _;

    let staging = longest_staging_path(runtime_dir);
    let longest =
        if endpoint.address().as_os_str().as_bytes().len() > staging.as_os_str().as_bytes().len() {
            endpoint.address()
        } else {
            &staging
        };
    let length = longest.as_os_str().as_bytes().len();
    if length > UNIX_SOCKET_PATH_BYTES {
        return Err(EndpointError::AddressTooLong {
            path: longest.to_path_buf(),
            length,
            limit: UNIX_SOCKET_PATH_BYTES,
            unit: "bytes",
        });
    }
    Ok(())
}

fn endpoint_name(socket_path: &Path) -> Result<String, EndpointError> {
    socket_path
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .ok_or_else(|| EndpointError::Endpoint {
            message: format!("endpoint path {} has no file name", socket_path.display()),
        })
}

fn identity_of(stat: &Stat) -> SocketIdentity {
    SocketIdentity {
        device: widen(stat.st_dev),
        inode: widen(stat.st_ino),
    }
}

fn widen(value: impl TryInto<u64>) -> u64 {
    value.try_into().unwrap_or(u64::MAX)
}

pub async fn connect(endpoint: impl AsRef<Path>) -> std::io::Result<IpcStream> {
    UnixStream::connect(endpoint).await
}

pub fn shutdown_handle(stream: &IpcStream) -> std::io::Result<IpcShutdown> {
    let socket = rustix::io::fcntl_dupfd_cloexec(stream, 0).map_err(std::io::Error::from)?;
    Ok(IpcShutdown {
        socket,
        requested: AtomicBool::new(false),
    })
}

pub fn split(stream: IpcStream) -> (IpcReadHalf, IpcWriteHalf) {
    stream.into_split()
}

pub fn peer_credentials(stream: &IpcStream) -> std::io::Result<PeerCredentials> {
    let credentials = stream.peer_cred()?;
    #[cfg(target_vendor = "apple")]
    let pid = Some(apple_peer_pid(stream)?);
    #[cfg(not(target_vendor = "apple"))]
    let pid = credentials.pid().and_then(|pid| u32::try_from(pid).ok());
    Ok(PeerCredentials {
        pid,
        uid: credentials.uid(),
        gid: credentials.gid(),
    })
}

pub fn peer_credentials_and_exit_watcher(
    stream: &IpcStream,
) -> std::io::Result<(PeerCredentials, Option<PeerExitWatcher>)> {
    peer_credentials(stream).map(|credentials| (credentials, None))
}

#[cfg(target_vendor = "apple")]
#[allow(unsafe_code)]
fn apple_peer_pid(stream: &IpcStream) -> std::io::Result<u32> {
    use std::os::fd::AsRawFd as _;

    let mut pid = 0 as libc::pid_t;
    let mut length = libc::socklen_t::try_from(size_of::<libc::pid_t>()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "pid_t size does not fit socklen_t",
        )
    })?;
    // SAFETY: `stream` owns the live socket descriptor, both output pointers
    // are writable, and `length` advertises exactly the pid_t buffer size.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            (&raw mut pid).cast(),
            &raw mut length,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    if usize::try_from(length).ok() != Some(size_of::<libc::pid_t>()) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "LOCAL_PEERPID returned an unexpected value length",
        ));
    }
    u32::try_from(pid).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "LOCAL_PEERPID returned an invalid process id",
        )
    })
}

pub fn peer_is_owner(stream: &IpcStream, owner_uid: u32) -> std::io::Result<bool> {
    peer_credentials(stream).map(|credentials| credentials.uid == owner_uid)
}

/// Direct non-blocking write used only for overload rejection. This remains a
/// raw `write(2)` loop so reactor readiness cannot drop the one rejection.
pub fn write_immediate(stream: &IpcStream, bytes: &[u8]) {
    let mut written = 0;
    while written < bytes.len() {
        match rustix::io::write(stream, &bytes[written..]) {
            Ok(0) => break,
            Ok(count) => written = written.saturating_add(count),
            Err(Errno::INTR) => {}
            Err(_) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Endpoint, EndpointError, STAGING_BASENAME_BYTES, STAGING_PREFIX, UNIX_SOCKET_PATH_BYTES,
        is_synthesized_probe_timeout, longest_staging_path, probe_failure, shutdown_error_outcome,
        shutdown_handle, staging_name, validate_endpoint_budget,
    };
    use crate::ipc::{IpcShutdownOutcome, RUNTIME_ARTIFACT_BASENAME_MAX_BYTES};
    use rustix::io::Errno;
    use std::io::{Error, ErrorKind};
    use std::os::unix::ffi::OsStrExt as _;
    use std::path::{Path, PathBuf};

    #[test]
    fn shutdown_error_mapping_accepts_only_not_connected() {
        assert!(matches!(
            shutdown_error_outcome(Error::from_raw_os_error(libc::ENOTCONN)),
            Ok(IpcShutdownOutcome::PeerAlreadyGone)
        ));

        for code in [libc::EBADF, libc::EINVAL, libc::ENOTSOCK] {
            let error = match shutdown_error_outcome(Error::from_raw_os_error(code)) {
                Ok(outcome) => panic!("errno {code} was hidden as {outcome:?}"),
                Err(error) => error,
            };
            assert_eq!(error.raw_os_error(), Some(code));
        }
    }

    #[tokio::test]
    async fn shutdown_with_an_already_closed_peer_is_never_a_failure() {
        let (stream, peer) = match std::os::unix::net::UnixStream::pair() {
            Ok(pair) => pair,
            Err(error) => panic!("create socket pair: {error}"),
        };
        if let Err(error) = stream.set_nonblocking(true) {
            panic!("make socket nonblocking: {error}");
        }
        let stream = match tokio::net::UnixStream::from_std(stream) {
            Ok(stream) => stream,
            Err(error) => panic!("adopt socket pair: {error}"),
        };
        let shutdown = match shutdown_handle(&stream) {
            Ok(shutdown) => shutdown,
            Err(error) => panic!("retain shutdown handle: {error}"),
        };
        drop(peer);

        let outcome = match shutdown.request() {
            Ok(outcome) => outcome,
            Err(error) => panic!("closed peer leaked a shutdown error: {error}"),
        };
        assert!(matches!(
            outcome,
            IpcShutdownOutcome::PeerNotified | IpcShutdownOutcome::PeerAlreadyGone
        ));
    }

    #[test]
    fn staging_name_fits_runtime_ceiling_with_ninety_six_random_bits() {
        let name = match staging_name() {
            Ok(name) => name,
            Err(error) => panic!("generate staging name: {error}"),
        };
        assert_eq!(name.len(), STAGING_BASENAME_BYTES);
        assert_eq!(name.len(), RUNTIME_ARTIFACT_BASENAME_MAX_BYTES);
        assert!(name.starts_with(STAGING_PREFIX));
        assert!(
            name[STAGING_PREFIX.len()..]
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        );
    }

    #[test]
    fn endpoint_budget_checks_the_longest_staging_path_before_bind() {
        let runtime_length = UNIX_SOCKET_PATH_BYTES - STAGING_BASENAME_BYTES - 1;
        let allowed_runtime =
            PathBuf::from(format!("/{}", "r".repeat(runtime_length.saturating_sub(1))));
        let allowed = Endpoint::new(&allowed_runtime, "profile");
        assert_eq!(
            longest_staging_path(&allowed_runtime)
                .as_os_str()
                .as_bytes()
                .len(),
            UNIX_SOCKET_PATH_BYTES
        );
        assert!(validate_endpoint_budget(&allowed, &allowed_runtime).is_ok());

        let too_long_runtime = allowed_runtime.join("x");
        let too_long = Endpoint::new(&too_long_runtime, "profile");
        match validate_endpoint_budget(&too_long, &too_long_runtime) {
            Err(EndpointError::AddressTooLong {
                path,
                length,
                limit,
                unit,
            }) => {
                assert_eq!(path, longest_staging_path(&too_long_runtime));
                assert_eq!(length, UNIX_SOCKET_PATH_BYTES + 2);
                assert_eq!(limit, UNIX_SOCKET_PATH_BYTES);
                assert_eq!(unit, "bytes");
            }
            other => panic!("expected typed endpoint budget error, got {other:?}"),
        }
    }

    #[test]
    fn shared_sticky_root_derivation_remains_inside_the_socket_budget() {
        let requested = PathBuf::from("/tmp/profile-123");
        let derived =
            crate::user::owner_scoped_runtime_directory_for_metadata(&requested, 0, 0o1777, 501);
        assert_eq!(derived, PathBuf::from("/tmp/haider-501/profile-123"));
        assert!(validate_endpoint_budget(&Endpoint::new(&derived, "profile"), &derived).is_ok());

        let base_root = PathBuf::from("/r");
        let base_requested = base_root.join("p");
        let base_derived = crate::user::owner_scoped_runtime_directory_for_metadata(
            &base_requested,
            0,
            0o1777,
            501,
        );
        let base_length = longest_staging_path(&base_derived)
            .as_os_str()
            .as_bytes()
            .len();
        let root = PathBuf::from(format!(
            "/{}",
            "r".repeat(UNIX_SOCKET_PATH_BYTES.saturating_sub(base_length) + 1)
        ));
        let at_limit_requested = root.join("p");
        let at_limit = crate::user::owner_scoped_runtime_directory_for_metadata(
            &at_limit_requested,
            0,
            0o1777,
            501,
        );
        assert_eq!(
            longest_staging_path(&at_limit).as_os_str().as_bytes().len(),
            UNIX_SOCKET_PATH_BYTES
        );
        assert!(validate_endpoint_budget(&Endpoint::new(&at_limit, "profile"), &at_limit).is_ok());

        let too_long_requested = root.join("rr").join("p");
        let too_long = crate::user::owner_scoped_runtime_directory_for_metadata(
            &too_long_requested,
            0,
            0o1777,
            501,
        );
        assert!(matches!(
            validate_endpoint_budget(&Endpoint::new(&too_long, "profile"), &too_long),
            Err(EndpointError::AddressTooLong { .. })
        ));
    }

    #[tokio::test]
    async fn connected_unix_peers_report_the_authenticated_process_id() {
        let directory = match tempfile::tempdir() {
            Ok(directory) => directory,
            Err(error) => panic!("create peer credential fixture: {error}"),
        };
        let path = directory.path().join("peer.sock");
        let listener = match tokio::net::UnixListener::bind(&path) {
            Ok(listener) => listener,
            Err(error) => panic!("bind peer credential fixture: {error}"),
        };
        let (client, accepted) =
            tokio::join!(tokio::net::UnixStream::connect(&path), listener.accept());
        let client = match client {
            Ok(client) => client,
            Err(error) => panic!("connect peer credential fixture: {error}"),
        };
        let server = match accepted {
            Ok((server, _)) => server,
            Err(error) => panic!("accept peer credential fixture: {error}"),
        };
        for stream in [&client, &server] {
            let credentials = match super::peer_credentials(stream) {
                Ok(credentials) => credentials,
                Err(error) => panic!("read authenticated peer credentials: {error}"),
            };
            assert_eq!(credentials.pid, Some(std::process::id()));
        }
    }

    /// MUTATION CHECK (executed): restore the raw-I/O mapping for `TimedOut`
    /// in `probe_failure` — this test fails at the unexpected-variant panic.
    #[test]
    fn synthesized_probe_timeout_is_a_typed_ambiguous_refusal() {
        let failure = probe_failure(
            "probe Unix socket",
            Path::new("/tmp/haider-timeout.sock"),
            Error::new(ErrorKind::TimedOut, "synthetic probe timeout"),
        );

        match failure {
            EndpointError::Endpoint { message } => {
                assert!(message.contains("was ambiguous (synthetic probe timeout)"));
                assert!(message.contains("treating it as live"));
            }
            other => panic!("expected typed endpoint refusal, got {other:?}"),
        }
    }

    #[test]
    fn kernel_timeout_is_not_classified_as_synthesized() {
        let kernel_timeout: Error = Errno::TIMEDOUT.into();

        assert_eq!(kernel_timeout.kind(), ErrorKind::TimedOut);
        assert!(!is_synthesized_probe_timeout(&kernel_timeout));
    }
}
