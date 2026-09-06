//! Durable executable-bundle commit, rollback, and v1/v2 crash recovery.

use super::UpdateError;
use super::members::{ALL_BUNDLE_MEMBERS, BUNDLE_MEMBERS, BundleMember};
use super::staging::{
    StageVerifier, SystemStageVerifier, VerifiedStagedPair, bounded_command_output, sha256_file,
    sync_dir,
};
use serde_json::{Value, json};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const LOCK_NAME: &str = ".haider-update.lock";
const MARKER_NAME: &str = ".haider-update-transaction.json";

#[derive(Debug, Clone)]
pub struct InstallLayout {
    pub dir: PathBuf,
    pub haider: PathBuf,
    pub haiderd: PathBuf,
    pub haider_tui: PathBuf,
    pub wayland_portal: PathBuf,
}

impl InstallLayout {
    pub fn running() -> Result<Self, UpdateError> {
        let running = std::env::current_exe()
            .map_err(|error| UpdateError::io("resolve running executable", error))?;
        Self::from_executable(&running)
    }

    fn from_executable(running: &Path) -> Result<Self, UpdateError> {
        if ![BundleMember::Cli.file_name(), BundleMember::Tui.file_name()].contains(
            &running
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default(),
        ) {
            return Err(UpdateError::Refused(format!(
                "running executable is not an expected haider installation: {}",
                running.display()
            )));
        }
        let dir = running
            .parent()
            .ok_or_else(|| {
                UpdateError::Refused("running executable has no install directory".into())
            })?
            .to_path_buf();
        let layout = Self::in_directory(dir);
        if layout.dir.join(MARKER_NAME).exists() {
            layout.validate_for_recovery()?;
        } else {
            layout.validate()?;
        }
        Ok(layout)
    }

    fn in_directory(dir: PathBuf) -> Self {
        Self {
            haider: dir.join(BundleMember::Cli.file_name()),
            haiderd: dir.join(BundleMember::Daemon.file_name()),
            haider_tui: dir.join(BundleMember::Tui.file_name()),
            wayland_portal: dir.join(BundleMember::WaylandPortal.file_name()),
            dir,
        }
    }

    pub fn path(&self, member: BundleMember) -> &Path {
        match member {
            BundleMember::Daemon => &self.haiderd,
            BundleMember::Tui => &self.haider_tui,
            BundleMember::Cli => &self.haider,
            BundleMember::WaylandPortal => &self.wayland_portal,
        }
    }

    pub fn validate(&self) -> Result<(), UpdateError> {
        self.validate_for_recovery()?;
        let cli_present = installed_metadata(&self.haider, BundleMember::Cli)?.is_some();
        let daemon_present = installed_metadata(&self.haiderd, BundleMember::Daemon)?.is_some();
        let tui_present = installed_metadata(&self.haider_tui, BundleMember::Tui)?.is_some();
        if cli_present != daemon_present || (!cli_present && tui_present) {
            return Err(UpdateError::Refused("installation is incomplete: CLI and daemon must both exist, or the entire bundle must be absent".into()));
        }
        for member in ALL_BUNDLE_MEMBERS {
            let path = self.path(member);
            let name = member.name();
            let Some(metadata) = installed_metadata(path, member)? else {
                continue;
            };
            if haider_platform::metadata_mode(&metadata) & 0o200 == 0 {
                return Err(UpdateError::Refused(format!(
                    "installed `{name}` is read-only"
                )));
            }
            if haider_platform::metadata_link_count(&metadata) != 1 {
                return Err(UpdateError::Refused(format!(
                    "installed `{name}` is hard-linked or managed"
                )));
            }
        }
        Ok(())
    }

    fn validate_for_recovery(&self) -> Result<(), UpdateError> {
        validate_real_directory(&self.dir)?;
        for member in ALL_BUNDLE_MEMBERS {
            if installed_metadata(self.path(member), member)?.is_some() {
                validate_recoverable_binary(self.path(member), member.name())?;
            }
        }
        Ok(())
    }

    /// A fresh directory is admitted only when all executable paths are absent;
    /// an existing historic pair may omit only the new payload.
    pub fn for_install_directory(dir: PathBuf) -> Result<Self, UpdateError> {
        let layout = Self::in_directory(dir);
        if layout.dir.join(MARKER_NAME).exists() {
            layout.validate_for_recovery()?;
        } else {
            layout.validate()?;
        }
        Ok(layout)
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub fn for_test(dir: PathBuf) -> Self {
        Self::in_directory(dir)
    }

    #[cfg(test)]
    pub fn from_executable_for_test(running: &Path) -> Result<Self, UpdateError> {
        Self::from_executable(running)
    }
}

/// The first migration starts from the historic CLI/daemon pair. Absence of
/// its new payload is recorded explicitly in v2 and restored as absence on
/// rollback. Existing payloads still receive every ownership/link/mode check.
fn installed_metadata(
    path: &Path,
    _member: BundleMember,
) -> Result<Option<fs::Metadata>, UpdateError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(UpdateError::io("inspect installed binary", error)),
    }
}

