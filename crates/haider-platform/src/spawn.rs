use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub const DAEMON_LOG_DIRECTORY: &str = "daemon-logs";
pub const DAEMON_LOG_FILE: &str = "daemon.log";
pub const DAEMON_LOG_RETENTION: usize = 32;
pub const DAEMON_LOG_PATH_ENV: &str = "HAIDER_DAEMON_PROCESS_LOG";
static LOG_SEQUENCE: AtomicU64 = AtomicU64::new(0);

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

/// Allocates a collision-resistant, per-launch log path.
///
/// The daemon PID is not known until after its stdio handles are opened, so
/// the launcher PID, nanosecond timestamp, and a process-local sequence form
/// the name. Distinct files are the serialization mechanism: no two daemon
/// candidates ever share a writable description, even during a spawn burst.
pub fn allocate_daemon_log_path(store_dir: &Path) -> std::io::Result<PathBuf> {
    let directory = store_dir.join(DAEMON_LOG_DIRECTORY);
    std::fs::create_dir_all(&directory)?;
    prune_old_daemon_logs(&directory);
    for _ in 0..128 {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let sequence = LOG_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = directory.join(format!(
            "haiderd-{}-{timestamp}-{sequence}.log",
            std::process::id()
        ));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(file) => {
                drop(file);
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not allocate a unique per-process daemon log after 128 attempts",
    ))
}

/// Keep the newest bounded history. Unlinking a live file is safe on Unix; the
/// winning daemon also publishes a hard link at `daemon.log`, so its inode
/// remains named even after an extreme contention burst. Cleanup is best
/// effort: failure to inspect or remove an old diagnostic never blocks start.
fn prune_old_daemon_logs(directory: &Path) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let mut logs = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            let matches = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("haiderd-") && name.ends_with(".log"));
            if !matches {
                return None;
            }
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, path))
        })
        .collect::<Vec<_>>();
    logs.sort_by_key(|(modified, _)| *modified);
    let remove = logs
        .len()
        .saturating_sub(DAEMON_LOG_RETENTION.saturating_sub(1));
    for (_, path) in logs.into_iter().take(remove) {
        let _ = std::fs::remove_file(path);
    }
}

/// Makes the lock-winning process log discoverable at the historical
/// `daemon.log` path without making that path a shared writable destination.
///
/// The launcher passes the candidate's isolated path in the environment. Only
/// the daemon that already owns the profile lifetime lock calls this function,
/// so losing candidates cannot replace the incumbent pointer. A hard link
/// keeps both names on one inode and preserves owner-only permissions.
pub fn publish_active_daemon_log(store_dir: &Path) -> std::io::Result<bool> {
    let Some(process_log) = std::env::var_os(DAEMON_LOG_PATH_ENV).map(PathBuf::from) else {
        return Ok(false);
    };
    publish_active_daemon_log_path(store_dir, &process_log)?;
    Ok(true)
}

