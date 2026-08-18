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

    #[cfg(unix)]
    configure_daemon(&mut command);
    #[cfg(windows)]
    configure_daemon(&mut command).map_err(DaemonSpawnError::Spawn)?;
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
            crate::process::close_inherited_descriptors();
            Ok(())
        });
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn configure_daemon(command: &mut std::process::Command) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt as _;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW};

    // Rust's stable `Command` duplicates the three configured child stdio
    // handles as inheritable, but still calls `CreateProcessW` with
    // `bInheritHandles=TRUE` and no explicit handle list. If this launcher was
    // itself started with captured stdio, those original pipe handles are
    // inheritable too. A daemon that outlives the launcher then keeps the
    // capture pipes open forever, so `Command::output` never observes EOF.
    //
    // Clear inheritance on the launcher's standard handles permanently — the
    // Windows equivalent of CLOEXEC. A later Rust child that intentionally
    // uses `Stdio::inherit` still works: `Command` duplicates the selected
    // handle with inheritance enabled for that one spawn.
    for (kind, name) in [
        (STD_INPUT_HANDLE, "stdin"),
        (STD_OUTPUT_HANDLE, "stdout"),
        (STD_ERROR_HANDLE, "stderr"),
    ] {
        let handle = unsafe { GetStdHandle(kind) };
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            continue;
        }
        clear_handle_inheritance(handle, name)?;
    }
    command.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
    Ok(())
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn clear_handle_inheritance(
    handle: windows_sys::Win32::Foundation::HANDLE,
    name: &str,
) -> std::io::Result<()> {
    use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};

    if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) } != 0 {
        return Ok(());
    }
    let source = std::io::Error::last_os_error();
    Err(std::io::Error::new(
        source.kind(),
        format!("cannot clear inheritance on daemon launcher's {name}: {source}"),
    ))
}

#[cfg(all(test, windows))]
mod windows_tests {
    use std::os::windows::io::AsRawHandle as _;

    use windows_sys::Win32::Foundation::{
        GetHandleInformation, HANDLE_FLAG_INHERIT, SetHandleInformation,
    };

    use super::clear_handle_inheritance;

    /// The daemon-spawn hygiene must clear the exact Win32 flag that would
    /// otherwise let a long-lived daemon retain its launcher's capture pipe.
    #[test]
    // Test setup failures should retain their exact Win32 fixture context.
    #[allow(unsafe_code, clippy::expect_used)]
    fn daemon_spawn_hygiene_clears_an_inheritable_handle() {
        let path = std::env::temp_dir().join(format!(
            "haider-platform-inheritance-{}.tmp",
            std::process::id()
        ));
        let file = std::fs::File::create(&path).expect("create inheritance fixture");
        let handle = file.as_raw_handle().cast();
        assert_ne!(
            unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) },
            0,
            "make fixture handle inheritable"
        );

        clear_handle_inheritance(handle, "fixture").expect("clear fixture inheritance");
        let mut flags = 0;
        assert_ne!(
            unsafe { GetHandleInformation(handle, &mut flags) },
            0,
            "read fixture handle flags"
        );
        assert_eq!(flags & HANDLE_FLAG_INHERIT, 0);

        drop(file);
        std::fs::remove_file(path).expect("remove inheritance fixture");
    }
}