fn validate_real_directory(path: &Path) -> Result<(), UpdateError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| UpdateError::io("inspect install directory", error))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(UpdateError::Refused(
            "install directory is symlinked or is not a directory".into(),
        ));
    }
    if !haider_platform::metadata_is_current_user(&metadata)
        || haider_platform::metadata_mode(&metadata) & 0o200 == 0
        || haider_platform::metadata_mode(&metadata) & 0o022 != 0
    {
        return Err(UpdateError::Refused(
            "install directory is managed, read-only, or not owned by this user".into(),
        ));
    }
    Ok(())
}

fn validate_recoverable_binary(path: &Path, name: &str) -> Result<(), UpdateError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| UpdateError::io("inspect installed binary", error))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(UpdateError::Refused(format!(
            "installed `{name}` is symlinked or not a regular file"
        )));
    }
    if !haider_platform::metadata_is_current_user(&metadata)
        || haider_platform::metadata_mode(&metadata) & 0o100 == 0
        || haider_platform::metadata_mode(&metadata) & 0o022 != 0
    {
        return Err(UpdateError::Refused(format!(
            "installed `{name}` is managed or not owner-executable"
        )));
    }
    Ok(())
}

/// Exclusively flocked update-lock descriptor released by explicit unlock.
///
/// Dropping the `File` alone is not a reliable release: `flock` locks belong
/// to the open file description, and fork/posix_spawn duplicates the whole
/// descriptor table, so a concurrently spawned child between clone and exec
/// keeps the description — and its lock — alive after this process closes its
/// own descriptor (`O_CLOEXEC` only applies at exec). Explicit `LOCK_UN`
/// releases the description's lock immediately regardless of surviving
/// duplicates, so an immediate re-acquire cannot be spuriously refused as a
/// concurrent update.
struct UpdateLock(File);

impl Drop for UpdateLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

pub struct PreparedTransaction {
    layout: InstallLayout,
    _lock: UpdateLock,
}

