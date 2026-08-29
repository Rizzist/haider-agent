//! Daemon-facing adapter for the shared platform endpoint owner.

use crate::{DaemonConfig, DaemonError};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const RUNTIME_REMOVE_RETRY_WINDOW: Duration = Duration::from_millis(250);
const RUNTIME_REMOVE_RETRY_INTERVAL: Duration = Duration::from_millis(25);

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PidFileIdentity {
    device: u64,
    inode: u64,
}

#[cfg(windows)]
type PidFileIdentity = haider_platform::WindowsFileIdentity;

struct OwnedRuntimeDirectory {
    logical_path: PathBuf,
    receipt: haider_platform::OwnedDirectoryReceipt,
    retired: bool,
}

impl OwnedRuntimeDirectory {
    fn from_receipt(receipt: haider_platform::OwnedDirectoryReceipt) -> Self {
        Self {
            logical_path: receipt.path().to_path_buf(),
            receipt,
            retired: false,
        }
    }
}

/// Advisory process file inside one profile's private runtime directory.
/// The store lock remains the only singleton authority.
pub const DAEMON_PID_FILE: &str = "haiderd.pid";

/// Bound IPC endpoint plus the profile-local diagnostic/runtime artifacts that
/// must disappear through the same graceful shutdown path.
pub(crate) struct BoundEndpoint {
    inner: haider_platform::BoundEndpoint,
    runtime: RuntimeDirectory,
    pid_path: PathBuf,
    pid_file: Option<std::fs::File>,
    pid_identity: Option<PidFileIdentity>,
    pid_active: bool,
    endpoint_active: bool,
}

/// Cleanup ownership starts as soon as the profile runtime is created, not
/// only after the socket binds. This makes every typed pre-Ready error remove
/// the directory it created as well.
pub(crate) struct RuntimeDirectory {
    path: PathBuf,
    daemon_temp_path: Option<PathBuf>,
    created_directories: Vec<OwnedRuntimeDirectory>,
    owned_entries: Vec<PathBuf>,
    active: bool,
}

impl RuntimeDirectory {
    pub(crate) fn prepare(path: &Path) -> Result<Self, DaemonError> {
        let daemon_temp_path = configured_daemon_temp_path(path);
        let prepared_result = match daemon_temp_path.as_deref() {
            Some(temp) => haider_platform::prepare_runtime_directory_with_temp(path, temp),
            None => haider_platform::prepare_runtime_directory(path),
        };
        let prepared = match prepared_result {
            Ok(prepared) => prepared,
            Err(haider_platform::EndpointError::OwnedResidual { path, .. }) => {
                let remaining_entries = std::fs::read_dir(&path)
                    .map_err(|error| DaemonError::io("inspect preparation residual", &path, error))?
                    .map(|entry| entry.map(|entry| entry.path()))
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|error| {
                        DaemonError::io("enumerate preparation residual", &path, error)
                    })?;
                return Err(DaemonError::RuntimeDirectoryNotEmpty {
                    path,
                    remaining_entries,
                });
            }
            Err(error) => return Err(map_error(error)),
        };
        let created_directories = prepared
            .into_created_directories()
            .into_iter()
            .map(OwnedRuntimeDirectory::from_receipt)
            .collect();
        Ok(Self {
            path: path.to_path_buf(),
            daemon_temp_path,
            created_directories,
            owned_entries: Vec::new(),
            active: true,
        })
    }

    pub(crate) fn cleanup(&mut self) -> Result<(), DaemonError> {
        if !self.active {
            return Ok(());
        }
        cleanup_runtime_dirs(
            &self.path,
            self.daemon_temp_path.as_deref(),
            &mut self.created_directories,
            &self.owned_entries,
        )?;
        self.active = false;
        Ok(())
    }

    pub(crate) fn remember_owned_path(&mut self, path: PathBuf) {
        if !self.owned_entries.contains(&path) {
            self.owned_entries.push(path);
        }
    }

    fn forget_owned_path(&mut self, path: &Path) {
        self.owned_entries.retain(|owned| owned != path);
    }
}

fn configured_daemon_temp_path(runtime_dir: &Path) -> Option<PathBuf> {
    let candidate = std::env::temp_dir();
    let expected_parent = runtime_dir.join("tmp");
    let is_daemon_private = candidate
        .file_name()
        .is_some_and(|name| name.to_string_lossy().starts_with(".haiderd-tmp-"));
    (candidate.parent() == Some(expected_parent.as_path()) && is_daemon_private)
        .then_some(candidate)
}

impl Drop for RuntimeDirectory {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

impl BoundEndpoint {
    #[must_use]
    pub(crate) fn path(&self) -> &Path {
        self.inner.path()
    }

    #[must_use]
    pub(crate) fn owner_uid(&self) -> u32 {
        self.inner.owner_uid()
    }

    pub(crate) async fn accept(
        &self,
    ) -> std::io::Result<(haider_platform::IpcStream, haider_platform::EndpointAddress)> {
        self.inner.accept().await
    }

    pub(crate) fn close_listener(&mut self) {
        self.inner.close_listener();
    }

