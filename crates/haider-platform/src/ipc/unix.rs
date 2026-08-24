//! Conservative filesystem UDS rendezvous ownership.
//!
//! This is the pre-abstraction daemon endpoint implementation moved intact:
//! directory-relative verification, unpredictable staging names, non-replacing
//! publication/restoration, stale probing, and device/inode cleanup identity.

use super::{Endpoint, EndpointError, PeerCredentials};
use rustix::fs::{AtFlags, FileType, Mode, OFlags, Stat};
use rustix::io::Errno;
use std::fs;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::net::{UnixListener, UnixStream};

pub type IpcStream = UnixStream;
pub type IpcReadHalf = tokio::net::unix::OwnedReadHalf;
pub type IpcWriteHalf = tokio::net::unix::OwnedWriteHalf;
pub type EndpointAddress = tokio::net::unix::SocketAddr;

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
    fs::create_dir_all(runtime_dir)
        .map_err(|error| EndpointError::io("create runtime directory", runtime_dir, error))?;
    let directory = rustix::fs::open(
        runtime_dir,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| EndpointError::io("open runtime directory", runtime_dir, error.into()))?;
    let stat = rustix::fs::fstat(&directory)
        .map_err(|error| EndpointError::io("fstat runtime directory", runtime_dir, error.into()))?;
    let expected_uid = rustix::process::geteuid().as_raw();
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
    Ok((directory, expected_uid))
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

/// Endpoint names this module owns: `haider-<32 hex>.sock` (see
/// [`super::Endpoint::new`]). The sweep only ever considers these.
const ENDPOINT_PREFIX: &str = "haider-";
const ENDPOINT_SUFFIX: &str = ".sock";
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
            if name.starts_with(ENDPOINT_PREFIX) && name.ends_with(ENDPOINT_SUFFIX) {
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

const STAGING_PREFIX: &str = ".haiderd-";

fn staging_name() -> Result<String, EndpointError> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| EndpointError::Task {
        message: format!("cannot generate endpoint staging name: {error}"),
    })?;
    let mut staging = String::with_capacity(STAGING_PREFIX.len() + bytes.len() * 2);
    staging.push_str(STAGING_PREFIX);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut staging, "{byte:02x}").map_err(|error| EndpointError::Task {
            message: format!("cannot format endpoint staging name: {error}"),
        })?;
    }
    Ok(staging)
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

pub fn split(stream: IpcStream) -> (IpcReadHalf, IpcWriteHalf) {
    stream.into_split()
}

pub fn peer_credentials(stream: &IpcStream) -> std::io::Result<PeerCredentials> {
    stream.peer_cred().map(|credentials| PeerCredentials {
        pid: credentials.pid().and_then(|pid| u32::try_from(pid).ok()),
        uid: credentials.uid(),
        gid: credentials.gid(),
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
    use super::{EndpointError, is_synthesized_probe_timeout, probe_failure};
    use rustix::io::Errno;
    use std::io::{Error, ErrorKind};
    use std::path::Path;

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
