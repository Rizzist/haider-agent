//! Daemon-facing adapter for the shared platform endpoint owner.

use crate::{DaemonConfig, DaemonError};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const RUNTIME_REMOVE_RETRY_WINDOW: Duration = Duration::from_millis(250);
const RUNTIME_REMOVE_RETRY_INTERVAL: Duration = Duration::from_millis(25);

/// Advisory process file inside one profile's private runtime directory.
/// The store lock remains the only singleton authority.
pub const DAEMON_PID_FILE: &str = "haiderd.pid";

/// Bound IPC endpoint plus the profile-local diagnostic/runtime artifacts that
/// must disappear through the same graceful shutdown path.
pub(crate) struct BoundEndpoint {
    inner: haider_platform::BoundEndpoint,
    runtime: RuntimeDirectory,
    pid_path: PathBuf,
    pid_contents: String,
    pid_active: bool,
    endpoint_active: bool,
}

/// Cleanup ownership starts as soon as the profile runtime is created, not
/// only after the socket binds. This makes every typed pre-Ready error remove
/// the directory it created as well.
pub(crate) struct RuntimeDirectory {
    path: PathBuf,
    active: bool,
}

impl RuntimeDirectory {
    pub(crate) fn prepare(path: &Path) -> Result<Self, DaemonError> {
        haider_platform::prepare_runtime_directory(path).map_err(map_error)?;
        Ok(Self {
            path: path.to_path_buf(),
            active: true,
        })
    }

    pub(crate) fn cleanup(&mut self) -> Result<(), DaemonError> {
        if !self.active {
            return Ok(());
        }
        cleanup_runtime_dirs(&self.path)?;
        self.active = false;
        Ok(())
    }
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
                        "removed_or_already_absent",
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
        self.runtime.cleanup()
    }

    fn remove_owned_pid(&mut self) -> Result<(), haider_platform::EndpointError> {
        if !self.pid_active {
            return Ok(());
        }
        match std::fs::read_to_string(&self.pid_path) {
            Ok(contents) if contents != self.pid_contents => {
                self.pid_active = false;
                journal_cleanup_success("pid_file_remove", &self.pid_path, "preserved_successor");
                return Ok(());
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.pid_active = false;
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
        match std::fs::remove_file(&self.pid_path) {
            Ok(()) => {
                self.pid_active = false;
                journal_cleanup_success("pid_file_remove", &self.pid_path, "removed");
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.pid_active = false;
                journal_cleanup_success("pid_file_remove", &self.pid_path, "already_absent");
                Ok(())
            }
            Err(error) => {
                journal_io_failure("pid_file_remove", &self.pid_path, &error);
                Err(haider_platform::EndpointError::Io {
                    operation: "remove owned daemon pid file",
                    path: self.pid_path.clone(),
                    source: error,
                })
            }
        }
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
    runtime: RuntimeDirectory,
) -> Result<BoundEndpoint, DaemonError> {
    let endpoint = haider_platform::Endpoint::from_address(config.endpoint_path());
    let mut inner = haider_platform::BoundEndpoint::bind(&endpoint, &config.runtime_dir)
        .await
        .map_err(map_error)?;
    let (pid_path, pid_contents) = match publish_pid(&config.runtime_dir) {
        Ok(published) => published,
        Err(error) => {
            let _ = inner.cleanup();
            return Err(error);
        }
    };
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
        pid_contents,
        pid_active: true,
        endpoint_active: true,
    })
}

pub(crate) fn cleanup_runtime_dirs(runtime_dir: &Path) -> Result<(), DaemonError> {
    let temp = runtime_dir.join("tmp");
    for (operation, path) in [
        ("remove daemon temporary directory", temp.as_path()),
        ("remove profile runtime directory", runtime_dir),
    ] {
        remove_runtime_directory(operation, path)?;
    }
    Ok(())
}

fn remove_runtime_directory(operation: &'static str, path: &Path) -> Result<(), DaemonError> {
    let retry_deadline = Instant::now() + RUNTIME_REMOVE_RETRY_WINDOW;
    loop {
        match std::fs::remove_dir(path) {
            Ok(()) => return journal_removed_directory(operation, path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                journal_cleanup_success(operation, path, "already_absent");
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => {
                let now = Instant::now();
                if now < retry_deadline {
                    std::thread::sleep(
                        RUNTIME_REMOVE_RETRY_INTERVAL
                            .min(retry_deadline.saturating_duration_since(now)),
                    );
                    continue;
                }
                return runtime_directory_not_empty(operation, path);
            }
            Err(error) => {
                journal_io_failure(operation, path, &error);
                return Err(DaemonError::io(operation, path, error));
            }
        }
    }
}

fn journal_removed_directory(operation: &'static str, path: &Path) -> Result<(), DaemonError> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            journal_cleanup_success(operation, path, "removed");
            Ok(())
        }
        Ok(_) => runtime_directory_not_empty(operation, path),
        Err(error) => {
            journal_io_failure("verify removed runtime directory", path, &error);
            Err(DaemonError::io(
                "verify removed runtime directory",
                path,
                error,
            ))
        }
    }
}

fn runtime_directory_not_empty(operation: &'static str, path: &Path) -> Result<(), DaemonError> {
    let mut remaining_entries = std::fs::read_dir(path)
        .map_err(|error| {
            journal_io_failure("list nonempty runtime directory", path, &error);
            DaemonError::io("list nonempty runtime directory", path, error)
        })?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| {
            journal_io_failure("inspect nonempty runtime directory", path, &error);
            DaemonError::io("inspect nonempty runtime directory", path, error)
        })?;
    remaining_entries.sort();
    eprintln!(
        "haiderd: ephemeral-lifecycle event=cleanup step={operation} outcome=not_removed_remaining_entries path={} remaining_entries={remaining_entries:?}",
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

fn publish_pid(runtime_dir: &Path) -> Result<(PathBuf, String), DaemonError> {
    let path = runtime_dir.join(DAEMON_PID_FILE);
    let contents = format!("{}\n", std::process::id());
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    haider_platform::configure_file_mode(&mut options, 0o600);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
    }
    let mut file = options
        .open(&path)
        .map_err(|error| DaemonError::io("open daemon pid file", &path, error))?;
    file.write_all(contents.as_bytes())
        .and_then(|()| file.sync_data())
        .map_err(|error| DaemonError::io("write daemon pid file", &path, error))?;
    Ok((path, contents))
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
