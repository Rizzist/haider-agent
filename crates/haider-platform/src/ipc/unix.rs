//! Conservative filesystem UDS rendezvous ownership.
//!
//! This is the pre-abstraction daemon endpoint implementation moved intact:
//! directory-relative verification, unpredictable staging names, non-replacing
//! publication/restoration, stale probing, and device/inode cleanup identity.

use super::{Endpoint, EndpointError, PeerCredentials};
use rustix::fs::{AtFlags, FileType, Mode, OFlags, Stat};
use rustix::io::Errno;
use std::fs;
use std::os::fd::AsRawFd as _;
use std::os::fd::OwnedFd;
use std::os::unix::fs::DirBuilderExt as _;
use std::path::{Path, PathBuf};
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

const STAGING_PREFIX: &str = ".haiderd-";
const STAGING_RANDOM_BYTES: usize = 12;
const STAGING_RANDOM_CHARS: usize = 16;
const STAGING_BASENAME_BYTES: usize = STAGING_PREFIX.len() + STAGING_RANDOM_CHARS;
const BASE64_URL_SAFE: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// A duplicate of a connected socket retained outside its async tasks.
///
/// `shutdown(2)` applies to the shared socket, so it wakes both halves and
/// makes a synchronous client close visible even while a current-thread Tokio
/// runtime is not being driven.
pub struct IpcShutdown {
    socket: OwnedFd,
}

