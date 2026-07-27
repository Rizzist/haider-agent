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
//! - **bind → identity**: the socket is created under an UNGUESSABLE sibling
//!   name (128 CSPRNG bits, [`staging_name`]), `statat`-ed there, and only then
//!   moved onto the public name with a non-replacing rename. A racer cannot
//!   name — and therefore cannot replace — the node whose device+inode is
//!   recorded, so that identity is provably the one this daemon created.
//! - **identity → unlink**: cleanup looks first (an already-replaced node is
//!   left completely alone, with no rename at all), then *claims* the public
//!   name by renaming it to a fresh unguessable name. Identity is verified, and
//!   the unlink performed, on that name. A replacement that lands after the
//!   claim creates a brand-new node at the public name this daemon never
//!   touches again.
//! - **restore** ([`restore`]) is non-replacing and reports failure: a third
//!   node that appeared at the public name is never overwritten. A node left
//!   stranded under a staging name is swept, with the same ownership and
//!   liveness checks, at the next start ([`sweep_staging`]).
//!
//! Residual, stated precisely:
//!
//! 1. If a same-UID process creates a node at the public name between our claim
//!    and our restore, the restore is REFUSED and the claimed node stays under
//!    its unguessable staging name until a later sweep. Nothing of anyone
//!    else's is deleted or overwritten, but a live foreign socket claimed in
//!    that window loses its public path until its owner rebinds.
//! 2. Guessing is the only way to target the staging window, and it is 128 bits
//!    wide; a brute-force attempt would also have to land inside a
//!    microsecond-scale window.
//! 3. All of this describes a same-UID process DELIBERATELY racing the daemon.
//!    A normally-starting peer daemon cannot reach bind at all — the store's
//!    profile lifetime lock is taken first and released last (R1) — so
//!    accidental successor deletion is prevented one layer up.
//! 4. [`rename_no_replace`] is native only on Apple/Linux; other Unix targets
//!    fall back to check-then-rename, which keeps a replacing race in publish
//!    and restore. That platform gap and its trigger are stated at that
//!    function.

