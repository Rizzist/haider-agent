//! Daemon-facing adapter for the shared platform endpoint owner.

use crate::{DaemonConfig, DaemonError};
use std::io::Write as _;
use std::path::{Path, PathBuf};

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
        let endpoint_error = self.inner.cleanup().err();
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
                return Ok(());
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.pid_active = false;
                return Ok(());
            }
            Err(error) => {
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
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.pid_active = false;
                Ok(())
            }
            Err(error) => Err(haider_platform::EndpointError::Io {
                operation: "remove owned daemon pid file",
                path: self.pid_path.clone(),
                source: error,
            }),
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
    let endpoint = haider_platform::Endpoint::new(&config.runtime_dir, &config.profile_id);
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
    })
}

pub(crate) fn cleanup_runtime_dirs(runtime_dir: &Path) -> Result<(), DaemonError> {
    let temp = runtime_dir.join("tmp");
    for (operation, path, preserve_nonempty) in [
        ("remove daemon temporary directory", temp.as_path(), false),
        ("remove profile runtime directory", runtime_dir, true),
    ] {
        match std::fs::remove_dir(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error)
                if preserve_nonempty && error.kind() == std::io::ErrorKind::DirectoryNotEmpty =>
            {
                // Exact socket/PID ownership cleanup may intentionally leave
                // a successor or unrelated node behind. Preserve it; an
                // empty ephemeral tree is still removed by the success path.
            }
            Err(error) => return Err(DaemonError::io(operation, path, error)),
        }
    }
    Ok(())
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
        haider_platform::EndpointError::Endpoint { message } => DaemonError::Endpoint { message },
        haider_platform::EndpointError::Task { message } => DaemonError::Task { message },
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

impl From<haider_platform::EndpointError> for DaemonError {
    fn from(error: haider_platform::EndpointError) -> Self {
        map_error(error)
    }
}