impl IpcShutdown {
    #[allow(unsafe_code)]
    pub fn request(&self) -> std::io::Result<()> {
        if unsafe { libc::shutdown(self.socket.as_raw_fd(), libc::SHUT_RDWR) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
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
        let (directory, owner_uid) =
            tokio::task::spawn_blocking(move || prepare_runtime_dir(&runtime_dir_owned))
                .await
                .map_err(|error| EndpointError::Task {
                    message: format!("runtime directory preparation task failed: {error}"),
                })??;
        let socket_path = endpoint.address().to_path_buf();
        let name = endpoint_name(&socket_path)?;
        sweep_staging(&directory, runtime_dir, owner_uid).await?;
        preflight(&directory, &socket_path, &name, owner_uid).await?;

        let staging = staging_name()?;
        let staging_path = runtime_dir.join(&staging);
        let listener = UnixListener::bind(&staging_path)
            .map_err(|error| EndpointError::io("bind Unix socket", &staging_path, error))?;
        let (identity, inode_anchor) =
            match stage_and_publish(&directory, &staging, &name, owner_uid) {
                Ok(published) => published,
                Err(error) => {
                    let _ = rustix::fs::unlinkat(&directory, staging.as_str(), AtFlags::empty());
                    return Err(error);
                }
            };
        Ok(Self {
            listener: Some(listener),
            cleanup: SocketCleanup {
                directory,
                name,
                path: socket_path,
                identity,
                _inode_anchor: inode_anchor,
                active: true,
            },
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
        self.active = false;
        match rustix::fs::statat(
            &self.directory,
            self.name.as_str(),
            AtFlags::SYMLINK_NOFOLLOW,
        ) {
            Ok(stat) if identity_of(&stat) != self.identity => return Ok(()),
            Ok(_) => {}
            Err(Errno::NOENT) => return Ok(()),
            Err(error) => {
                return Err(EndpointError::io(
                    "lstat owned socket",
                    &self.path,
                    error.into(),
                ));
            }
        }
        let claim = staging_name()?;
        match rustix::fs::renameat(
            &self.directory,
            self.name.as_str(),
            &self.directory,
            claim.as_str(),
        ) {
            Ok(()) => {}
            Err(Errno::NOENT) => return Ok(()),
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
                Err(Errno::NOENT) => return Ok(()),
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
            return restore(&self.directory, &claim, &self.name);
        }
        match rustix::fs::unlinkat(&self.directory, claim.as_str(), AtFlags::empty()) {
            Ok(()) | Err(Errno::NOENT) => Ok(()),
            Err(error) => Err(EndpointError::io(
                "remove owned socket",
                &self.path,
                error.into(),
            )),
        }
    }
}

impl Drop for SocketCleanup {
    fn drop(&mut self) {
        let _ = self.remove_owned();
    }
}

fn stage_and_publish(
    directory: &OwnedFd,
    staging: &str,
    name: &str,
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
    rustix::fs::chmodat(
        directory,
        staging,
        Mode::from_bits_truncate(0o600),
        AtFlags::empty(),
    )
    .map_err(|error| EndpointError::io("chmod Unix socket", Path::new(staging), error.into()))?;
    let identity = identity_of(&stat);
    let inode_anchor = anchor_socket(directory, staging, identity)?;
    publish(directory, staging, name)?;
    Ok((identity, inode_anchor))
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

fn prepare_runtime_dir(runtime_dir: &Path) -> Result<(OwnedFd, u32), EndpointError> {
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
    // root remains a separate mkdir/open/fstat sequence below so we can tell
    // a newly created private root from an existing path that must already be
    // owner-private.
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
    let root_created = match root_builder.create(root_path) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(error) => {
            return Err(EndpointError::io(
                "create private runtime root",
                root_path,
                error,
            ));
        }
    };
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
    if root_created {
        rustix::fs::fchmod(&root, Mode::from_bits_truncate(0o700)).map_err(|error| {
            EndpointError::io("chmod private runtime root", root_path, error.into())
        })?;
    } else if root_stat.st_mode & 0o077 != 0 {
        return Err(EndpointError::Endpoint {
            message: format!(
                "runtime root {} is not owner-private (mode {:04o}; expected no group/other bits)",
                root_path.display(),
                root_stat.st_mode & 0o7777,
            ),
        });
    }
    let name = runtime_dir
        .file_name()
        .ok_or_else(|| EndpointError::Endpoint {
            message: format!(
                "profile runtime directory {} has no basename",
                runtime_dir.display()
            ),
        })?;
    match rustix::fs::mkdirat(&root, name, Mode::from_bits_truncate(0o700)) {
        Ok(()) | Err(Errno::EXIST) => {}
        Err(error) => {
            return Err(EndpointError::io(
                "create profile runtime directory",
                runtime_dir,
                error.into(),
            ));
        }
    }
    let directory = rustix::fs::openat(
        &root,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| EndpointError::io("open runtime directory", runtime_dir, error.into()))?;
    let stat = rustix::fs::fstat(&directory)
        .map_err(|error| EndpointError::io("fstat runtime directory", runtime_dir, error.into()))?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::Directory {
        return Err(EndpointError::Endpoint {
            message: format!(
                "runtime path {} is not a real directory",
                runtime_dir.display()
            ),
        });
    }
    if stat.st_uid != expected_uid {
        return Err(EndpointError::Endpoint {
            message: format!(
                "runtime directory {} is owned by uid {}, expected {}",
                runtime_dir.display(),
                stat.st_uid,
                expected_uid
            ),
        });
    }
    rustix::fs::fchmod(&directory, Mode::from_bits_truncate(0o700))
        .map_err(|error| EndpointError::io("chmod runtime directory", runtime_dir, error.into()))?;
    let temp_path = runtime_dir.join("tmp");
    match rustix::fs::mkdirat(&directory, "tmp", Mode::from_bits_truncate(0o700)) {
        Ok(()) | Err(Errno::EXIST) => {}
        Err(error) => {
            return Err(EndpointError::io(
                "create daemon temporary directory",
                &temp_path,
                error.into(),
            ));
        }
    }
    let temp = rustix::fs::openat(
        &directory,
        "tmp",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| {
        EndpointError::io("open daemon temporary directory", &temp_path, error.into())
    })?;
    let temp_stat = rustix::fs::fstat(&temp).map_err(|error| {
        EndpointError::io("fstat daemon temporary directory", &temp_path, error.into())
    })?;
    if FileType::from_raw_mode(temp_stat.st_mode) != FileType::Directory
        || temp_stat.st_uid != expected_uid
    {
        return Err(EndpointError::Endpoint {
            message: format!(
                "daemon temporary path {} is not an owner-private directory",
                temp_path.display()
            ),
        });
    }
    rustix::fs::fchmod(&temp, Mode::from_bits_truncate(0o700)).map_err(|error| {
        EndpointError::io("chmod daemon temporary directory", &temp_path, error.into())
    })?;
    Ok((directory, expected_uid))
}

pub(super) fn prepare_runtime_directory(runtime_dir: &Path) -> Result<(), EndpointError> {
    prepare_runtime_dir(runtime_dir).map(|_| ())
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
    match rustix::fs::renameat(directory, name, directory, claim.as_str()) {
        Ok(()) => {}
        Err(Errno::NOENT) => return Ok(()),
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

/// The one endpoint name this profile-scoped module owns (see
/// [`super::Endpoint::new`]). The sweep only ever considers these.
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
    let Ok(Ok((directory, owner_uid))) =
        tokio::task::spawn_blocking(move || prepare_runtime_dir(&runtime_dir_owned)).await
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
            if name == ENDPOINT_NAME {
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
            }
            _ => {}
        }
    }
    removed
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
    // unpadded URL-safe base64 encoding keeps the full basename to 25 bytes.
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

async fn sweep_staging(
    directory: &OwnedFd,
    runtime_dir: &Path,
    expected_uid: u32,
) -> Result<(), EndpointError> {
    let entries = {
        let mut directory = rustix::fs::Dir::read_from(directory).map_err(|error| {
            EndpointError::io("read runtime directory", runtime_dir, error.into())
        })?;
        let mut names = Vec::new();
        while let Some(entry) = directory.read() {
            let entry = entry.map_err(|error| {
                EndpointError::io("read runtime dir entry", runtime_dir, error.into())
            })?;
            if let Ok(name) = entry.file_name().to_str()
                && name.starts_with(STAGING_PREFIX)
            {
                names.push(name.to_owned());
            }
        }
        names
    };
    for name in entries {
        let Ok(stat) = rustix::fs::statat(directory, name.as_str(), AtFlags::SYMLINK_NOFOLLOW)
        else {
            continue;
        };
        if FileType::from_raw_mode(stat.st_mode) != FileType::Socket || stat.st_uid != expected_uid
        {
            continue;
        }
        match probe(&runtime_dir.join(&name)).await {
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {
                let _ = rustix::fs::unlinkat(directory, name.as_str(), AtFlags::empty());
            }
            _ => {}
        }
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
    Ok(IpcShutdown { socket })
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

#[cfg(target_vendor = "apple")]
#[allow(unsafe_code)]
fn apple_peer_pid(stream: &IpcStream) -> std::io::Result<u32> {
    let mut pid = 0 as libc::pid_t;
    let mut length = libc::socklen_t::try_from(size_of::<libc::pid_t>()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "pid_t size does not fit socklen_t",
        )
    })?;
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
        is_synthesized_probe_timeout, longest_staging_path, probe_failure, staging_name,
        validate_endpoint_budget,
    };
    use rustix::io::Errno;
    use std::io::{Error, ErrorKind};
    use std::os::unix::ffi::OsStrExt as _;
    use std::path::{Path, PathBuf};

    #[test]
    fn staging_name_is_twenty_five_url_safe_bytes_with_ninety_six_random_bits() {
        let name = match staging_name() {
            Ok(name) => name,
            Err(error) => panic!("generate staging name: {error}"),
        };
        assert_eq!(name.len(), STAGING_BASENAME_BYTES);
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
