//! Conservative filesystem UDS rendezvous ownership (d1 report R2/R3/R22).
//!
//! Callers may only reach a bound endpoint through [`bind`], which runs the
//! full probe → verified-unlink → bind → record-identity sequence. This is
//! rendezvous plumbing only; the singleton authority is the profile lock,
//! which `runtime.rs` acquires before this module ever touches the socket.
//!
//! # Why every step is descriptor-relative
//!
//! A pathname is re-resolved by every syscall, so an `lstat(path)` followed by
//! an `unlink(path)` can act on two different objects. This module therefore
//! opens the runtime directory ONCE (`O_DIRECTORY | O_NOFOLLOW`) and performs
//! every later operation relative to that descriptor, which removes the
//! directory half of the race outright: no later swap of the runtime path can
//! redirect a `statat`/`unlinkat`/`renameat` issued here.
//!
//! The file half is closed by never verifying and acting on the *public* name:
//!
//! - **bind → identity**: the socket is created under a private, random
//!   sibling name, `statat`-ed there, and only then moved onto the public name
//!   with a non-replacing rename. Nothing else can reach the private name, so
//!   the device+inode recorded is provably the node this daemon created — a
//!   racing replacement cannot make the daemon adopt a foreign identity.
//! - **identity → unlink**: cleanup first *claims* the public name by renaming
//!   it to a fresh private name. Identity is then verified, and the unlink
//!   performed, on that private name. A replacement that landed before the
//!   claim is restored untouched (identity mismatch); one that lands after the
//!   claim creates a brand-new node at the public name that this daemon never
//!   touches again.
//!
//! Residual, stated precisely: each rename is atomic, but "restore what we
//! claimed" is a second rename. If a same-UID process creates a node at the
//! public name *between* our claim and our restore, the restore replaces that
//! third node. Nothing in that window can delete a *live* successor's socket —
//! the claim only ever unlinks a node whose recorded identity is ours, or (in
//! stale cleanup) one that still refuses a connect probe under the private
//! name. On platforms without a non-replacing rename the publish step carries
//! the same shape of residual; see [`publish`].

use crate::{DaemonConfig, DaemonError};
use rustix::fs::{AtFlags, FileType, Mode, OFlags, Stat};
use rustix::io::Errno;
use std::fs;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use tokio::net::{UnixListener, UnixStream};

/// The one identity cleanup trusts: the device+inode pair recorded from the
/// private staging name immediately after bind (R3). Path equality is never
/// sufficient — a successor daemon may have re-bound the same path with a new
/// inode.
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

/// Idempotent remove-exactly-what-we-bound guard (R3), acting only through the
/// directory descriptor opened at bind time.
struct SocketCleanup {
    directory: OwnedFd,
    /// Public file name inside `directory`; `path` is diagnostics only.
    name: String,
    path: PathBuf,
    identity: SocketIdentity,
    active: bool,
}