impl PreparedTransaction {
    /// Acquires the real update lock and restores an interrupted pair before
    /// admitting a new transaction.
    ///
    /// MUTATION SAFETY: this may restore paths named by a valid durable
    /// marker. Runtime recovery failures retain the marker and every usable
    /// backup; they never accept a mixed canonical pair.
    pub fn acquire(layout: InstallLayout) -> Result<Self, UpdateError> {
        if layout.dir.join(MARKER_NAME).exists() {
            layout.validate_for_recovery()?;
        } else {
            layout.validate()?;
        }
        let lock_path = layout.dir.join(LOCK_NAME);
        if fs::symlink_metadata(&lock_path).is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            return Err(UpdateError::Refused("update lock is a symlink".into()));
        }
        let mut lock_options = OpenOptions::new();
        lock_options
            .read(true)
            .write(true)
            .create(true)
            .truncate(false);
        haider_platform::configure_file_mode(&mut lock_options, 0o600);
        let lock = lock_options
            .open(&lock_path)
            .map_err(|error| UpdateError::io("open update lock", error))?;
        let metadata = lock
            .metadata()
            .map_err(|error| UpdateError::io("inspect update lock", error))?;
        if !haider_platform::metadata_is_current_user(&metadata)
            || haider_platform::metadata_mode(&metadata) & 0o077 != 0
        {
            return Err(UpdateError::Refused(
                "update lock is not owner-only and owned by this user".into(),
            ));
        }
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(UpdateError::Refused(
                    "another haider update is already running".into(),
                ));
            }
            Err(TryLockError::Error(error)) => {
                return Err(UpdateError::io("lock update transaction", error));
            }
        }
        let lock = UpdateLock(lock);
        recover_pending(&layout)?;
        layout.validate()?;
        Ok(Self {
            layout,
            _lock: lock,
        })
    }

    /// Returns a same-open-file-description duplicate of the held lock
    /// descriptor, exactly what fork/posix_spawn leaves in a child's file
    /// table between clone and exec.
    #[cfg(test)]
    #[allow(dead_code)]
    pub fn duplicate_lock_handle_for_test(&self) -> std::io::Result<File> {
        self._lock.0.try_clone()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitBoundary {
    BackupDaemon,
    BackupTui,
    BackupPortal,
    BackupCli,
    RenameDaemon,
    RenameTui,
    RenamePortal,
    RenameCli,
    ChmodDaemon,
    ChmodTui,
    ChmodPortal,
    ChmodCli,
    InstallDirFsync,
    InstalledPairVerify,
}

pub trait FaultInjector {
    fn after(&self, boundary: CommitBoundary) -> Result<(), UpdateError>;
}

pub struct NoFaults;

impl FaultInjector for NoFaults {
    fn after(&self, _boundary: CommitBoundary) -> Result<(), UpdateError> {
        Ok(())
    }
}

pub trait InstalledPairVerifier {
    fn verify(&self, layout: &InstallLayout, pair: &VerifiedStagedPair) -> Result<(), UpdateError>;
}

pub struct SystemInstalledPairVerifier;

impl InstalledPairVerifier for SystemInstalledPairVerifier {
    fn verify(&self, layout: &InstallLayout, pair: &VerifiedStagedPair) -> Result<(), UpdateError> {
        for member in pair.members() {
            let binary = layout.path(member);
            let name = member.name();
            if sha256_file(binary)? != pair.digest(member) {
                return Err(UpdateError::Refused(format!(
                    "canonical {name} does not match its verified staged digest"
                )));
            }
            SystemStageVerifier.verify_signature(binary)?;
            let version = bounded_command_output(
                Command::new(binary).arg("--version"),
                4096,
                "verify installed executable version",
            )?;
            if version != format!("{name} {}\n", pair.version()).as_bytes() {
                return Err(UpdateError::Refused(format!(
                    "installed {name} version does not match the target"
                )));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionPhase {
    Prepared,
    BackupDaemon,
    BackupTui,
    BackupPortal,
    BackupsReady,
    DaemonInstalled,
    TuiInstalled,
    PortalInstalled,
    PairInstalled,
    DaemonWritable,
    TuiWritable,
    PortalWritable,
    PairWritable,
    PairSynced,
    PairVerified,
    RestartPending,
    DrainSignaled,
    LockReleased,
    ChildSpawned,
    Finalizing,
    RollingBack,
}

impl TransactionPhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::BackupDaemon => "backup_daemon",
            Self::BackupTui => "backup_tui",
            Self::BackupPortal => "backup_portal",
            Self::BackupsReady => "backups_ready",
            Self::DaemonInstalled => "daemon_installed",
            Self::TuiInstalled => "tui_installed",
            Self::PortalInstalled => "portal_installed",
            Self::PairInstalled => "pair_installed",
            Self::DaemonWritable => "daemon_writable",
            Self::TuiWritable => "tui_writable",
            Self::PortalWritable => "portal_writable",
            Self::PairWritable => "pair_writable",
            Self::PairSynced => "pair_synced",
            Self::PairVerified => "pair_verified",
            Self::RestartPending => "restart_pending",
            Self::DrainSignaled => "drain_signaled",
            Self::LockReleased => "lock_released",
            Self::ChildSpawned => "child_spawned",
            Self::Finalizing => "finalizing",
            Self::RollingBack => "rolling_back",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "prepared" => Self::Prepared,
            "backup_daemon" => Self::BackupDaemon,
            "backup_tui" => Self::BackupTui,
            "backup_portal" => Self::BackupPortal,
            "backups_ready" => Self::BackupsReady,
            "daemon_installed" => Self::DaemonInstalled,
            "tui_installed" => Self::TuiInstalled,
            "portal_installed" => Self::PortalInstalled,
            "pair_installed" => Self::PairInstalled,
            "daemon_writable" => Self::DaemonWritable,
            "tui_writable" => Self::TuiWritable,
            "portal_writable" => Self::PortalWritable,
            "pair_writable" => Self::PairWritable,
            "pair_synced" => Self::PairSynced,
            "pair_verified" => Self::PairVerified,
            "restart_pending" => Self::RestartPending,
            "drain_signaled" => Self::DrainSignaled,
            "lock_released" => Self::LockReleased,
            "child_spawned" => Self::ChildSpawned,
            "finalizing" => Self::Finalizing,
            "rolling_back" => Self::RollingBack,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone)]
struct MarkerMember {
    member: BundleMember,
    old_digest: Option<String>,
    target_digest: String,
    backup: String,
}

#[derive(Debug, Clone)]
struct Marker {
    legacy: bool,
    transaction_id: String,
    old_version: String,
    target_version: String,
    source_archive_digest: String,
    members: Vec<MarkerMember>,
    phase: TransactionPhase,
}

impl Marker {
    fn matches_target(&self, layout: &InstallLayout) -> bool {
        self.members.iter().all(|entry| {
            sha256_file(layout.path(entry.member)).is_ok_and(|hash| hash == entry.target_digest)
        })
    }

    fn matches_old(&self, layout: &InstallLayout) -> bool {
        self.members.iter().all(|entry| match &entry.old_digest {
            Some(digest) => {
                sha256_file(layout.path(entry.member)).is_ok_and(|hash| hash == *digest)
            }
            None => fs::symlink_metadata(layout.path(entry.member))
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
        })
    }

    fn remove_backups(&self, layout: &InstallLayout) -> Result<(), UpdateError> {
        for entry in &self.members {
            remove_if_exists(
                &layout.dir.join(&entry.backup),
                "remove update recovery backup",
            )?;
        }
        sync_dir(&layout.dir)
    }
}

pub struct CommittedUpdate {
    layout: InstallLayout,
    marker: Marker,
    _lock: UpdateLock,
}

impl CommittedUpdate {
    pub fn layout(&self) -> &InstallLayout {
        &self.layout
    }

    pub fn target_version(&self) -> &str {
        &self.marker.target_version
    }

    pub fn old_version(&self) -> &str {
        &self.marker.old_version
    }

    /// Re-reads every live bundle member. Restart calls this as its first operation,
    /// making entry impossible for a one-swap or failed-verification state.
    pub fn verify_target_pair(&self) -> Result<(), UpdateError> {
        if self.marker.matches_target(&self.layout) {
            Ok(())
        } else {
            Err(UpdateError::Internal(
                "restart entry did not observe the exact verified target bundle".into(),
            ))
        }
    }

    pub fn set_phase(&mut self, phase: TransactionPhase) -> Result<(), UpdateError> {
        self.marker.phase = phase;
        write_marker(&self.layout, &self.marker)
    }

    /// Restores old hard-linked inodes and any recorded payload absence.
    ///
    /// MUTATION SAFETY: runtime failures retain the marker and any remaining
    /// backup. Success fsyncs the exact old pair before removing the marker.
    pub fn rollback(&mut self) -> Result<(), UpdateError> {
        rollback_marker(&self.layout, &mut self.marker)?;
        Ok(())
    }

    /// Commits health success through a durable finalizing phase.
    ///
    /// MUTATION SAFETY: recovery recognizes this phase only when all
    /// canonical paths still match the target digests, and can finish deleting
    /// any remaining backups before removing the marker. Thus no crash window
    /// leaves anonymous hard links that poison a later layout validation.
    pub fn finalize(&mut self) -> Result<(), UpdateError> {
        self.verify_target_pair()?;
        self.set_phase(TransactionPhase::Finalizing)?;
        let marker_path = self.layout.dir.join(MARKER_NAME);
        self.marker.remove_backups(&self.layout)?;
        fs::remove_file(&marker_path)
            .map_err(|error| UpdateError::io("remove successful update marker", error))?;
        sync_dir(&self.layout.dir)?;
        Ok(())
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub fn recovery_assets_exist(&self) -> bool {
        self.layout.dir.join(MARKER_NAME).exists()
            && self.marker.members.iter().all(|entry| {
                entry.old_digest.is_none() || self.layout.dir.join(&entry.backup).exists()
            })
    }
}

/// Publishes daemon, payload, then CLI using one descriptor-driven transaction.
///
/// MUTATION SAFETY: every runtime failure before restart calls rollback while
/// the old daemon remains unsignaled. Each named durability boundary invokes
/// the fault hook only after its operation and marker are durable.
pub fn commit_pair<F: FaultInjector, V: InstalledPairVerifier>(
    prepared: PreparedTransaction,
    pair: VerifiedStagedPair,
    faults: &F,
    verifier: &V,
    old_version: &str,
) -> Result<CommittedUpdate, UpdateError> {
    // The capability is re-read before the marker or first hard-link backup.
    // A same-user accidental alteration therefore cannot transiently reach a
    // canonical path and rely on post-swap rollback for detection.
    pair.verify_immutable()?;
    let PreparedTransaction { layout, _lock } = prepared;
    // Keep an existing install's access policy, including a newly introduced
    // payload alongside an owner-only historic CLI. Fresh package installs
    // retain the installers' public executable mode. Private staging, lock
    // and marker permissions are independent of these canonical file modes.
    let new_member_mode = installed_metadata(&layout.haider, BundleMember::Cli)?
        .map_or(0o755, |metadata| {
            haider_platform::metadata_mode(&metadata) & 0o777
        });
    let publication_modes = pair
        .members()
        .map(|member| {
            let mode = installed_metadata(layout.path(member), member)?
                .map_or(new_member_mode, |metadata| {
                    haider_platform::metadata_mode(&metadata) & 0o777
                });
            Ok((member, mode))
        })
        .collect::<Result<Vec<_>, UpdateError>>()?;
    let transaction_id = transaction_id();
    let members = pair
        .members()
        .map(|member| {
            let old_digest = installed_metadata(layout.path(member), member)?
                .map(|_| sha256_file(layout.path(member)))
                .transpose()?;
            Ok(MarkerMember {
                member,
                old_digest,
                target_digest: pair.digest(member).to_owned(),
                backup: format!("{}{transaction_id}", member.backup_prefix()),
            })
        })
        .collect::<Result<Vec<_>, UpdateError>>()?;
    let mut marker = Marker {
        legacy: false,
        transaction_id,
        old_version: old_version.to_owned(),
        target_version: pair.version().to_owned(),
        source_archive_digest: pair.source_digest().to_owned(),
        members,
        phase: TransactionPhase::Prepared,
    };
    write_marker(&layout, &marker)?;

    let result = (|| {
        for member in pair.members() {
            let entry = marker
                .members
                .iter()
                .find(|entry| entry.member == member)
                .ok_or_else(|| {
                    UpdateError::Internal("verified member missing from transaction".into())
                })?;
            if entry.old_digest.is_some() {
                make_backup(layout.path(member), &layout.dir.join(&entry.backup))?;
            }
            let (phase, boundary) = match member {
                BundleMember::Daemon => {
                    (TransactionPhase::BackupDaemon, CommitBoundary::BackupDaemon)
                }
                BundleMember::Tui => (TransactionPhase::BackupTui, CommitBoundary::BackupTui),
                BundleMember::WaylandPortal => {
                    (TransactionPhase::BackupPortal, CommitBoundary::BackupPortal)
                }
                BundleMember::Cli => (TransactionPhase::BackupsReady, CommitBoundary::BackupCli),
            };
            marker.phase = phase;
            write_marker(&layout, &marker)?;
            faults.after(boundary)?;
        }

        for member in pair.members() {
            fs::rename(pair.path(member), layout.path(member)).map_err(|error| {
                UpdateError::io("rename staged bundle executable into place", error)
            })?;
            let (phase, boundary) = match member {
                BundleMember::Daemon => (
                    TransactionPhase::DaemonInstalled,
                    CommitBoundary::RenameDaemon,
                ),
                BundleMember::Tui => (TransactionPhase::TuiInstalled, CommitBoundary::RenameTui),
                BundleMember::WaylandPortal => (
                    TransactionPhase::PortalInstalled,
                    CommitBoundary::RenamePortal,
                ),
                BundleMember::Cli => (TransactionPhase::PairInstalled, CommitBoundary::RenameCli),
            };
            marker.phase = phase;
            write_marker(&layout, &marker)?;
            faults.after(boundary)?;
        }

        for &(member, mode) in &publication_modes {
            set_installed_mode(layout.path(member), mode)?;
            let (phase, boundary) = match member {
                BundleMember::Daemon => (
                    TransactionPhase::DaemonWritable,
                    CommitBoundary::ChmodDaemon,
                ),
                BundleMember::Tui => (TransactionPhase::TuiWritable, CommitBoundary::ChmodTui),
                BundleMember::WaylandPortal => (
                    TransactionPhase::PortalWritable,
                    CommitBoundary::ChmodPortal,
                ),
                BundleMember::Cli => (TransactionPhase::PairWritable, CommitBoundary::ChmodCli),
            };
            marker.phase = phase;
            write_marker(&layout, &marker)?;
            faults.after(boundary)?;
        }

        sync_dir(&layout.dir)?;
        marker.phase = TransactionPhase::PairSynced;
        write_marker(&layout, &marker)?;
        faults.after(CommitBoundary::InstallDirFsync)?;

        verifier.verify(&layout, &pair)?;
        marker.phase = TransactionPhase::PairVerified;
        write_marker(&layout, &marker)?;
        faults.after(CommitBoundary::InstalledPairVerify)?;
        marker.phase = TransactionPhase::RestartPending;
        write_marker(&layout, &marker)?;
        Ok(())
    })();

    if let Err(error) = result {
        if let Err(rollback) = rollback_marker(&layout, &mut marker) {
            return Err(UpdateError::Internal(format!(
                "update failed ({error}); rollback also failed ({rollback}); recovery assets retained"
            )));
        }
        return Err(error);
    }
    drop(pair);
    Ok(CommittedUpdate {
        layout,
        marker,
        _lock,
    })
}

fn set_installed_mode(path: &Path, mode: u32) -> Result<(), UpdateError> {
    haider_platform::set_mode(path, mode)
        .map_err(|error| UpdateError::io("make installed binary owner-writable", error))?;
    OpenOptions::new()
        .write(true)
        .open(path)
        .and_then(|file| file.sync_all())
        .map_err(|error| UpdateError::io("fsync installed binary permissions", error))
}

fn make_backup(source: &Path, backup: &Path) -> Result<(), UpdateError> {
    fs::hard_link(source, backup)
        .map_err(|error| UpdateError::io("create same-filesystem binary backup", error))?;
    // Existing backup bytes were already installed; Unix additionally flushes
    // their inode here. Windows cannot obtain a writable flush handle to an
    // executing image. Its directory-entry durability remains the documented
    // platform sync_directory limitation, and needs native crash testing.
    #[cfg(unix)]
    File::open(backup)
        .and_then(|file| file.sync_all())
        .map_err(|error| UpdateError::io("fsync binary backup", error))?;
    sync_dir(backup.parent().unwrap_or_else(|| Path::new(".")))
}

fn rollback_marker(layout: &InstallLayout, marker: &mut Marker) -> Result<(), UpdateError> {
    // Validate ALL restore sources before changing any canonical path. For a
    // migrated payload that previously did not exist, only our exact target
    // bytes may be removed; a changed file retains the marker for recovery.
    for entry in &marker.members {
        if let Some(old_digest) = &entry.old_digest {
            verify_restore_source(
                layout.path(entry.member),
                &layout.dir.join(&entry.backup),
                old_digest,
            )?;
        } else {
            verify_new_member_removable(layout, entry)?;
        }
    }
    marker.phase = TransactionPhase::RollingBack;
    let _ = write_marker(layout, marker);
    for entry in &marker.members {
        if let Some(old_digest) = &entry.old_digest {
            restore_one(
                layout.path(entry.member),
                &layout.dir.join(&entry.backup),
                old_digest,
            )?;
        } else {
            remove_installed_member(layout.path(entry.member))?;
        }
    }
    sync_dir(&layout.dir)?;
    if !marker.matches_old(layout) {
        return Err(UpdateError::Internal(
            "rollback did not restore the exact old executable bundle".into(),
        ));
    }
    // Backups are cleaned and synced while the marker still names them.
    marker.remove_backups(layout)?;
    remove_if_exists(&layout.dir.join(MARKER_NAME), "remove rollback marker")?;
    sync_dir(&layout.dir)
}

fn remove_installed_member(path: &Path) -> Result<(), UpdateError> {
    // Windows refuses deletion/replacement of a read-only staged file. This is
    // called only after all recovery sources and introduced bytes are checked.
    #[cfg(windows)]
    if path.exists() {
        set_installed_mode(path, 0o700)?;
    }
    remove_if_exists(path, "remove introduced executable during rollback")
}

fn verify_new_member_removable(
    layout: &InstallLayout,
    entry: &MarkerMember,
) -> Result<(), UpdateError> {
    let path = layout.path(entry.member);
    if fs::symlink_metadata(layout.dir.join(&entry.backup)).is_ok() {
        return Err(UpdateError::Internal(
            "unexpected backup for a previously absent executable".into(),
        ));
    }
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(UpdateError::io("inspect introduced executable", error)),
        Ok(metadata)
            if metadata.is_file()
                && !metadata.file_type().is_symlink()
                && haider_platform::metadata_is_current_user(&metadata)
                && sha256_file(path)? == entry.target_digest =>
        {
            Ok(())
        }
        Ok(_) => Err(UpdateError::Internal(
            "introduced executable no longer matches the transaction; recovery assets retained"
                .into(),
        )),
    }
}

fn verify_restore_source(
    canonical: &Path,
    backup: &Path,
    expected: &str,
) -> Result<(), UpdateError> {
    if backup.exists() {
        let metadata = fs::symlink_metadata(backup)
            .map_err(|error| UpdateError::io("inspect recovery backup", error))?;
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || !haider_platform::metadata_is_current_user(&metadata)
        {
            return Err(UpdateError::Internal(format!(
                "recovery backup {} is not a trusted regular file",
                backup.display()
            )));
        }
        if sha256_file(backup)? == expected {
            return Ok(());
        }
        return Err(UpdateError::Internal(format!(
            "recovery backup {} is corrupt",
            backup.display()
        )));
    }
    if canonical.exists() && sha256_file(canonical)? == expected {
        return Ok(());
    }
    Err(UpdateError::Internal(format!(
        "cannot restore {}; its backup is missing and canonical bytes are not old",
        canonical.display()
    )))
}

fn restore_one(canonical: &Path, backup: &Path, expected: &str) -> Result<(), UpdateError> {
    if backup.exists() {
        if sha256_file(backup)? != expected {
            return Err(UpdateError::Internal(format!(
                "recovery backup {} is corrupt",
                backup.display()
            )));
        }
        #[cfg(windows)]
        if canonical.exists() {
            set_installed_mode(canonical, 0o700)?;
        }
        fs::rename(backup, canonical)
            .map_err(|error| UpdateError::io("restore binary backup by rename", error))?;
        return Ok(());
    }
    if canonical.exists() && sha256_file(canonical)? == expected {
        return Ok(());
    }
    Err(UpdateError::Internal(format!(
        "cannot restore {}; its backup is missing and canonical bytes are not old",
        canonical.display()
    )))
}

fn recover_pending(layout: &InstallLayout) -> Result<(), UpdateError> {
    let marker_path = layout.dir.join(MARKER_NAME);
    let Some(mut marker) = read_marker(&marker_path)? else {
        return Ok(());
    };
    if (marker.phase == TransactionPhase::Finalizing && marker.matches_target(layout))
        || marker.matches_old(layout)
    {
        marker.remove_backups(layout)?;
        remove_if_exists(&marker_path, "finish recovered update marker cleanup")?;
        return sync_dir(&layout.dir);
    }
    rollback_marker(layout, &mut marker)
}

fn write_marker(layout: &InstallLayout, marker: &Marker) -> Result<(), UpdateError> {
    let path = layout.dir.join(MARKER_NAME);
    let part = layout
        .dir
        .join(format!(".{MARKER_NAME}.{}.part", marker.transaction_id));
    let mut value = json!({
        "schema": if marker.legacy { "haider.update.transaction.v1" } else { "haider.update.transaction.v2" },
        "transaction_id": marker.transaction_id,
        "old_version": marker.old_version,
        "target_version": marker.target_version,
        "source_archive_digest": marker.source_archive_digest,
        "phase": marker.phase.as_str(),
    });
    if marker.legacy {
        // Preserve v1 if rollback itself crashes: historical two-member markers
        // remain readable, and never acquire a fictitious payload dependency.
        for entry in &marker.members {
            let name = entry.member.name();
            value[format!("old_{name}_digest")] = json!(entry.old_digest);
            value[format!("target_{name}_digest")] = json!(entry.target_digest);
            value[format!("backup_{name}")] = json!(entry.backup);
        }
    } else {
        value["members"] = Value::Array(
            marker
                .members
                .iter()
                .map(|entry| {
                    json!({
                        "name": entry.member.name(),
                        "old_digest": entry.old_digest,
                        "target_digest": entry.target_digest,
                        "backup": entry.backup,
                    })
                })
                .collect(),
        );
    }
    let mut bytes = serde_json::to_vec(&value)
        .map_err(|error| UpdateError::Internal(format!("encode update marker: {error}")))?;
    bytes.push(b'\n');
    let mut marker_options = OpenOptions::new();
    marker_options.write(true).create_new(true);
    haider_platform::configure_file_mode(&mut marker_options, 0o600);
    let mut file = marker_options
        .open(&part)
        .map_err(|error| UpdateError::io("create durable update marker", error))?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| UpdateError::io("persist durable update marker", error))?;
    drop(file);
    fs::rename(&part, &path)
        .map_err(|error| UpdateError::io("publish durable update marker", error))?;
    sync_dir(&layout.dir)
}

fn read_marker(path: &Path) -> Result<Option<Marker>, UpdateError> {
    match fs::symlink_metadata(path) {
        Ok(metadata)
            if metadata.file_type().is_symlink()
                || !metadata.is_file()
                || !haider_platform::metadata_is_current_user(&metadata)
                || haider_platform::metadata_mode(&metadata) & 0o077 != 0 =>
        {
            return Err(UpdateError::Internal(
                "update recovery marker is not a trusted owner-only regular file".into(),
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(UpdateError::io("inspect update recovery marker", error)),
    }
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(UpdateError::io("read update recovery marker", error)),
    };
    if bytes.len() > 64 * 1024 {
        return Err(UpdateError::Internal(
            "update recovery marker exceeds its size bound".into(),
        ));
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(|error| {
        UpdateError::Internal(format!("invalid update recovery marker: {error}"))
    })?;
    let legacy = match value.get("schema").and_then(Value::as_str) {
        Some("haider.update.transaction.v1") => true,
        Some("haider.update.transaction.v2") => false,
        _ => {
            return Err(UpdateError::Internal(
                "unknown update recovery marker schema".into(),
            ));
        }
    };
    let field = |name: &str| -> Result<String, UpdateError> {
        value
            .get(name)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| UpdateError::Internal(format!("update marker lacks `{name}`")))
    };
    let phase_text = field("phase")?;
    let phase = TransactionPhase::parse(&phase_text)
        .ok_or_else(|| UpdateError::Internal("update marker has an unknown phase".into()))?;
    let transaction_id = field("transaction_id")?;
    if transaction_id.is_empty()
        || transaction_id.len() > 80
        || !transaction_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'-')
    {
        return Err(UpdateError::Internal(
            "update marker contains an unsafe transaction id".into(),
        ));
    }
    let members = if legacy {
        [BundleMember::Daemon, BundleMember::Cli]
            .into_iter()
            .map(|member| {
                let name = member.name();
                Ok(MarkerMember {
                    member,
                    old_digest: Some(digest_field(&field(&format!("old_{name}_digest"))?)?),
                    target_digest: digest_field(&field(&format!("target_{name}_digest"))?)?,
                    backup: field(&format!("backup_{name}"))?,
                })
            })
            .collect::<Result<Vec<_>, UpdateError>>()?
    } else {
        let entries = value
            .get("members")
            .and_then(Value::as_array)
            .ok_or_else(|| UpdateError::Internal("update marker lacks bundle members".into()))?;
        if !(BUNDLE_MEMBERS.len()..=ALL_BUNDLE_MEMBERS.len()).contains(&entries.len()) {
            return Err(UpdateError::Internal(
                "update marker has incomplete bundle membership".into(),
            ));
        }
        ALL_BUNDLE_MEMBERS
            .into_iter()
            .filter(|member| {
                *member != BundleMember::WaylandPortal
                    || entries.iter().any(|entry| {
                        entry.get("name").and_then(Value::as_str) == Some(member.name())
                    })
            })
            .map(|member| {
                let matching = entries
                    .iter()
                    .filter(|entry| {
                        entry.get("name").and_then(Value::as_str) == Some(member.name())
                    })
                    .collect::<Vec<_>>();
                if matching.len() != 1 {
                    return Err(UpdateError::Internal(
                        "update marker has missing or duplicate bundle members".into(),
                    ));
                }
                let entry = matching[0];
                let entry_field = |key: &str| {
                    entry.get(key).and_then(Value::as_str).ok_or_else(|| {
                        UpdateError::Internal(format!("update member lacks `{key}`"))
                    })
                };
                let old_digest = match entry.get("old_digest") {
                    Some(Value::Null) => None,
                    Some(Value::String(value)) => Some(digest_field(value)?),
                    _ => {
                        return Err(UpdateError::Internal(
                            "update member has invalid old digest".into(),
                        ));
                    }
                };
                Ok(MarkerMember {
                    member,
                    old_digest,
                    target_digest: digest_field(entry_field("target_digest")?)?,
                    backup: entry_field("backup")?.to_owned(),
                })
            })
            .collect::<Result<Vec<_>, UpdateError>>()?
    };
    if !legacy && members.len() != value["members"].as_array().map_or(0, Vec::len) {
        return Err(UpdateError::Internal(
            "update marker contains unknown optional members".into(),
        ));
    }
    if !legacy {
        let cli_old = members
            .iter()
            .find(|entry| entry.member == BundleMember::Cli)
            .is_some_and(|entry| entry.old_digest.is_some());
        let daemon_old = members
            .iter()
            .find(|entry| entry.member == BundleMember::Daemon)
            .is_some_and(|entry| entry.old_digest.is_some());
        if cli_old != daemon_old
            || (!cli_old
                && members
                    .iter()
                    .any(|entry| entry.member == BundleMember::Tui && entry.old_digest.is_some()))
        {
            return Err(UpdateError::Internal(
                "update marker records an incomplete old installation".into(),
            ));
        }
    }
    for entry in &members {
        if !safe_backup_name(&entry.backup, &entry.member.backup_prefix()) {
            return Err(UpdateError::Internal(
                "update marker contains an unsafe backup name".into(),
            ));
        }
    }
    let marker = Marker {
        legacy,
        transaction_id,
        old_version: field("old_version")?,
        target_version: field("target_version")?,
        source_archive_digest: digest_field(&field("source_archive_digest")?)?,
        members,
        phase,
    };
    Ok(Some(marker))
}

fn digest_field(value: &str) -> Result<String, UpdateError> {
    if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(value.to_ascii_lowercase())
    } else {
        Err(UpdateError::Internal(
            "update marker contains a malformed digest".into(),
        ))
    }
}

fn safe_backup_name(name: &str, prefix: &str) -> bool {
    name.starts_with(prefix)
        && Path::new(name).components().count() == 1
        && matches!(
            Path::new(name).components().next(),
            Some(Component::Normal(_))
        )
}

fn remove_if_exists(path: &Path, operation: &'static str) -> Result<(), UpdateError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(UpdateError::io(operation, error)),
    }
}

fn transaction_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    format!("{}-{nanos}", std::process::id())
}

#[cfg(test)]
#[allow(dead_code)]
pub fn marker_path(layout: &InstallLayout) -> PathBuf {
    layout.dir.join(MARKER_NAME)
}
