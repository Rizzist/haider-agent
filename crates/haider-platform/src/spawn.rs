use std::path::Path;
use std::process::{Child, Stdio};

/// Fully resolved inputs for launching the packaged sibling daemon.
#[derive(Debug, Clone, Copy)]
pub struct DaemonSpawn<'a> {
    pub binary: &'a Path,
    pub profile_id: &'a str,
    pub store_dir: &'a Path,
    pub runtime_dir: &'a Path,
    pub log_path: &'a Path,
}

#[derive(Debug)]
pub enum DaemonSpawnError {
    OpenLog(std::io::Error),
    CloneLog(std::io::Error),
    Spawn(std::io::Error),
}

impl std::fmt::Display for DaemonSpawnError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OpenLog(error) => write!(formatter, "cannot open daemon log: {error}"),
            Self::CloneLog(error) => write!(formatter, "cannot clone daemon log handle: {error}"),
            Self::Spawn(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for DaemonSpawnError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::OpenLog(error) | Self::CloneLog(error) | Self::Spawn(error) => Some(error),
        }
    }
}

/// Spawns the sibling daemon with its fixed argv/stdout/stderr contract and
/// the platform's detach + inheritance hygiene.
pub fn spawn_daemon(spec: DaemonSpawn<'_>) -> Result<Child, DaemonSpawnError> {
    let mut log_options = std::fs::OpenOptions::new();
    log_options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        log_options.mode(0o600);
    }
    let log = log_options
        .open(spec.log_path)
        .map_err(DaemonSpawnError::OpenLog)?;
    let log_err = log.try_clone().map_err(DaemonSpawnError::CloneLog)?;
    let mut command = std::process::Command::new(spec.binary);
    command
        .arg("--profile")
        .arg(spec.profile_id)
        .arg("--store-dir")
        .arg(spec.store_dir)
        .arg("--runtime-dir")
        .arg(spec.runtime_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));

    configure_daemon(&mut command);
    command.spawn().map_err(DaemonSpawnError::Spawn)
}

#[cfg(unix)]
fn configure_daemon(command: &mut std::process::Command) {
    use std::os::unix::process::CommandExt as _;
    command.process_group(0);
    // SAFETY: the hook runs between fork and exec and calls only the
    // async-signal-safe close(2); no allocation or runtime state is touched.
    #[allow(unsafe_code)]
    unsafe {
        command.pre_exec(|| {
            for fd in 3..65_536_i32 {
                rustix::io::close(fd);
            }
            Ok(())
        });
    }
}

#[cfg(windows)]
fn configure_daemon(command: &mut std::process::Command) {
    use std::os::windows::process::CommandExt as _;
    use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW};

    // Rust's Command implementation builds an explicit inherited-handle list
    // for redirected stdio on supported Windows versions. Only the three
    // configured standard handles are therefore inherited by the daemon.
    command.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
}
