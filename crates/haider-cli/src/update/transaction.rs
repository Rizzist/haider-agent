//! Durable two-path commit, rollback, and crash recovery.

use super::UpdateError;
use super::staging::{VerifiedStagedPair, bounded_command_output, sha256_file, sync_dir};
use serde_json::{Value, json};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

const LOCK_NAME: &str = ".haider-update.lock";
const MARKER_NAME: &str = ".haider-update-transaction.json";
const CODESIGN: &str = "/usr/bin/codesign";

#[derive(Debug, Clone)]
pub(crate) struct InstallLayout {
    pub dir: PathBuf,
    pub haider: PathBuf,
    pub haiderd: PathBuf,
}

impl InstallLayout {
    pub fn running() -> Result<Self, UpdateError> {
        let haider = std::env::current_exe()
            .map_err(|error| UpdateError::io("resolve running executable", error))?;
        if haider.file_name().and_then(|name| name.to_str()) != Some("haider") {
            return Err(UpdateError::Refused(format!(
                "running executable is not an expected `haider` installation: {}",
                haider.display()
            )));
        }
        let dir = haider
            .parent()
            .ok_or_else(|| {
                UpdateError::Refused("running executable has no install directory".into())
            })?
            .to_path_buf();
        let layout = Self {
            haider,
            haiderd: dir.join("haiderd"),
            dir,
        };
        if layout.dir.join(MARKER_NAME).exists() {
            layout.validate_for_recovery()?;
        } else {
            layout.validate()?;
        }
        Ok(layout)
    }