use crate::{DaemonConfig, DaemonError};
use rustix::fs::{AtFlags, FileType, Mode, OFlags, Stat};
use rustix::io::Errno;
use std::fs;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::time::Duration;
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
    /// Claims the public name, verifies identity on the claimed (unguessable)
    /// name, and unlinks only there. A node that is not ours goes back exactly
    /// where it was, which is what keeps an old daemon from deleting its
    /// successor's socket (R22 named case: successor-socket-deletion).
    fn remove_owned(&mut self) -> Result<(), DaemonError> {
        if !self.active {
            return Ok(());
        }
        self.active = false;
        // Look before claiming: in the ordinary successor case the node at the
        // public name is already someone else's, and this check means it is
        // never moved at all — no claim, no restore, no window.
        match rustix::fs::statat(
            &self.directory,
            self.name.as_str(),
            AtFlags::SYMLINK_NOFOLLOW,
        ) {
            Ok(stat) if identity_of(&stat) != self.identity => return Ok(()),
            Ok(_) => {}
            Err(Errno::NOENT) => return Ok(()),
            Err(error) => {
                return Err(DaemonError::io(
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
                    restore(&self.directory, &claim, &self.name)?;
                    return Err(DaemonError::io(
                        "lstat claimed socket",
                        &self.path,
                        error.into(),
                    ));
                }
            };
        if identity_of(&stat) != self.identity {
            // A successor replaced the node between the look and the claim:
            // put it back and never unlink it.
            return restore(&self.directory, &claim, &self.name);
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
    // Clear leftovers from a daemon that died mid-claim before deciding what
    // the public name is: a stranded staging node is ours by construction, and
    // sweeping it here keeps the directory from accumulating garbage.
    sweep_staging(&directory, &config.runtime_dir, owner_uid).await?;
    preflight(&directory, &socket_path, &name, owner_uid).await?;

    let staging = staging_name()?;
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
fn publish(directory: &OwnedFd, staging: &str, name: &str) -> Result<(), DaemonError> {
    match rename_no_replace(directory, staging, name) {
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

/// Rename that refuses to replace an existing node.
///
/// Native (`RENAME_EXCL` on Apple, `RENAME_NOREPLACE` on Linux) where the
/// platform provides it. PLATFORM GAP, stated plainly: every other Unix target
/// falls back to check-then-rename, whose window a same-UID process can use to
/// have its node at the destination replaced instead of the operation being
/// refused. Trigger: a racer creating a node at the public (publish) or claimed
/// (restore) name between the check and the rename. macOS and Linux — the only
/// targets this workspace builds — take the native path.
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
    match probe(socket_path).await {
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
        // A probe that never settled is resolved as LIVE: refuse, touch nothing.
        Err(error) if error.kind() == std::io::ErrorKind::TimedOut => Err(DaemonError::Endpoint {
            message: format!(
                "endpoint {} did not answer its liveness probe; treating it as live",
                socket_path.display()
            ),
        }),
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
    let claim = staging_name()?;
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
            restore(directory, &claim, name)?;
            return Err(DaemonError::io(
                "lstat stale Unix socket",
                socket_path,
                error.into(),
            ));
        }
    };
    if FileType::from_raw_mode(stat.st_mode) != FileType::Socket || stat.st_uid != expected_uid {
        restore(directory, &claim, name)?;
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
    match probe(&claim_path).await {
        Ok(_) => {
            restore(directory, &claim, name)?;
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
        // Same conservative resolution under the claimed name.
        Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {
            restore(directory, &claim, name)?;
            Err(DaemonError::Endpoint {
                message: format!(
                    "claimed endpoint {} did not answer its liveness probe; treating it as live",
                    socket_path.display()
                ),
            })
        }
        Err(error) => {
            restore(directory, &claim, name)?;
            Err(DaemonError::io(
                "probe claimed Unix socket",
                &claim_path,
                error,
            ))
        }
    }
}

/// How long a liveness probe may take before the node counts as LIVE.
///
/// `connect(2)` to a Unix socket whose listener never accepts BLOCKS once the
/// backlog fills, so an unbounded probe could hang daemon startup on a peer
/// that is alive but wedged. Timing out is resolved conservatively — the node
/// is treated as live, which means "leave it completely alone".
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Connect probe with that bound. `Ok` means live; the error kinds carry the
/// same meaning they do for a plain connect.
async fn probe(path: &Path) -> std::io::Result<UnixStream> {
    match tokio::time::timeout(PROBE_TIMEOUT, UnixStream::connect(path)).await {
        Ok(result) => result,
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "endpoint probe did not settle; treating the node as live",
        )),
    }
}

/// Puts a claimed node back where it came from, NEVER over something else.
///
/// A replacing rename here could destroy a third node that appeared at the
/// public name while we held the claim, so the restore refuses to replace and
/// reports its failure instead of hiding it. The claimed node then stays under
/// its staging name — visible to [`sweep_staging`] on the next start — which is
/// the conservative outcome: nothing of anyone else's is deleted or overwritten.
fn restore(directory: &OwnedFd, claim: &str, name: &str) -> Result<(), DaemonError> {
    match rename_no_replace(directory, claim, name) {
        Ok(()) => Ok(()),
        Err(Errno::EXIST) => Err(DaemonError::Endpoint {
            message: format!(
                "cannot restore claimed endpoint {claim}: another node now occupies {name}"
            ),
        }),
        Err(Errno::NOENT) => Ok(()),
        Err(error) => Err(DaemonError::io(
            "restore claimed endpoint",
            Path::new(claim),
            error.into(),
        )),
    }
}

/// Prefix of every staging/claim name this daemon creates. Kept short so the
/// full path stays well inside the `sun_path` limit, and distinct so
/// [`sweep_staging`] can recognise leftovers by construction.
const STAGING_PREFIX: &str = ".haiderd-";

/// An unguessable sibling name in the same directory.
///
/// 128 CSPRNG bits: this is the load-bearing property of the claim discipline.
/// A same-UID racer cannot replace a node it cannot name, so claim → verify →
/// unlink acts on one object that only this daemon can reach. (A 32-bit name
/// would be enumerable, which is precisely what made the earlier version
/// raceable.)
fn staging_name() -> Result<String, DaemonError> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| DaemonError::Task {
        message: format!("cannot generate endpoint staging name: {error}"),
    })?;
    let mut staging = String::with_capacity(STAGING_PREFIX.len() + bytes.len() * 2);
    staging.push_str(STAGING_PREFIX);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut staging, "{byte:02x}").map_err(|error| DaemonError::Task {
            message: format!("cannot format endpoint staging name: {error}"),
        })?;
    }
    Ok(staging)
}

/// Removes staging leftovers from a daemon that died between claim and restore.
///
/// Everything under [`STAGING_PREFIX`] was created by a daemon of this user in
/// this directory, but "by construction" is not proof, so each candidate must
/// still be an owner-matched socket AND refuse a connect probe before it is
/// unlinked — a live node (a concurrent daemon's staging socket, or a node
/// stranded by a failed restore that is still being served) is left alone.
async fn sweep_staging(
    directory: &OwnedFd,
    runtime_dir: &Path,
    expected_uid: u32,
) -> Result<(), DaemonError> {
    let entries = {
        let mut directory = rustix::fs::Dir::read_from(directory).map_err(|error| {
            DaemonError::io("read runtime directory", runtime_dir, error.into())
        })?;
        let mut names = Vec::new();
        while let Some(entry) = directory.read() {
            let entry = entry.map_err(|error| {
                DaemonError::io("read runtime dir entry", runtime_dir, error.into())
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
            // Refused: a socket node with no listener, i.e. our own leftover.
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {
                let _ = rustix::fs::unlinkat(directory, name.as_str(), AtFlags::empty());
            }
            // Live, vanished, or unreadable: leave it exactly as it is.
            _ => {}
        }
    }
    Ok(())
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