    pub(crate) fn cleanup(&mut self) -> Result<(), haider_platform::EndpointError> {
        let endpoint_error = if self.endpoint_active {
            // Platform cleanup is intentionally one-shot: on Unix its owned
            // identity is retired before the fallible unlink attempt. Mirror
            // that contract so Drop cannot report a false retry success.
            self.endpoint_active = false;
            let result = self.inner.cleanup();
            match &result {
                Ok(()) => {
                    #[cfg(unix)]
                    journal_cleanup_success(
                        "endpoint_remove",
                        self.inner.path(),
                        "owned_endpoint_removed_or_absent_or_replacement_preserved",
                    );
                    #[cfg(windows)]
                    journal_cleanup_success(
                        "endpoint_cleanup",
                        self.inner.path(),
                        "no_filesystem_remove_pipe_name_may_outlive_cleanup",
                    );
                }
                Err(error) => {
                    journal_endpoint_failure("endpoint_remove", self.inner.path(), error);
                }
            }
            result.err()
        } else {
            None
        };
        let pid_error = self.remove_owned_pid().err();
        match endpoint_error.or(pid_error) {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    pub(crate) fn cleanup_runtime(&mut self) -> Result<(), DaemonError> {
        for path in self.inner.owned_runtime_paths().map_err(map_error)? {
            self.runtime.remember_owned_path(path);
        }
        self.runtime.cleanup()
    }

    fn remove_owned_pid(&mut self) -> Result<(), haider_platform::EndpointError> {
        if !self.pid_active {
            return Ok(());
        }
        let anchor_identity = self
            .pid_file
            .as_ref()
            .map(pid_file_identity_from_file)
            .transpose()
            .map_err(|error| haider_platform::EndpointError::Io {
                operation: "identify retained daemon pid file",
                path: self.pid_path.clone(),
                source: error,
            })?
            .ok_or_else(|| haider_platform::EndpointError::Endpoint {
                message: "active daemon pid file has no retained ownership handle".into(),
            })?;
        let expected_identity =
            self.pid_identity
                .ok_or_else(|| haider_platform::EndpointError::Endpoint {
                    message: "active daemon pid file has no ownership identity".into(),
                })?;
        if anchor_identity != expected_identity {
            return Err(haider_platform::EndpointError::Endpoint {
                message: "retained daemon pid ownership handle changed identity".into(),
            });
        }
        match pid_file_identity(&self.pid_path) {
            Ok(identity) if identity != expected_identity => {
                if !retained_pid_is_unlinked(self.pid_file.as_ref(), &self.pid_path)? {
                    return Err(owned_pid_coordinate_lost(&self.pid_path));
                }
                self.pid_active = false;
                self.runtime.forget_owned_path(&self.pid_path);
                self.pid_file.take();
                journal_cleanup_success(
                    "pid_file_remove",
                    &self.pid_path,
                    "preserved_replacement_identity",
                );
                return Ok(());
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if !retained_pid_is_unlinked(self.pid_file.as_ref(), &self.pid_path)? {
                    return Err(owned_pid_coordinate_lost(&self.pid_path));
                }
                self.pid_active = false;
                self.runtime.forget_owned_path(&self.pid_path);
                self.pid_file.take();
                journal_cleanup_success("pid_file_remove", &self.pid_path, "already_absent");
                return Ok(());
            }
            Err(error) => {
                journal_io_failure("pid_file_read", &self.pid_path, &error);
                return Err(haider_platform::EndpointError::Io {
                    operation: "read owned daemon pid file",
                    path: self.pid_path.clone(),
                    source: error,
                });
            }
        }
        let claim_path = pid_claim_path(&self.pid_path)?;
        match rename_pid_no_replace(&self.pid_path, &claim_path) {
            Ok(()) => {
                self.runtime.forget_owned_path(&self.pid_path);
                self.runtime.remember_owned_path(claim_path.clone());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if !retained_pid_is_unlinked(self.pid_file.as_ref(), &self.pid_path)? {
                    return Err(owned_pid_coordinate_lost(&self.pid_path));
                }
                self.pid_active = false;
                self.runtime.forget_owned_path(&self.pid_path);
                self.pid_file.take();
                journal_cleanup_success(
                    "pid_file_remove",
                    &self.pid_path,
                    "owned_public_path_already_absent",
                );
                return Ok(());
            }
            Err(error) => {
                journal_io_failure("pid_file_claim", &self.pid_path, &error);
                return Err(haider_platform::EndpointError::Io {
                    operation: "claim owned daemon pid file",
                    path: self.pid_path.clone(),
                    source: error,
                });
            }
        }
        match pid_file_identity(&claim_path) {
            Ok(identity) if identity != expected_identity => {
                let restore = rename_pid_no_replace(&claim_path, &self.pid_path);
                return match restore {
                    Ok(()) => {
                        self.runtime.forget_owned_path(&claim_path);
                        if !retained_pid_is_unlinked(self.pid_file.as_ref(), &self.pid_path)? {
                            return Err(owned_pid_coordinate_lost(&self.pid_path));
                        }
                        self.pid_active = false;
                        self.pid_file.take();
                        journal_cleanup_success(
                            "pid_file_remove",
                            &self.pid_path,
                            "preserved_racing_replacement_identity",
                        );
                        Ok(())
                    }
                    Err(error) => {
                        journal_io_failure("pid_file_restore", &claim_path, &error);
                        Err(haider_platform::EndpointError::Io {
                            operation: "restore replacement daemon pid file",
                            path: claim_path,
                            source: error,
                        })
                    }
                };
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if !retained_pid_is_unlinked(self.pid_file.as_ref(), &claim_path)? {
                    return Err(owned_pid_coordinate_lost(&claim_path));
                }
                self.runtime.forget_owned_path(&claim_path);
                self.pid_active = false;
                self.pid_file.take();
                journal_cleanup_success(
                    "pid_file_remove",
                    &claim_path,
                    "owned_claim_already_absent_no_public_replacement_touched",
                );
                return Ok(());
            }
            Err(error) => {
                journal_io_failure("pid_file_claim_read", &claim_path, &error);
                return Err(haider_platform::EndpointError::Io {
                    operation: "inspect claimed daemon pid file",
                    path: claim_path,
                    source: error,
                });
            }
        }
        match std::fs::remove_file(&claim_path) {
            Ok(()) => {
                self.pid_active = false;
                #[cfg(unix)]
                self.runtime.forget_owned_path(&claim_path);
                self.pid_file.take();
                journal_cleanup_success(
                    "pid_file_remove",
                    &claim_path,
                    "owned_claim_removal_requested",
                );
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if !retained_pid_is_unlinked(self.pid_file.as_ref(), &claim_path)? {
                    return Err(owned_pid_coordinate_lost(&claim_path));
                }
                self.pid_active = false;
                self.runtime.forget_owned_path(&claim_path);
                self.pid_file.take();
                journal_cleanup_success("pid_file_remove", &claim_path, "already_absent");
                Ok(())
            }
            Err(error) => {
                journal_io_failure("pid_file_remove", &claim_path, &error);
                Err(haider_platform::EndpointError::Io {
                    operation: "remove owned daemon pid file",
                    path: claim_path,
                    source: error,
                })
            }
        }
    }
}

fn retained_pid_is_unlinked(
    pid_file: Option<&std::fs::File>,
    last_known_path: &Path,
) -> Result<bool, haider_platform::EndpointError> {
    let file = pid_file.ok_or_else(|| haider_platform::EndpointError::Endpoint {
        message: "active daemon pid file has no retained ownership handle".into(),
    })?;
    haider_platform::retained_file_link_count(file)
        .map(|links| links == 0)
        .map_err(|source| haider_platform::EndpointError::Io {
            operation: "inspect retained daemon pid link count",
            path: last_known_path.to_path_buf(),
            source,
        })
}

fn owned_pid_coordinate_lost(last_known_path: &Path) -> haider_platform::EndpointError {
    eprintln!(
        "haiderd: ephemeral-lifecycle event=cleanup step=pid_file_remove \
         outcome=failed reason=owned_pid_coordinate_lost last_known_path={}",
        last_known_path.display()
    );
    tracing::warn!(
        step = "pid_file_remove",
        last_known_path = %last_known_path.display(),
        reason = "owned_pid_coordinate_lost",
        "daemon-owned pid file remains linked outside its recorded coordinate"
    );
    haider_platform::EndpointError::OwnedResidual {
        path: last_known_path.to_path_buf(),
        source: Box::new(haider_platform::EndpointError::Endpoint {
            message: "daemon-owned pid file remains linked outside its recorded coordinate".into(),
        }),
    }
}

impl Drop for BoundEndpoint {
    fn drop(&mut self) {
        let _ = self.cleanup();
        let _ = self.cleanup_runtime();
    }
}

pub(crate) async fn bind(
    config: &DaemonConfig,
    mut runtime: RuntimeDirectory,
) -> Result<BoundEndpoint, DaemonError> {
    let endpoint = haider_platform::Endpoint::from_address(config.endpoint_path());
    let mut inner = match haider_platform::BoundEndpoint::bind(&endpoint, &config.runtime_dir).await
    {
        Ok(inner) => inner,
        Err(error) => {
            if let Some(path) = error.owned_residual_path() {
                runtime.remember_owned_path(path.to_path_buf());
                runtime.cleanup()?;
            }
            return Err(map_error(error));
        }
    };
    let (pid_path, pid_file, pid_identity, pid_active) = match publish_pid(&config.runtime_dir) {
        Ok(Some((path, file, identity))) => (path, Some(file), Some(identity), true),
        Ok(None) => (config.runtime_dir.join(DAEMON_PID_FILE), None, None, false),
        Err(failure) => {
            if let Some(path) = failure.owned_path {
                runtime.remember_owned_path(path);
            }
            runtime.remember_owned_path(inner.path().to_path_buf());
            let endpoint_error = inner.cleanup().err();
            match inner.owned_runtime_paths() {
                Ok(paths) if paths.is_empty() => runtime.forget_owned_path(inner.path()),
                Ok(paths) => {
                    for path in paths {
                        runtime.remember_owned_path(path);
                    }
                }
                Err(error) => {
                    runtime.cleanup()?;
                    return Err(map_error(error));
                }
            }
            runtime.cleanup()?;
            if let Some(error) = endpoint_error {
                return Err(map_error(error));
            }
            return Err(failure.error);
        }
    };
    if pid_active {
        runtime.remember_owned_path(pid_path.clone());
    }
    // Hygiene, never correctness: a daemon killed without running its cleanup
    // (SIGKILL, panic, power loss) can leave its endpoint node behind. The
    // per-profile directory bounds this sweep to one profile; every removal is
    // still proven dead exactly as the bind path requires. Failures are ignored.
    let removed =
        haider_platform::sweep_stale_endpoints(&config.runtime_dir, Some(inner.path())).await;
    if removed > 0 {
        tracing::info!(
            removed,
            "swept stale endpoint nodes from the profile runtime"
        );
    }
    Ok(BoundEndpoint {
        inner,
        runtime,
        pid_path,
        pid_file,
        pid_identity,
        pid_active,
        endpoint_active: true,
    })
}

fn cleanup_runtime_dirs(
    runtime_dir: &Path,
    daemon_temp_path: Option<&Path>,
    created_directories: &mut [OwnedRuntimeDirectory],
    owned_entries: &[PathBuf],
) -> Result<(), DaemonError> {
    let temp = runtime_dir.join("tmp");
    let mut cleanup_targets = Vec::with_capacity(3);
    if let Some(daemon_temp_path) = daemon_temp_path {
        cleanup_targets.push((
            "remove daemon-private temporary directory",
            daemon_temp_path,
            true,
        ));
    }
    cleanup_targets.extend([
        ("remove daemon temporary directory", temp.as_path(), false),
        ("remove profile runtime directory", runtime_dir, false),
    ]);
    for (operation, path, contents_are_daemon_owned) in cleanup_targets {
        if let Some(created) = created_directories
            .iter_mut()
            .find(|created| created.logical_path == path && !created.retired)
        {
            remove_runtime_directory(operation, created, contents_are_daemon_owned, owned_entries)?;
            created.retired = true;
        } else {
            inspect_retained_unowned_directory(operation, path, owned_entries)?;
        }
    }
    Ok(())
}

fn remove_runtime_directory(
    operation: &'static str,
    created_directory: &mut OwnedRuntimeDirectory,
    contents_are_daemon_owned: bool,
    owned_entries: &[PathBuf],
) -> Result<(), DaemonError> {
    let retry_deadline = Instant::now() + RUNTIME_REMOVE_RETRY_WINDOW;
    let mut removal_requested = false;
    loop {
        if !removal_requested {
            match haider_platform::owned_directory_path_state(&created_directory.receipt) {
                Ok(state) => {
                    if handle_non_owned_directory_state(operation, created_directory, state)? {
                        return Ok(());
                    }
                }
                Err(error) if runtime_removal_may_be_pending(&error) => {
                    if retry_runtime_cleanup(retry_deadline) {
                        continue;
                    }
                    let current = created_directory.receipt.path().to_path_buf();
                    journal_runtime_entries_unavailable(operation, &current, &error);
                    return Err(DaemonError::io(
                        "verify owned runtime directory coordinate",
                        current,
                        error,
                    ));
                }
                Err(error) => {
                    let current = created_directory.receipt.path().to_path_buf();
                    journal_io_failure(
                        "verify owned runtime directory coordinate",
                        &current,
                        &error,
                    );
                    return Err(DaemonError::io(
                        "verify owned runtime directory coordinate",
                        current,
                        error,
                    ));
                }
            }
        }
        let path = created_directory.receipt.path().to_path_buf();
        let remaining_entries = if removal_requested {
            match inspect_remaining_entries(&path)? {
                DirectoryInspection::Absent => {
                    journal_cleanup_success(operation, &path, "owned_removal_request_completed");
                    return Ok(());
                }
                DirectoryInspection::Entries(entries) => entries,
                DirectoryInspection::Pending(error) => {
                    if Instant::now() >= retry_deadline {
                        journal_runtime_entries_unavailable(operation, &path, &error);
                        return Err(DaemonError::io(
                            "inspect owned runtime directory after bounded retry",
                            path,
                            error,
                        ));
                    }
                    std::thread::sleep(RUNTIME_REMOVE_RETRY_INTERVAL);
                    continue;
                }
            }
        } else {
            match haider_platform::inspect_owned_directory(&created_directory.receipt) {
                Ok(haider_platform::OwnedDirectoryInspection::Entries(entries)) => entries,
                Ok(haider_platform::OwnedDirectoryInspection::OwnedObjectUnlinked) => Vec::new(),
                Err(error) if runtime_removal_may_be_pending(&error) => {
                    if retry_runtime_cleanup(retry_deadline) {
                        continue;
                    }
                    journal_runtime_entries_unavailable(operation, &path, &error);
                    return Err(DaemonError::io(
                        "inspect exact owned runtime directory after bounded retry",
                        path,
                        error,
                    ));
                }
                Err(error) => {
                    journal_io_failure("inspect exact owned runtime directory", &path, &error);
                    return Err(DaemonError::io(
                        "inspect exact owned runtime directory",
                        path,
                        error,
                    ));
                }
            }
        };
        if !removal_requested {
            match haider_platform::owned_directory_path_state(&created_directory.receipt) {
                Ok(state) => {
                    if handle_non_owned_directory_state(operation, created_directory, state)? {
                        return Ok(());
                    }
                }
                Err(error) if runtime_removal_may_be_pending(&error) => {
                    if retry_runtime_cleanup(retry_deadline) {
                        continue;
                    }
                    journal_runtime_entries_unavailable(operation, &path, &error);
                    return Err(DaemonError::io(
                        "reverify owned runtime directory coordinate",
                        path,
                        error,
                    ));
                }
                Err(error) => {
                    journal_io_failure(
                        "reverify owned runtime directory coordinate",
                        &path,
                        &error,
                    );
                    return Err(DaemonError::io(
                        "reverify owned runtime directory coordinate",
                        path,
                        error,
                    ));
                }
            }
        }
        if !remaining_entries.is_empty() {
            if !has_daemon_owned_entry(contents_are_daemon_owned, &remaining_entries, owned_entries)
            {
                journal_retained_unowned_entries(operation, &path, &remaining_entries);
                return Ok(());
            }
            if retry_runtime_cleanup(retry_deadline) {
                continue;
            }
            return runtime_directory_not_empty(operation, &path, remaining_entries);
        }

        match haider_platform::remove_owned_empty_directory(&mut created_directory.receipt) {
            Ok(haider_platform::OwnedDirectoryRemoval::Removed) => {
                journal_cleanup_success(operation, &path, "removed_exact_owned_directory");
                return Ok(());
            }
            Ok(haider_platform::OwnedDirectoryRemoval::RemovalRequested) => {
                removal_requested = true;
                let _ = retry_runtime_cleanup(retry_deadline);
                continue;
            }
            Ok(haider_platform::OwnedDirectoryRemoval::AlreadyAbsent) => {
                journal_cleanup_success(
                    operation,
                    created_directory.receipt.path(),
                    "exact_owned_directory_already_absent",
                );
                return Ok(());
            }
            Ok(haider_platform::OwnedDirectoryRemoval::ReplacementPreserved) => {
                inspect_retained_replacement_directory(operation, &created_directory.logical_path)?;
                return Ok(());
            }
            Ok(haider_platform::OwnedDirectoryRemoval::CoordinateLost) => {
                return owned_directory_coordinate_lost(operation, created_directory);
            }
            Ok(haider_platform::OwnedDirectoryRemoval::NotEmpty) => {
                if retry_runtime_cleanup(retry_deadline) {
                    continue;
                }
                let current = created_directory.receipt.path().to_path_buf();
                let entries =
                    match haider_platform::inspect_owned_directory(&created_directory.receipt) {
                        Ok(haider_platform::OwnedDirectoryInspection::Entries(entries)) => entries,
                        Ok(haider_platform::OwnedDirectoryInspection::OwnedObjectUnlinked) => {
                            journal_cleanup_success(
                                operation,
                                &current,
                                "exact_owned_directory_unlinked_after_not_empty_race",
                            );
                            return Ok(());
                        }
                        Err(error) => {
                            journal_runtime_entries_unavailable(operation, &current, &error);
                            return Err(DaemonError::io(
                                "inspect exact raced owned runtime residual",
                                current,
                                error,
                            ));
                        }
                    };
                match haider_platform::owned_directory_path_state(&created_directory.receipt) {
                    Ok(state) => {
                        if handle_non_owned_directory_state(operation, created_directory, state)? {
                            return Ok(());
                        }
                    }
                    Err(error) => {
                        journal_runtime_entries_unavailable(operation, &current, &error);
                        return Err(DaemonError::io(
                            "reverify exact raced owned runtime residual",
                            current,
                            error,
                        ));
                    }
                }
                if !has_daemon_owned_entry(contents_are_daemon_owned, &entries, owned_entries) {
                    journal_retained_unowned_entries(operation, &current, &entries);
                    return Ok(());
                }
                return runtime_directory_not_empty(operation, &current, entries);
            }
            Err(error) if runtime_removal_may_be_pending(&error) => {
                if retry_runtime_cleanup(retry_deadline) {
                    continue;
                }
                let current = created_directory.receipt.path().to_path_buf();
                journal_runtime_entries_unavailable(operation, &current, &error);
                return Err(DaemonError::io(
                    "retire pending owned runtime directory",
                    current,
                    error,
                ));
            }
            Err(error) => {
                journal_io_failure(operation, &path, &error);
                return Err(DaemonError::io(operation, path, error));
            }
        }
    }
}

fn handle_non_owned_directory_state(
    operation: &'static str,
    created_directory: &OwnedRuntimeDirectory,
    state: haider_platform::OwnedDirectoryPathState,
) -> Result<bool, DaemonError> {
    match state {
        haider_platform::OwnedDirectoryPathState::Owned => Ok(false),
        haider_platform::OwnedDirectoryPathState::OwnedObjectUnlinked => {
            journal_cleanup_success(
                operation,
                created_directory.receipt.path(),
                "exact_owned_directory_unlinked",
            );
            Ok(true)
        }
        haider_platform::OwnedDirectoryPathState::ReplacementPreserved => {
            inspect_retained_replacement_directory(operation, &created_directory.logical_path)?;
            Ok(true)
        }
        haider_platform::OwnedDirectoryPathState::CoordinateLost => {
            owned_directory_coordinate_lost(operation, created_directory)?;
            Ok(true)
        }
    }
}

fn owned_directory_coordinate_lost(
    operation: &'static str,
    created_directory: &OwnedRuntimeDirectory,
) -> Result<(), DaemonError> {
    let path = created_directory.receipt.path();
    let error = std::io::Error::other("owned directory remains linked outside its recorded path");
    eprintln!(
        "haiderd: ephemeral-lifecycle event=cleanup step={operation} \
         outcome=failed reason=owned_directory_coordinate_lost path={} error={error}",
        path.display()
    );
    tracing::warn!(
        step = operation,
        path = %path.display(),
        reason = "owned_directory_coordinate_lost",
        "daemon-owned directory remains linked outside its recorded coordinate"
    );
    Err(DaemonError::io(
        "locate daemon-owned runtime directory",
        path,
        error,
    ))
}

fn retry_runtime_cleanup(deadline: Instant) -> bool {
    let now = Instant::now();
    if now >= deadline {
        return false;
    }
    std::thread::sleep(RUNTIME_REMOVE_RETRY_INTERVAL.min(deadline.saturating_duration_since(now)));
    true
}

fn has_daemon_owned_entry(
    contents_are_daemon_owned: bool,
    remaining_entries: &[PathBuf],
    owned_entries: &[PathBuf],
) -> bool {
    contents_are_daemon_owned && !remaining_entries.is_empty()
        || owned_entries.iter().any(|owned| {
            remaining_entries
                .iter()
                .any(|entry| entry == owned || owned.starts_with(entry))
        })
}

enum DirectoryInspection {
    Absent,
    Entries(Vec<PathBuf>),
    Pending(std::io::Error),
}

fn inspect_remaining_entries(path: &Path) -> Result<DirectoryInspection, DaemonError> {
    let directory = match std::fs::read_dir(path) {
        Ok(directory) => directory,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(DirectoryInspection::Absent);
        }
        Err(error) if runtime_removal_may_be_pending(&error) => {
            return Ok(DirectoryInspection::Pending(error));
        }
        Err(error) => {
            journal_io_failure("list nonempty runtime directory", path, &error);
            return Err(DaemonError::io(
                "list nonempty runtime directory",
                path,
                error,
            ));
        }
    };
    let mut remaining_entries = Vec::new();
    for entry in directory {
        match entry {
            Ok(entry) => remaining_entries.push(entry.path()),
            Err(error) if runtime_removal_may_be_pending(&error) => {
                return Ok(DirectoryInspection::Pending(error));
            }
            Err(error) => {
                journal_io_failure("inspect nonempty runtime directory", path, &error);
                return Err(DaemonError::io(
                    "inspect nonempty runtime directory",
                    path,
                    error,
                ));
            }
        }
    }
    remaining_entries.sort();
    Ok(DirectoryInspection::Entries(remaining_entries))
}

fn runtime_directory_not_empty(
    operation: &'static str,
    path: &Path,
    remaining_entries: Vec<PathBuf>,
) -> Result<(), DaemonError> {
    eprintln!(
        "haiderd: ephemeral-lifecycle event=cleanup step={operation} outcome=not_removed_after_retry path={} remaining_entries={remaining_entries:?}",
        path.display()
    );
    tracing::warn!(
        step = operation,
        path = %path.display(),
        remaining_entries = ?remaining_entries,
        "daemon runtime cleanup left a directory behind"
    );
    Err(DaemonError::RuntimeDirectoryNotEmpty {
        path: path.to_path_buf(),
        remaining_entries,
    })
}

fn journal_runtime_entries_unavailable(
    operation: &'static str,
    path: &Path,
    error: &std::io::Error,
) {
    eprintln!(
        "haiderd: ephemeral-lifecycle event=cleanup step={operation} \
         outcome=not_removed_after_retry reason=remaining_entries_unavailable \
         raw_os_error={:?} path={} error={error}",
        error.raw_os_error(),
        path.display()
    );
    tracing::warn!(
        step = operation,
        path = %path.display(),
        raw_os_error = ?error.raw_os_error(),
        %error,
        "owned runtime directory remained but its entries could not be enumerated"
    );
}

fn inspect_retained_unowned_directory(
    operation: &'static str,
    path: &Path,
    owned_entries: &[PathBuf],
) -> Result<(), DaemonError> {
    let retry_deadline = Instant::now() + RUNTIME_REMOVE_RETRY_WINDOW;
    loop {
        let retained_entries = match inspect_remaining_entries(path)? {
            DirectoryInspection::Absent => {
                journal_cleanup_success(operation, path, "already_absent_not_owned");
                return Ok(());
            }
            DirectoryInspection::Entries(entries) => entries,
            DirectoryInspection::Pending(error) => {
                if retry_runtime_cleanup(retry_deadline) {
                    continue;
                }
                journal_runtime_entries_unavailable(operation, path, &error);
                return Err(DaemonError::io(
                    "inspect retained non-daemon runtime directory after bounded retry",
                    path,
                    error,
                ));
            }
        };
        let owned_remains = owned_entries.iter().any(|owned| {
            retained_entries
                .iter()
                .any(|entry| entry == owned || owned.starts_with(entry))
        });
        if !owned_remains {
            eprintln!(
                "haiderd: ephemeral-lifecycle event=cleanup step={operation} \
                 outcome=retained_unowned_directory \
                 reason=directory_not_created_by_this_daemon path={} \
                 retained_entries={retained_entries:?}",
                path.display()
            );
            tracing::info!(
                step = operation,
                path = %path.display(),
                retained_entries = ?retained_entries,
                reason = "directory_not_created_by_this_daemon",
                "retained non-daemon runtime directory"
            );
            return Ok(());
        }
        let now = Instant::now();
        if now < retry_deadline {
            std::thread::sleep(
                RUNTIME_REMOVE_RETRY_INTERVAL.min(retry_deadline.saturating_duration_since(now)),
            );
            continue;
        }
        return runtime_directory_not_empty(operation, path, retained_entries);
    }
}

fn inspect_retained_replacement_directory(
    operation: &'static str,
    path: &Path,
) -> Result<(), DaemonError> {
    let retained_entries = match inspect_remaining_entries(path)? {
        DirectoryInspection::Absent => {
            journal_cleanup_success(operation, path, "became_absent_after_identity_change");
            return Ok(());
        }
        DirectoryInspection::Entries(entries) => entries,
        DirectoryInspection::Pending(error) => {
            journal_io_failure("inspect replacement runtime directory", path, &error);
            return Err(DaemonError::io(
                "inspect replacement runtime directory",
                path,
                error,
            ));
        }
    };
    eprintln!(
        "haiderd: ephemeral-lifecycle event=cleanup step={operation} \
         outcome=retained_replacement_directory reason=directory_identity_changed path={} \
         retained_entries={retained_entries:?}",
        path.display()
    );
    tracing::info!(
        step = operation,
        path = %path.display(),
        retained_entries = ?retained_entries,
        reason = "directory_identity_changed",
        "retained replacement runtime directory"
    );
    Ok(())
}

#[cfg(windows)]
fn runtime_removal_may_be_pending(error: &std::io::Error) -> bool {
    // ERROR_ACCESS_DENIED, ERROR_SHARING_VIOLATION, ERROR_DELETE_PENDING.
    matches!(error.raw_os_error(), Some(5 | 32 | 303))
}

#[cfg(not(windows))]
fn runtime_removal_may_be_pending(_error: &std::io::Error) -> bool {
    false
}

fn journal_retained_unowned_entries(
    operation: &'static str,
    path: &Path,
    remaining_entries: &[PathBuf],
) {
    eprintln!(
        "{}",
        retained_unowned_journal_line(operation, path, remaining_entries)
    );
    tracing::info!(
        step = operation,
        path = %path.display(),
        retained_entries = ?remaining_entries,
        reason = "no_remaining_entry_is_daemon_owned",
        "retained runtime directory containing non-daemon-managed entries"
    );
}

fn retained_unowned_journal_line(
    operation: &'static str,
    path: &Path,
    remaining_entries: &[PathBuf],
) -> String {
    format!(
        "haiderd: ephemeral-lifecycle event=cleanup step={operation} \
         outcome=retained_unowned_entries reason=no_remaining_entry_is_daemon_owned path={} \
         retained_entries={remaining_entries:?}",
        path.display()
    )
}

fn journal_cleanup_success(step: &str, path: &Path, outcome: &str) {
    eprintln!(
        "haiderd: ephemeral-lifecycle event=cleanup step={step} outcome={outcome} path={}",
        path.display()
    );
    tracing::info!(
        step,
        outcome,
        path = %path.display(),
        "daemon runtime cleanup step completed"
    );
}

fn journal_io_failure(step: &str, path: &Path, error: &std::io::Error) {
    eprintln!(
        "haiderd: ephemeral-lifecycle event=cleanup step={step} outcome=failed raw_os_error={:?} path={} error={error}",
        error.raw_os_error(),
        path.display()
    );
    tracing::warn!(
        step,
        path = %path.display(),
        raw_os_error = ?error.raw_os_error(),
        %error,
        "daemon runtime cleanup step failed"
    );
}

fn journal_endpoint_failure(step: &str, path: &Path, error: &haider_platform::EndpointError) {
    let raw_os_error = match error {
        haider_platform::EndpointError::Io { source, .. } => source.raw_os_error(),
        _ => None,
    };
    eprintln!(
        "haiderd: ephemeral-lifecycle event=cleanup step={step} outcome=failed raw_os_error={raw_os_error:?} path={} error={error}",
        path.display()
    );
    tracing::warn!(
        step,
        path = %path.display(),
        ?raw_os_error,
        %error,
        "daemon endpoint cleanup step failed"
    );
}

struct PublishPidFailure {
    error: DaemonError,
    owned_path: Option<PathBuf>,
}

fn publish_pid(
    runtime_dir: &Path,
) -> Result<Option<(PathBuf, std::fs::File, PidFileIdentity)>, PublishPidFailure> {
    let path = runtime_dir.join(DAEMON_PID_FILE);
    let contents = format!("{}\n", std::process::id());
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    haider_platform::configure_file_mode(&mut options, 0o600);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
    }
    let mut file = match options.open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            eprintln!(
                "haiderd: ephemeral-lifecycle event=pid_file_publish \
                 outcome=retained_preexisting_unowned \
                 reason=path_not_created_by_this_daemon path={}",
                path.display()
            );
            tracing::info!(
                path = %path.display(),
                reason = "path_not_created_by_this_daemon",
                "retained pre-existing advisory pid path"
            );
            return Ok(None);
        }
        Err(error) => {
            return Err(PublishPidFailure {
                error: DaemonError::io("open daemon pid file", &path, error),
                owned_path: None,
            });
        }
    };
    file.write_all(contents.as_bytes())
        .and_then(|()| file.sync_data())
        .map_err(|error| PublishPidFailure {
            error: DaemonError::io("write daemon pid file", &path, error),
            owned_path: Some(path.clone()),
        })?;
    let identity = pid_file_identity_from_file(&file).map_err(|error| PublishPidFailure {
        error: DaemonError::io("identify daemon pid file", &path, error),
        owned_path: Some(path.clone()),
    })?;
    Ok(Some((path, file, identity)))
}