fn publish_active_daemon_log_path(store_dir: &Path, process_log: &Path) -> std::io::Result<()> {
    let stable = store_dir.join(DAEMON_LOG_FILE);
    let temporary = store_dir.join(format!(".{DAEMON_LOG_FILE}.{}.tmp", std::process::id()));
    match std::fs::remove_file(&temporary) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    std::fs::hard_link(process_log, &temporary)?;
    #[cfg(windows)]
    match std::fs::remove_file(&stable) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    if let Err(error) = std::fs::rename(&temporary, &stable) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(())
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
    command.env(DAEMON_LOG_PATH_ENV, spec.log_path);

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

#[cfg(test)]
mod log_tests {
    use super::*;
    use std::collections::HashSet;
    use std::fs::OpenOptions;
    use std::io::Write as _;

    const CHILD_STORE: &str = "HAIDER_LOG_TEST_CHILD_STORE";
    const CHILD_RECEIPT: &str = "HAIDER_LOG_TEST_CHILD_RECEIPT";
    const CHILD_MARKER: &str = "HAIDER_LOG_TEST_CHILD_MARKER";

    #[test]
    fn concurrent_process_writer_child() {
        let Some(store) = std::env::var_os(CHILD_STORE) else {
            return;
        };
        let receipt = std::env::var_os(CHILD_RECEIPT)
            .map(PathBuf::from)
            .unwrap_or_else(|| panic!("child receipt is missing"));
        let marker =
            std::env::var(CHILD_MARKER).unwrap_or_else(|error| panic!("child marker: {error}"));
        let path = allocate_daemon_log_path(Path::new(&store))
            .unwrap_or_else(|error| panic!("child allocate: {error}"));
        std::fs::write(&receipt, path.to_string_lossy().as_bytes())
            .unwrap_or_else(|error| panic!("child receipt: {error}"));
        let mut file = OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap_or_else(|error| panic!("child log: {error}"));
        let body = "x".repeat(640);
        for index in 0..128 {
            writeln!(file, "{marker}:{index:03}:{body}")
                .unwrap_or_else(|error| panic!("child write: {error}"));
        }
        file.sync_data()
            .unwrap_or_else(|error| panic!("child sync: {error}"));
    }

    /// MUTATION PIN: return `store_dir.join("daemon.log")` from
    /// `allocate_daemon_log_path`. This test then sees multiple process
    /// markers in one file instead of one intact writer per file.
    #[test]
    fn concurrent_process_writers_have_isolated_uncorrupted_lines() {
        let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        let executable = std::env::current_exe()
            .unwrap_or_else(|error| panic!("current test executable: {error}"));
        let mut children = Vec::new();
        let mut receipts = Vec::new();
        for index in 0..8 {
            let marker = format!("writer-{index}");
            let receipt = root.path().join(format!("receipt-{index}"));
            let child = std::process::Command::new(&executable)
                .arg("--exact")
                .arg("spawn::log_tests::concurrent_process_writer_child")
                .arg("--nocapture")
                .env(CHILD_STORE, root.path())
                .env(CHILD_RECEIPT, &receipt)
                .env(CHILD_MARKER, &marker)
                .spawn()
                .unwrap_or_else(|error| panic!("spawn writer child: {error}"));
            children.push(child);
            receipts.push((receipt, marker));
        }
        for mut child in children {
            let status = child
                .wait()
                .unwrap_or_else(|error| panic!("wait writer child: {error}"));
            assert!(status.success(), "writer child failed: {status}");
        }
        let paths = receipts
            .into_iter()
            .map(|(receipt, marker)| {
                let path = std::fs::read_to_string(&receipt)
                    .map(PathBuf::from)
                    .unwrap_or_else(|error| panic!("read {}: {error}", receipt.display()));
                (path, marker)
            })
            .collect::<Vec<_>>();
        for (path, marker) in &paths {
            let text = std::fs::read_to_string(path)
                .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
            let lines = text.lines().collect::<Vec<_>>();
            assert_eq!(lines.len(), 128);
            assert!(
                lines.iter().all(|line| line.starts_with(marker.as_str())
                    && line.len() == marker.len() + 1 + 3 + 1 + 640),
                "{} contains a foreign or partial line",
                path.display()
            );
        }
        assert_eq!(
            paths
                .iter()
                .map(|(path, _)| path)
                .collect::<HashSet<_>>()
                .len(),
            paths.len(),
            "every process must own a distinct log path"
        );
    }

    #[test]
    fn lock_winner_publishes_the_stable_legacy_log_without_copying() {
        let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        let process_log = allocate_daemon_log_path(root.path())
            .unwrap_or_else(|error| panic!("allocate log: {error}"));
        std::fs::write(&process_log, b"winner\n")
            .unwrap_or_else(|error| panic!("write log: {error}"));
        publish_active_daemon_log_path(root.path(), &process_log)
            .unwrap_or_else(|error| panic!("publish: {error}"));

        let stable = root.path().join(DAEMON_LOG_FILE);
        assert_eq!(
            std::fs::read(&stable).unwrap_or_else(|error| panic!("read stable: {error}")),
            b"winner\n"
        );
        OpenOptions::new()
            .append(true)
            .open(&process_log)
            .and_then(|mut log| log.write_all(b"more\n"))
            .unwrap_or_else(|error| panic!("extend log: {error}"));
        assert_eq!(
            std::fs::read(stable).unwrap_or_else(|error| panic!("reread stable: {error}")),
            b"winner\nmore\n"
        );
    }

    #[test]
    fn per_process_log_history_is_count_bounded() {
        let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        for _ in 0..(DAEMON_LOG_RETENTION + 8) {
            allocate_daemon_log_path(root.path())
                .unwrap_or_else(|error| panic!("allocate retained log: {error}"));
        }
        let count = std::fs::read_dir(root.path().join(DAEMON_LOG_DIRECTORY))
            .unwrap_or_else(|error| panic!("read log directory: {error}"))
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with("haiderd-") && name.ends_with(".log"))
            })
            .count();
        assert_eq!(count, DAEMON_LOG_RETENTION);
    }
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