    pub fn validate(&self) -> Result<(), UpdateError> {
        self.validate_for_recovery()?;
        for (path, name) in [(&self.haider, "haider"), (&self.haiderd, "haiderd")] {
            let metadata = fs::metadata(path)
                .map_err(|error| UpdateError::io("inspect installed binary link count", error))?;
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
        validate_recoverable_binary(&self.haider, "haider")?;
        validate_recoverable_binary(&self.haiderd, "haiderd")?;
        Ok(())
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub fn for_test(dir: PathBuf) -> Self {
        Self {
            haider: dir.join("haider"),
            haiderd: dir.join("haiderd"),
            dir,
        }
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

pub(crate) struct PreparedTransaction {
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
pub(crate) enum CommitBoundary {
    BackupDaemon,
    BackupCli,
    RenameDaemon,
    RenameCli,
    ChmodDaemon,
    ChmodCli,
    InstallDirFsync,
    InstalledPairVerify,
}

pub(crate) trait FaultInjector {
    fn after(&self, boundary: CommitBoundary) -> Result<(), UpdateError>;
}

pub(crate) struct NoFaults;

impl FaultInjector for NoFaults {
    fn after(&self, _boundary: CommitBoundary) -> Result<(), UpdateError> {
        Ok(())
    }
}

pub(crate) trait InstalledPairVerifier {
    fn verify(&self, layout: &InstallLayout, pair: &VerifiedStagedPair) -> Result<(), UpdateError>;
}

pub(crate) struct SystemInstalledPairVerifier;

impl InstalledPairVerifier for SystemInstalledPairVerifier {
    fn verify(&self, layout: &InstallLayout, pair: &VerifiedStagedPair) -> Result<(), UpdateError> {
        if sha256_file(&layout.haider)? != pair.haider_digest()
            || sha256_file(&layout.haiderd)? != pair.haiderd_digest()
        {
            return Err(UpdateError::Refused(
                "canonical binaries do not match the verified staged pair".into(),
            ));
        }
        for binary in [&layout.haider, &layout.haiderd] {
            let status = Command::new(CODESIGN)
                .args(["--verify", "--strict"])
                .arg(binary)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map_err(|error| UpdateError::io("verify installed signature", error))?;
            if !status.success() {
                return Err(UpdateError::Refused(
                    "installed signature verification failed".into(),
                ));
            }
        }
        let version = bounded_command_output(
            Command::new(&layout.haider).arg("--version"),
            4096,
            "verify installed haider version",
        )?;
        if version != format!("haider {}\n", pair.version()).as_bytes() {
            return Err(UpdateError::Refused(
                "installed haider version does not match the target".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransactionPhase {
    Prepared,
    BackupDaemon,
    BackupsReady,
    DaemonInstalled,
    PairInstalled,
    DaemonWritable,
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
            Self::BackupsReady => "backups_ready",
            Self::DaemonInstalled => "daemon_installed",
            Self::PairInstalled => "pair_installed",
            Self::DaemonWritable => "daemon_writable",
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
            "backups_ready" => Self::BackupsReady,
            "daemon_installed" => Self::DaemonInstalled,
            "pair_installed" => Self::PairInstalled,
            "daemon_writable" => Self::DaemonWritable,
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
struct Marker {
    transaction_id: String,
    old_version: String,
    target_version: String,
    old_haider_digest: String,
    old_haiderd_digest: String,
    target_haider_digest: String,
    target_haiderd_digest: String,
    source_archive_digest: String,
    backup_haider: String,
    backup_haiderd: String,
    phase: TransactionPhase,
}

pub(crate) struct CommittedUpdate {
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

    /// Re-reads both live paths. Restart calls this as its first operation,
    /// making entry impossible for a one-swap or failed-verification state.
    pub fn verify_target_pair(&self) -> Result<(), UpdateError> {
        if sha256_file(&self.layout.haider)? == self.marker.target_haider_digest
            && sha256_file(&self.layout.haiderd)? == self.marker.target_haiderd_digest
        {
            Ok(())
        } else {
            Err(UpdateError::Internal(
                "restart entry did not observe the exact verified target pair".into(),
            ))
        }
    }

    pub fn set_phase(&mut self, phase: TransactionPhase) -> Result<(), UpdateError> {
        self.marker.phase = phase;
        write_marker(&self.layout, &self.marker)
    }

    /// Restores both old hard-linked inodes by rename.
    ///
    /// MUTATION SAFETY: runtime failures retain the marker and any remaining
    /// backup. Success fsyncs the exact old pair before removing the marker.
    pub fn rollback(&mut self) -> Result<(), UpdateError> {
        rollback_marker(&self.layout, &mut self.marker)?;
        Ok(())
    }

    /// Commits health success through a durable finalizing phase.
    ///
    /// MUTATION SAFETY: recovery recognizes this phase only when both
    /// canonical paths still match the target digests, and can finish deleting
    /// any remaining backups before removing the marker. Thus no crash window
    /// leaves anonymous hard links that poison a later layout validation.
    pub fn finalize(&mut self) -> Result<(), UpdateError> {
        self.verify_target_pair()?;
        self.set_phase(TransactionPhase::Finalizing)?;
        let marker_path = self.layout.dir.join(MARKER_NAME);
        for backup in [
            self.marker.backup_haiderd.clone(),
            self.marker.backup_haider.clone(),
        ] {
            let path = self.layout.dir.join(backup);
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(UpdateError::io("remove successful update backup", error));
                }
            }
        }
        sync_dir(&self.layout.dir)?;
        fs::remove_file(&marker_path)
            .map_err(|error| UpdateError::io("remove successful update marker", error))?;
        sync_dir(&self.layout.dir)?;
        Ok(())
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub fn recovery_assets_exist(&self) -> bool {
        self.layout.dir.join(MARKER_NAME).exists()
            && self.layout.dir.join(&self.marker.backup_haider).exists()
            && self.layout.dir.join(&self.marker.backup_haiderd).exists()
    }
}

/// Performs the daemon-first, CLI-second rename transaction.
///
/// MUTATION SAFETY: every runtime failure before restart calls rollback while
/// the old daemon remains unsignaled. Each named durability boundary invokes
/// the fault hook only after its operation and marker are durable.
pub(crate) fn commit_pair<F: FaultInjector, V: InstalledPairVerifier>(
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
    let transaction_id = transaction_id();
    let mut marker = Marker {
        transaction_id: transaction_id.clone(),
        old_version: old_version.to_owned(),
        target_version: pair.version().to_owned(),
        old_haider_digest: sha256_file(&layout.haider)?,
        old_haiderd_digest: sha256_file(&layout.haiderd)?,
        target_haider_digest: pair.haider_digest().to_owned(),
        target_haiderd_digest: pair.haiderd_digest().to_owned(),
        source_archive_digest: pair.source_digest().to_owned(),
        backup_haider: format!(".haider-old-{transaction_id}"),
        backup_haiderd: format!(".haiderd-old-{transaction_id}"),
        phase: TransactionPhase::Prepared,
    };
    write_marker(&layout, &marker)?;

    let result = (|| {
        make_backup(&layout.haiderd, &layout.dir.join(&marker.backup_haiderd))?;
        marker.phase = TransactionPhase::BackupDaemon;
        write_marker(&layout, &marker)?;
        faults.after(CommitBoundary::BackupDaemon)?;

        make_backup(&layout.haider, &layout.dir.join(&marker.backup_haider))?;
        marker.phase = TransactionPhase::BackupsReady;
        write_marker(&layout, &marker)?;
        faults.after(CommitBoundary::BackupCli)?;

        fs::rename(pair.haiderd_path(), &layout.haiderd)
            .map_err(|error| UpdateError::io("rename staged haiderd into place", error))?;
        marker.phase = TransactionPhase::DaemonInstalled;
        write_marker(&layout, &marker)?;
        faults.after(CommitBoundary::RenameDaemon)?;

        fs::rename(pair.haider_path(), &layout.haider)
            .map_err(|error| UpdateError::io("rename staged haider into place", error))?;
        marker.phase = TransactionPhase::PairInstalled;
        write_marker(&layout, &marker)?;
        faults.after(CommitBoundary::RenameCli)?;

        make_owner_writable(&layout.haiderd)?;
        marker.phase = TransactionPhase::DaemonWritable;
        write_marker(&layout, &marker)?;
        faults.after(CommitBoundary::ChmodDaemon)?;

        make_owner_writable(&layout.haider)?;
        marker.phase = TransactionPhase::PairWritable;
        write_marker(&layout, &marker)?;
        faults.after(CommitBoundary::ChmodCli)?;

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

fn make_owner_writable(path: &Path) -> Result<(), UpdateError> {
    haider_platform::set_mode(path, 0o700)
        .map_err(|error| UpdateError::io("make installed binary owner-writable", error))?;
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|error| UpdateError::io("fsync installed binary permissions", error))
}

fn make_backup(source: &Path, backup: &Path) -> Result<(), UpdateError> {
    fs::hard_link(source, backup)
        .map_err(|error| UpdateError::io("create same-filesystem binary backup", error))?;
    File::open(backup)
        .and_then(|file| file.sync_all())
        .map_err(|error| UpdateError::io("fsync binary backup", error))?;
    sync_dir(backup.parent().unwrap_or_else(|| Path::new(".")))
}

fn rollback_marker(layout: &InstallLayout, marker: &mut Marker) -> Result<(), UpdateError> {
    // Validate both restore sources before the first rename. A corrupt second
    // backup must not turn a recoverable new/new pair into daemon-old/CLI-new.
    verify_restore_source(
        &layout.haiderd,
        &layout.dir.join(&marker.backup_haiderd),
        &marker.old_haiderd_digest,
    )?;
    verify_restore_source(
        &layout.haider,
        &layout.dir.join(&marker.backup_haider),
        &marker.old_haider_digest,
    )?;
    marker.phase = TransactionPhase::RollingBack;
    let _ = write_marker(layout, marker);
    restore_one(
        &layout.haiderd,
        &layout.dir.join(&marker.backup_haiderd),
        &marker.old_haiderd_digest,
    )?;
    restore_one(
        &layout.haider,
        &layout.dir.join(&marker.backup_haider),
        &marker.old_haider_digest,
    )?;
    sync_dir(&layout.dir)?;
    if sha256_file(&layout.haider)? != marker.old_haider_digest
        || sha256_file(&layout.haiderd)? != marker.old_haiderd_digest
    {
        return Err(UpdateError::Internal(
            "rollback did not restore the exact old binary pair".into(),
        ));
    }
    remove_if_exists(&layout.dir.join(MARKER_NAME), "remove rollback marker")?;
    remove_if_exists(
        &layout.dir.join(&marker.backup_haider),
        "remove consumed CLI backup",
    )?;
    remove_if_exists(
        &layout.dir.join(&marker.backup_haiderd),
        "remove consumed daemon backup",
    )?;
    sync_dir(&layout.dir)
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
    if marker.phase == TransactionPhase::Finalizing {
        let cli_target =
            sha256_file(&layout.haider).is_ok_and(|hash| hash == marker.target_haider_digest);
        let daemon_target =
            sha256_file(&layout.haiderd).is_ok_and(|hash| hash == marker.target_haiderd_digest);
        if cli_target && daemon_target {
            remove_if_exists(
                &layout.dir.join(&marker.backup_haider),
                "finish successful CLI backup cleanup",
            )?;
            remove_if_exists(
                &layout.dir.join(&marker.backup_haiderd),
                "finish successful daemon backup cleanup",
            )?;
            remove_if_exists(&marker_path, "finish successful marker cleanup")?;
            return sync_dir(&layout.dir);
        }
    }
    let cli_old = sha256_file(&layout.haider).is_ok_and(|hash| hash == marker.old_haider_digest);
    let daemon_old =
        sha256_file(&layout.haiderd).is_ok_and(|hash| hash == marker.old_haiderd_digest);
    if cli_old && daemon_old {
        remove_if_exists(
            &layout.dir.join(&marker.backup_haider),
            "remove recovered CLI backup",
        )?;
        remove_if_exists(
            &layout.dir.join(&marker.backup_haiderd),
            "remove recovered daemon backup",
        )?;
        remove_if_exists(&marker_path, "remove recovered update marker")?;
        return sync_dir(&layout.dir);
    }
    rollback_marker(layout, &mut marker)
}

fn write_marker(layout: &InstallLayout, marker: &Marker) -> Result<(), UpdateError> {
    let path = layout.dir.join(MARKER_NAME);
    let part = layout
        .dir
        .join(format!(".{MARKER_NAME}.{}.part", marker.transaction_id));
    let value = json!({
        "schema": "haider.update.transaction.v1",
        "transaction_id": marker.transaction_id,
        "old_version": marker.old_version,
        "target_version": marker.target_version,
        "old_haider_digest": marker.old_haider_digest,
        "old_haiderd_digest": marker.old_haiderd_digest,
        "target_haider_digest": marker.target_haider_digest,
        "target_haiderd_digest": marker.target_haiderd_digest,
        "source_archive_digest": marker.source_archive_digest,
        "backup_haider": marker.backup_haider,
        "backup_haiderd": marker.backup_haiderd,
        "phase": marker.phase.as_str(),
    });
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
    if value.get("schema").and_then(Value::as_str) != Some("haider.update.transaction.v1") {
        return Err(UpdateError::Internal(
            "unknown update recovery marker schema".into(),
        ));
    }
    let field = |name: &str| -> Result<String, UpdateError> {
        value
            .get(name)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| UpdateError::Internal(format!("update marker lacks `{name}`")))
    };
    let backup_haider = field("backup_haider")?;
    let backup_haiderd = field("backup_haiderd")?;
    if !safe_backup_name(&backup_haider, ".haider-old-")
        || !safe_backup_name(&backup_haiderd, ".haiderd-old-")
    {
        return Err(UpdateError::Internal(
            "update marker contains an unsafe backup name".into(),
        ));
    }
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
    let marker = Marker {
        transaction_id,
        old_version: field("old_version")?,
        target_version: field("target_version")?,
        old_haider_digest: digest_field(&field("old_haider_digest")?)?,
        old_haiderd_digest: digest_field(&field("old_haiderd_digest")?)?,
        target_haider_digest: digest_field(&field("target_haider_digest")?)?,
        target_haiderd_digest: digest_field(&field("target_haiderd_digest")?)?,
        source_archive_digest: digest_field(&field("source_archive_digest")?)?,
        backup_haider,
        backup_haiderd,
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
pub(crate) fn marker_path(layout: &InstallLayout) -> PathBuf {
    layout.dir.join(MARKER_NAME)
}