impl SocketCleanup {
    /// Claims the public name, verifies identity on the claimed (private)
    /// name, and unlinks only there. A node that is not ours goes back exactly
    /// where it was, which is what keeps an old daemon from deleting its
    /// successor's socket (R22 named case: successor-socket-deletion).
    fn remove_owned(&mut self) -> Result<(), DaemonError> {
        if !self.active {
            return Ok(());
        }
        self.active = false;
        let claim = staging_name(&self.name)?;
        match rustix::fs::renameat(
            &self.directory,
            self.name.as_str(),
            &self.directory,
            claim.as_str(),
        ) {
            Ok(()) => {}
            // Already gone: nothing of ours remains to remove.
            Err(Errno::NOENT) => return Ok(()),
            Err(error) => {
                return Err(DaemonError::io(
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
                    restore(&self.directory, &claim, &self.name);
                    return Err(DaemonError::io(
                        "lstat claimed socket",
                        &self.path,
                        error.into(),
                    ));
                }
            };
        if identity_of(&stat) != self.identity {
            // A successor replaced the node before we claimed it: put it back
            // and never unlink it.
            restore(&self.directory, &claim, &self.name);
            return Ok(());
        }
        match rustix::fs::unlinkat(&self.directory, claim.as_str(), AtFlags::empty()) {
            Ok(()) | Err(Errno::NOENT) => Ok(()),
            Err(error) => Err(DaemonError::io(
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

/// Owns the profile rendezvous socket, in this fixed order:
///
/// 1. create the runtime directory, then open it `O_DIRECTORY | O_NOFOLLOW`
///    and verify owner + force `0700` through that descriptor (R2);
/// 2. probe the endpoint, unlinking only a verified-stale socket (R3);
/// 3. bind under a private staging name, record that node's device+inode, and
///    chmod it `0600` while it is still unreachable;
/// 4. publish it onto the public name with a non-replacing rename.
///
/// Precondition: the caller already holds the profile lifetime lock, so a
/// *live* endpoint here is an inconsistency worth failing on, not a race.
pub(crate) async fn bind(config: &DaemonConfig) -> Result<BoundEndpoint, DaemonError> {
    let runtime_dir = config.runtime_dir.clone();
    let (directory, owner_uid) =
        tokio::task::spawn_blocking(move || prepare_runtime_dir(&runtime_dir))
            .await
            .map_err(|error| DaemonError::Task {
                message: format!("runtime directory preparation task failed: {error}"),
            })??;
    let socket_path = config.endpoint_path();
    let name = endpoint_name(&socket_path)?;
    preflight(&directory, &socket_path, &name, owner_uid).await?;

    let staging = staging_name(&name)?;
    let staging_path = config.runtime_dir.join(&staging);
    let listener = UnixListener::bind(&staging_path)
        .map_err(|error| DaemonError::io("bind Unix socket", &staging_path, error))?;
    let identity = match stage_and_publish(&directory, &staging, &name, owner_uid) {
        Ok(identity) => identity,
        Err(error) => {
            let _ = rustix::fs::unlinkat(&directory, staging.as_str(), AtFlags::empty());
            return Err(error);
        }
    };
    Ok(BoundEndpoint {
        listener: Some(listener),
        cleanup: SocketCleanup {
            directory,
            name,
            path: socket_path,
            identity,
            active: true,
        },
        owner_uid,
    })
}

/// Verifies the freshly bound staging node, tightens its mode, and moves it
/// onto the public name. The returned identity is provably this daemon's: no
/// other process can name, replace, or even see the staging node.
fn stage_and_publish(
    directory: &OwnedFd,
    staging: &str,
    name: &str,
    owner_uid: u32,
) -> Result<SocketIdentity, DaemonError> {
    let stat = rustix::fs::statat(directory, staging, AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|error| DaemonError::io("lstat bound socket", Path::new(staging), error.into()))?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::Socket || stat.st_uid != owner_uid {
        return Err(DaemonError::Endpoint {
            message: format!("bound endpoint {staging} is not an owner-matched socket"),
        });
    }
    rustix::fs::chmodat(
        directory,
        staging,
        Mode::from_bits_truncate(0o600),
        AtFlags::empty(),
    )
    .map_err(|error| DaemonError::io("chmod Unix socket", Path::new(staging), error.into()))?;
    publish(directory, staging, name)?;
    Ok(identity_of(&stat))
}

/// Moves the staged socket onto the public name without replacing anything
/// that may have appeared there: an endpoint that materialised after the
/// preflight is an error, exactly as a plain `bind` would have reported
/// `EADDRINUSE`.
#[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android"))]
fn publish(directory: &OwnedFd, staging: &str, name: &str) -> Result<(), DaemonError> {
    use rustix::fs::RenameFlags;

    match rustix::fs::renameat_with(directory, staging, directory, name, RenameFlags::NOREPLACE) {
        Ok(()) => Ok(()),
        Err(Errno::EXIST) => Err(DaemonError::Endpoint {
            message: format!("endpoint {name} appeared while binding under the profile lock"),
        }),
        Err(error) => Err(DaemonError::io(
            "publish Unix socket",
            Path::new(name),
            error.into(),
        )),
    }
}

/// Fallback for platforms without a non-replacing rename: check, then rename.
/// Residual: a node created at the public name inside that window is replaced
/// rather than reported.
#[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android")))]
fn publish(directory: &OwnedFd, staging: &str, name: &str) -> Result<(), DaemonError> {
    match rustix::fs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW) {
        Err(Errno::NOENT) => {}
        Ok(_) => {
            return Err(DaemonError::Endpoint {
                message: format!("endpoint {name} appeared while binding under the profile lock"),
            });
        }
        Err(error) => {
            return Err(DaemonError::io(
                "lstat endpoint name",
                Path::new(name),
                error.into(),
            ));
        }
    }
    rustix::fs::renameat(directory, staging, directory, name)
        .map_err(|error| DaemonError::io("publish Unix socket", Path::new(name), error.into()))
}

/// Creates the runtime directory if needed, then opens and verifies it as a
/// descriptor (R2): a real, non-symlink directory owned by the current user,
/// permissions forced to `0700` through the descriptor itself. Returns that
/// descriptor plus the owner UID used for all later peer checks.
fn prepare_runtime_dir(runtime_dir: &Path) -> Result<(OwnedFd, u32), DaemonError> {
    fs::create_dir_all(runtime_dir)
        .map_err(|error| DaemonError::io("create runtime directory", runtime_dir, error))?;
    let directory = rustix::fs::open(
        runtime_dir,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| DaemonError::io("open runtime directory", runtime_dir, error.into()))?;
    let stat = rustix::fs::fstat(&directory)
        .map_err(|error| DaemonError::io("fstat runtime directory", runtime_dir, error.into()))?;
    let expected_uid = rustix::process::geteuid().as_raw();
    if FileType::from_raw_mode(stat.st_mode) != FileType::Directory {
        return Err(DaemonError::Endpoint {
            message: format!(
                "runtime path {} is not a real directory",
                runtime_dir.display()
            ),
        });
    }
    if stat.st_uid != expected_uid {
        return Err(DaemonError::Endpoint {
            message: format!(
                "runtime directory {} is owned by uid {}, expected {}",
                runtime_dir.display(),
                stat.st_uid,
                expected_uid
            ),
        });
    }
    rustix::fs::fchmod(&directory, Mode::from_bits_truncate(0o700))
        .map_err(|error| DaemonError::io("chmod runtime directory", runtime_dir, error.into()))?;
    Ok((directory, expected_uid))
}

/// Probe-then-verified-unlink (R3): connect first; only `ECONNREFUSED`
/// (a socket node with no listener) may lead to an unlink, and even then
/// only after [`remove_verified_stale`] proves ownership. `NotFound` is the
/// clean cold-start path; any live listener is an error because the caller
/// holds the profile lock.
async fn preflight(
    directory: &OwnedFd,
    socket_path: &Path,
    name: &str,
    expected_uid: u32,
) -> Result<(), DaemonError> {
    match UnixStream::connect(socket_path).await {
        Ok(_) => Err(DaemonError::Endpoint {
            message: format!(
                "a live endpoint exists despite holding the profile lock: {}",
                socket_path.display()
            ),
        }),
        // A missing node and a node the probe cannot follow (a dangling
        // symlink, for one) both report NotFound, so check the name itself
        // before assuming a clean cold start: a non-socket squatter is refused
        // here — never removed, never bound over.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match rustix::fs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW) {
                Err(Errno::NOENT) => Ok(()),
                Ok(_) => Err(DaemonError::Endpoint {
                    message: format!(
                        "refusing to bind over unverified endpoint {}",
                        socket_path.display()
                    ),
                }),
                Err(error) => Err(DaemonError::io(
                    "lstat endpoint name",
                    socket_path,
                    error.into(),
                )),
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {
            remove_verified_stale(directory, socket_path, name, expected_uid).await
        }
        Err(error) => Err(DaemonError::io("probe Unix socket", socket_path, error)),
    }
}

/// The conservative half of R3. The candidate is claimed under a private name
/// first, so ownership verification, the liveness re-probe, and the unlink all
/// act on one object nobody else can reach. Anything that fails a check is
/// renamed back exactly where it was rather than removed — a wrong unlink here
/// could take down a healthy daemon's endpoint.
async fn remove_verified_stale(
    directory: &OwnedFd,
    socket_path: &Path,
    name: &str,
    expected_uid: u32,
) -> Result<(), DaemonError> {
    let claim = staging_name(name)?;
    match rustix::fs::renameat(directory, name, directory, claim.as_str()) {
        Ok(()) => {}
        // Someone else already removed the stale node; the name is clean.
        Err(Errno::NOENT) => return Ok(()),
        Err(error) => {
            return Err(DaemonError::io(
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
            restore(directory, &claim, name);
            return Err(DaemonError::io(
                "lstat stale Unix socket",
                socket_path,
                error.into(),
            ));
        }
    };
    if FileType::from_raw_mode(stat.st_mode) != FileType::Socket || stat.st_uid != expected_uid {
        restore(directory, &claim, name);
        return Err(DaemonError::Endpoint {
            message: format!(
                "refusing to remove unverified endpoint {}",
                socket_path.display()
            ),
        });
    }
    // Re-probe under the claimed name: if a daemon bound this node between the
    // preflight probe and the claim, it is LIVE and must be restored, never
    // unlinked.
    let claim_path = socket_path.with_file_name(&claim);
    match UnixStream::connect(&claim_path).await {
        Ok(_) => {
            restore(directory, &claim, name);
            Err(DaemonError::Endpoint {
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
                Err(error) => Err(DaemonError::io(
                    "remove stale Unix socket",
                    socket_path,
                    error.into(),
                )),
            }
        }
        Err(error) => {
            restore(directory, &claim, name);
            Err(DaemonError::io(
                "probe claimed Unix socket",
                &claim_path,
                error,
            ))
        }
    }
}

/// Best-effort undo of a claim: the node goes back to the name it came from.
fn restore(directory: &OwnedFd, claim: &str, name: &str) {
    let _ = rustix::fs::renameat(directory, claim, directory, name);
}

/// A private, unguessable sibling name in the same directory. Nothing else can
/// reach it, which is what makes verify-then-act sequences on it race-free.
fn staging_name(name: &str) -> Result<String, DaemonError> {
    let mut bytes = [0_u8; 4];
    getrandom::fill(&mut bytes).map_err(|error| DaemonError::Task {
        message: format!("cannot generate endpoint staging name: {error}"),
    })?;
    let mut staging = String::with_capacity(name.len() + 10);
    staging.push_str(name);
    staging.push_str(".t");
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut staging, "{byte:02x}").map_err(|error| DaemonError::Task {
            message: format!("cannot format endpoint staging name: {error}"),
        })?;
    }
    Ok(staging)
}

fn endpoint_name(socket_path: &Path) -> Result<String, DaemonError> {
    socket_path
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .ok_or_else(|| DaemonError::Endpoint {
            message: format!("endpoint path {} has no file name", socket_path.display()),
        })
}

/// Device+inode of a stat result. The widths differ per platform (`dev_t` is
/// signed on Apple), and these values are only ever compared against
/// identities produced by this same function.
fn identity_of(stat: &Stat) -> SocketIdentity {
    SocketIdentity {
        device: widen(stat.st_dev),
        inode: widen(stat.st_ino),
    }
}

/// Platform-width-independent widening for stat fields (generic so it stays
/// correct where the field is already `u64` and where it is signed).
fn widen(value: impl TryInto<u64>) -> u64 {
    value.try_into().unwrap_or(u64::MAX)
}