#[cfg(unix)]
fn pid_file_identity(path: &Path) -> std::io::Result<PidFileIdentity> {
    use std::os::unix::fs::MetadataExt as _;

    let metadata = std::fs::symlink_metadata(path)?;
    Ok(PidFileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(windows)]
fn pid_file_identity(path: &Path) -> std::io::Result<PidFileIdentity> {
    use std::os::windows::fs::OpenOptionsExt as _;

    const FILE_READ_ATTRIBUTES: u32 = 0x0000_0080;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const FILE_SHARE_DELETE: u32 = 0x0000_0004;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

    let file = std::fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    haider_platform::windows_file_identity(&file)
}

#[cfg(unix)]
fn pid_file_identity_from_file(file: &std::fs::File) -> std::io::Result<PidFileIdentity> {
    use std::os::unix::fs::MetadataExt as _;

    let metadata = file.metadata()?;
    Ok(PidFileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(windows)]
fn pid_file_identity_from_file(file: &std::fs::File) -> std::io::Result<PidFileIdentity> {
    haider_platform::windows_file_identity(file)
}

fn pid_claim_path(pid_path: &Path) -> Result<PathBuf, haider_platform::EndpointError> {
    use std::fmt::Write as _;

    let mut random = [0_u8; 12];
    getrandom::fill(&mut random).map_err(|error| haider_platform::EndpointError::Task {
        message: format!("cannot generate daemon pid claim name: {error}"),
    })?;
    let mut basename = String::from(".haiderd-pid-");
    for byte in random {
        write!(&mut basename, "{byte:02x}").map_err(|error| {
            haider_platform::EndpointError::Task {
                message: format!("cannot format daemon pid claim name: {error}"),
            }
        })?;
    }
    Ok(pid_path.with_file_name(basename))
}

#[cfg(unix)]
fn rename_pid_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    rustix::fs::renameat_with(
        rustix::fs::CWD,
        from,
        rustix::fs::CWD,
        to,
        rustix::fs::RenameFlags::NOREPLACE,
    )
    .map_err(std::io::Error::from)
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn rename_pid_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};

    let from = from
        .as_os_str()
        .encode_wide()
        .chain([0])
        .collect::<Vec<_>>();
    let to = to.as_os_str().encode_wide().chain([0]).collect::<Vec<_>>();
    // SAFETY: both buffers are NUL-terminated for the duration of the call;
    // omitting MOVEFILE_REPLACE_EXISTING gives the claim/restore operation its
    // required atomic no-replace semantics.
    let moved = unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), MOVEFILE_WRITE_THROUGH) };
    if moved == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub(crate) fn map_error(error: haider_platform::EndpointError) -> DaemonError {
    match error {
        haider_platform::EndpointError::Io {
            operation,
            path,
            source,
        } => DaemonError::Io {
            operation,
            path,
            source,
        },
        haider_platform::EndpointError::AddressTooLong {
            path,
            length,
            limit,
            unit,
        } => DaemonError::EndpointAddressTooLong {
            path,
            length,
            limit,
            unit,
        },
        haider_platform::EndpointError::Endpoint { message } => DaemonError::Endpoint { message },
        haider_platform::EndpointError::Task { message } => DaemonError::Task { message },
        haider_platform::EndpointError::OwnedResidual { source, .. } => map_error(*source),
        haider_platform::EndpointError::PathTooLong {
            path,
            length,
            limit,
        } => DaemonError::Endpoint {
            message: format!(
                "runtime path {} is {length} bytes; limit is {limit} bytes",
                path.display()
            ),
        },
    }
}

pub(crate) fn validate_budget(config: &DaemonConfig) -> Result<(), DaemonError> {
    haider_platform::Endpoint::from_address(config.endpoint_path())
        .validate_for_bind(&config.runtime_dir)
        .map_err(map_error)
}

impl From<haider_platform::EndpointError> for DaemonError {
    fn from(error: haider_platform::EndpointError) -> Self {
        map_error(error)
    }
}
