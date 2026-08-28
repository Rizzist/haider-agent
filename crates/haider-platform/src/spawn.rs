use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(test)]
#[path = "spawn_tests.rs"]
mod readiness_tests;

#[cfg(unix)]
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};

#[cfg(windows)]
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};

#[cfg(windows)]
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

pub const DAEMON_LOG_DIRECTORY: &str = "daemon-logs";
pub const DAEMON_LOG_FILE: &str = "daemon.log";
pub const DAEMON_LOG_RETENTION: usize = 32;
pub const DAEMON_LOG_PATH_ENV: &str = "HAIDER_DAEMON_PROCESS_LOG";
pub const DAEMON_READINESS_ARG: &str = "--startup-ready";
pub const DAEMON_LIVENESS_ARG: &str = "--launcher-liveness";
#[cfg(unix)]
const DAEMON_READINESS_TOKEN: &str = "3";
#[cfg(windows)]
const DAEMON_READINESS_TOKEN: &str = "stdin";
#[cfg(unix)]
const DAEMON_LIVENESS_TOKEN: &str = "4";
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
    Readiness(std::io::Error),
    Spawn(std::io::Error),
}

/// A spawned daemon together with the one-shot readiness receiver owned by
/// its launcher.
pub struct SpawnedDaemon {
    pub child: Child,
    pub readiness: DaemonReadiness,
}

/// Launcher-owned half of an ephemeral daemon's process-liveness channel.
///
/// On Unix the open writer is the proof that the launcher still exists. On
/// Windows the daemon waits on an inherited handle to the launcher process;
/// this marker keeps the ownership contract explicit at the call site even
/// though process termination, rather than handle closure, is the signal.
pub struct DaemonLivenessGuard {
    #[cfg(unix)]
    _writer: OwnedFd,
}

/// Daemon-owned half of an ephemeral launcher's process-liveness channel.
pub struct DaemonLivenessWatcher {
    #[cfg(unix)]
    reader: OwnedFd,
    #[cfg(windows)]
    launcher: OwnedHandle,
}

/// The launcher side of a one-byte daemon readiness notification.
pub struct DaemonReadiness {
    #[cfg(unix)]
    reader: OwnedFd,
    #[cfg(windows)]
    server: NamedPipeServer,
}

/// The daemon side of a one-byte readiness notification.
pub struct DaemonReadyNotifier {
    #[cfg(unix)]
    writer: OwnedFd,
    #[cfg(windows)]
    pipe: std::fs::File,
}

impl DaemonReadiness {
    /// Waits until the spawned daemon explicitly reports that its listener and
    /// lifecycle state are both ready. EOF is a typed I/O failure, not Ready.
    pub async fn wait(self) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            let reader = tokio::io::unix::AsyncFd::new(self.reader)?;
            let mut byte = [0_u8; 1];
            let read = reader
                .async_io(tokio::io::Interest::READABLE, |reader| {
                    rustix::io::read(reader, &mut byte).map_err(std::io::Error::from)
                })
                .await?;
            if read == 1 && byte == [1] {
                return Ok(());
            }
            Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "daemon readiness channel closed without a notification",
            ))
        }
        #[cfg(windows)]
        {
            use tokio::io::AsyncReadExt as _;

            let mut server = self.server;
            server.connect().await?;
            let mut byte = [0_u8; 1];
            server.read_exact(&mut byte).await?;
            if byte == [1] {
                return Ok(());
            }
            Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "daemon readiness channel carried an invalid notification",
            ))
        }
    }
}

impl DaemonReadyNotifier {
    /// Adopts the readiness coordinate passed in the daemon's private argv.
    /// The daemon immediately disables further inheritance so pre-Ready
    /// subprocesses cannot retain the writer if it exits before publishing
    /// Ready.
    #[allow(unsafe_code)]
    pub fn from_spawn_token(token: &str) -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            if token != DAEMON_READINESS_TOKEN {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "invalid daemon readiness descriptor",
                ));
            }
            // SAFETY: the launcher moves its owned pipe writer to descriptor 3
            // in `pre_exec` and passes exactly that descriptor in argv. This is
            // the daemon's first and only adoption of it.
            let writer = unsafe { OwnedFd::from_raw_fd(3) };
            rustix::io::fcntl_setfd(&writer, rustix::io::FdFlags::CLOEXEC)
                .map_err(std::io::Error::from)?;
            Ok(Self { writer })
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _};
            use windows_sys::Win32::Foundation::{
                HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation,
            };
            use windows_sys::Win32::System::Console::{GetStdHandle, STD_INPUT_HANDLE};

            if token != DAEMON_READINESS_TOKEN {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "invalid daemon readiness pipe coordinate",
                ));
            }
            let raw = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
            if raw.is_null() || raw == INVALID_HANDLE_VALUE {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "daemon readiness pipe is missing from standard input",
                ));
            }
            // SAFETY: the launcher supplies its named-pipe writer as this
            // child's stdin through Command's protected stdio inheritance
            // path. No daemon code reads stdin, and this is its only adoption.
            let pipe = unsafe { std::fs::File::from_raw_handle(raw) };
            if unsafe { SetHandleInformation(pipe.as_raw_handle().cast(), HANDLE_FLAG_INHERIT, 0) }
                == 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(Self { pipe })
        }
    }

    /// Emits the single buffered Ready byte and closes the notification side.
    pub fn notify(self) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            let written = rustix::io::write(&self.writer, &[1]).map_err(std::io::Error::from)?;
            if written == 1 {
                return Ok(());
            }
            Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "daemon readiness notification was not written",
            ))
        }
        #[cfg(windows)]
        {
            use std::io::Write as _;

            let mut pipe = self.pipe;
            pipe.write_all(&[1])
        }
    }
}

impl DaemonLivenessWatcher {
    /// Adopts the liveness coordinate inherited from an ephemeral launcher.
    #[allow(unsafe_code)]
    pub fn from_spawn_token(token: &str) -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            if token != DAEMON_LIVENESS_TOKEN {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "invalid launcher-liveness descriptor",
                ));
            }
            // SAFETY: the launcher moves its owned pipe reader to descriptor 4
            // in `pre_exec` and passes exactly that descriptor in argv. This is
            // the daemon's first and only adoption of it.
            let reader = unsafe { OwnedFd::from_raw_fd(4) };
            rustix::io::fcntl_setfd(&reader, rustix::io::FdFlags::CLOEXEC)
                .map_err(std::io::Error::from)?;
            Ok(Self { reader })
        }
        #[cfg(windows)]
        {
            use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};

            let raw = token.parse::<usize>().map_err(|error| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("invalid launcher process handle: {error}"),
                )
            })? as std::os::windows::io::RawHandle;
            if raw.is_null() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "launcher process handle is null",
                ));
            }
            // SAFETY: the launcher passes the numeric value of the real,
            // inheritable process handle copied into this daemon by
            // CreateProcess. This is the child's first and only adoption.
            let launcher = unsafe { OwnedHandle::from_raw_handle(raw) };
            if unsafe {
                SetHandleInformation(launcher.as_raw_handle().cast(), HANDLE_FLAG_INHERIT, 0)
            } == 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(Self { launcher })
        }
    }

    /// Waits for the launcher to disappear. No client cleanup code is needed:
    /// Unix reports EOF after the last writer closes, while Windows signals
    /// the inherited process handle for every exit path, including kill.
    pub async fn wait(self) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            let reader = tokio::io::unix::AsyncFd::new(self.reader)?;
            let mut byte = [0_u8; 1];
            let read = reader
                .async_io(tokio::io::Interest::READABLE, |reader| {
                    rustix::io::read(reader, &mut byte).map_err(std::io::Error::from)
                })
                .await?;
            if read == 0 {
                Ok(())
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "launcher-liveness channel carried unexpected data",
                ))
            }
        }
        #[cfg(windows)]
        {
            use windows_sys::Win32::Foundation::{WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};
            use windows_sys::Win32::System::Threading::{INFINITE, WaitForSingleObject};

            let (sender, receiver) = tokio::sync::oneshot::channel();
            // Rendezvous after a zero-time kernel probe. Once it reports
            // alive, process-handle signaling is sticky, so a launcher exit
            // between this handshake and the infinite wait cannot be lost.
            // This makes the outer runtime's first-poll "armed" fence real.
            let (armed_sender, armed_receiver) = std::sync::mpsc::sync_channel(0);
            std::thread::Builder::new()
                .name("haider-launcher-liveness".into())
                .spawn(move || {
                    let handle = self.launcher.as_raw_handle().cast();
                    let initial = unsafe { WaitForSingleObject(handle, 0) };
                    if initial != WAIT_TIMEOUT {
                        let outcome = match initial {
                            WAIT_OBJECT_0 => Ok(()),
                            WAIT_FAILED => Err(std::io::Error::last_os_error()),
                            other => Err(std::io::Error::other(format!(
                                "launcher process wait returned unexpected status {other}"
                            ))),
                        };
                        let _ = sender.send(outcome);
                        let _ = armed_sender.send(());
                        return;
                    }
                    if armed_sender.send(()).is_err() {
                        return;
                    }
                    let result = unsafe { WaitForSingleObject(handle, INFINITE) };
                    let outcome = match result {
                        WAIT_OBJECT_0 => Ok(()),
                        WAIT_FAILED => Err(std::io::Error::last_os_error()),
                        other => Err(std::io::Error::other(format!(
                            "launcher process wait returned unexpected status {other}"
                        ))),
                    };
                    let _ = sender.send(outcome);
                })
                .map_err(|error| {
                    std::io::Error::new(
                        error.kind(),
                        format!("could not start launcher-liveness thread: {error}"),
                    )
                })?;
            armed_receiver.recv().map_err(|error| {
                std::io::Error::other(format!(
                    "launcher-liveness thread did not arm its kernel wait: {error}"
                ))
            })?;
            receiver.await.map_err(|error| {
                std::io::Error::other(format!("launcher-liveness thread failed: {error}"))
            })?
        }
    }
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
            Self::Readiness(error) => {
                write!(formatter, "cannot create daemon readiness channel: {error}")
            }
            Self::Spawn(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for DaemonSpawnError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::OpenLog(error)
            | Self::CloneLog(error)
            | Self::Readiness(error)
            | Self::Spawn(error) => Some(error),
        }
    }
}

/// Spawns the sibling daemon with its fixed argv/stdout/stderr contract and
/// the platform's detach + inheritance hygiene.
pub fn spawn_daemon(spec: DaemonSpawn<'_>) -> Result<Child, DaemonSpawnError> {
    spawn_daemon_with_stderr(spec, false, None, None)
}

/// Spawns a daemon with a private one-shot readiness notification.
pub fn spawn_daemon_with_readiness(
    spec: DaemonSpawn<'_>,
) -> Result<SpawnedDaemon, DaemonSpawnError> {
    let prepared = prepare_readiness().map_err(DaemonSpawnError::Readiness)?;
    let token = prepared.token().to_owned();
    let coordinate = prepared
        .child_coordinate()
        .map_err(DaemonSpawnError::Readiness)?;
    let child = spawn_daemon_with_stderr(spec, false, Some((&token, coordinate)), None)?;
    Ok(SpawnedDaemon {
        child,
        readiness: prepared.into_receiver(),
    })
}

/// Spawns an ephemeral daemon with F1's unchanged readiness notification plus
/// a reverse channel that independently proves launcher process liveness.
pub fn spawn_daemon_with_readiness_and_liveness(
    spec: DaemonSpawn<'_>,
) -> Result<(SpawnedDaemon, DaemonLivenessGuard), DaemonSpawnError> {
    let readiness = prepare_readiness().map_err(DaemonSpawnError::Readiness)?;
    let readiness_token = readiness.token().to_owned();
    let readiness_coordinate = readiness
        .child_coordinate()
        .map_err(DaemonSpawnError::Readiness)?;
    let liveness = prepare_liveness().map_err(DaemonSpawnError::Readiness)?;
    let liveness_token = liveness.token().to_owned();
    let liveness_coordinate = liveness.child_coordinate();
    let child = spawn_daemon_with_stderr(
        spec,
        false,
        Some((&readiness_token, readiness_coordinate)),
        Some((&liveness_token, liveness_coordinate)),
    )?;
    Ok((
        SpawnedDaemon {
            child,
            readiness: readiness.into_receiver(),
        },
        liveness.into_guard(),
    ))
}

/// Test/diagnostic variant of [`spawn_daemon`] that leaves the child's stderr
/// piped while preserving the ordinary argv, stdout log, process group, and
/// inherited-descriptor hygiene.
///
/// The caller must continuously drain [`Child::stderr`] so a noisy daemon
/// cannot block on a full pipe.
#[doc(hidden)]
pub fn spawn_daemon_with_piped_stderr(spec: DaemonSpawn<'_>) -> Result<Child, DaemonSpawnError> {
    spawn_daemon_with_stderr(spec, true, None, None)
}

fn spawn_daemon_with_stderr(
    spec: DaemonSpawn<'_>,
    pipe_stderr: bool,
    readiness: Option<(&str, ReadinessChildCoordinate)>,
    liveness: Option<(&str, LivenessChildCoordinate)>,
) -> Result<Child, DaemonSpawnError> {
    // The daemon creates this directory only after it owns the profile lock.
    // Merely naming it here keeps a launcher killed before `exec` from
    // leaving an empty runtime tree behind.
    let runtime_temp = spec.runtime_dir.join("tmp");
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
    let stderr = if pipe_stderr {
        Stdio::piped()
    } else {
        Stdio::from(log.try_clone().map_err(DaemonSpawnError::CloneLog)?)
    };
    let mut command = std::process::Command::new(spec.binary);
    command
        .arg("--profile")
        .arg(spec.profile_id)
        .arg("--store-dir")
        .arg(spec.store_dir)
        .arg("--runtime-dir")
        .arg(spec.runtime_dir)
        .stdout(Stdio::from(log))
        .stderr(stderr);
    command.env(DAEMON_LOG_PATH_ENV, spec.log_path);
    if let Some((token, _)) = readiness.as_ref() {
        command.arg(DAEMON_READINESS_ARG).arg(*token);
    }
    if let Some((token, _)) = liveness.as_ref() {
        command.arg(DAEMON_LIVENESS_ARG).arg(*token);
    }
    #[cfg(unix)]
    command.env("TMPDIR", &runtime_temp);
    #[cfg(windows)]
    command.env("TEMP", &runtime_temp).env("TMP", &runtime_temp);

    #[cfg(unix)]
    {
        command.stdin(Stdio::null());
        configure_daemon(
            &mut command,
            readiness.map(|(_, coordinate)| coordinate),
            liveness.map(|(_, coordinate)| coordinate),
        );
        command.spawn().map_err(DaemonSpawnError::Spawn)
    }
    #[cfg(windows)]
    {
        let stdin = readiness.map_or_else(Stdio::null, |(_, writer)| Stdio::from(writer));
        command.stdin(stdin);
        configure_daemon(&mut command).map_err(DaemonSpawnError::Spawn)?;
        command.spawn().map_err(DaemonSpawnError::Spawn)
    }
}

#[cfg(unix)]
type ReadinessChildCoordinate = std::os::raw::c_int;

#[cfg(windows)]
type ReadinessChildCoordinate = std::fs::File;

#[cfg(unix)]
type LivenessChildCoordinate = std::os::raw::c_int;

#[cfg(windows)]
type LivenessChildCoordinate = ();

struct PreparedReadiness {
    token: String,
    receiver: DaemonReadiness,
    #[cfg(unix)]
    writer: OwnedFd,
    #[cfg(windows)]
    writer: std::fs::File,
}

struct PreparedLiveness {
    token: String,
    guard: DaemonLivenessGuard,
    #[cfg(unix)]
    reader: OwnedFd,
    #[cfg(windows)]
    _launcher: OwnedHandle,
}

impl PreparedLiveness {
    fn token(&self) -> &str {
        &self.token
    }

    fn child_coordinate(&self) -> LivenessChildCoordinate {
        #[cfg(unix)]
        {
            self.reader.as_raw_fd()
        }
        #[cfg(windows)]
        {}
    }

    fn into_guard(self) -> DaemonLivenessGuard {
        self.guard
    }
}

impl PreparedReadiness {
    fn token(&self) -> &str {
        &self.token
    }

    fn child_coordinate(&self) -> std::io::Result<ReadinessChildCoordinate> {
        #[cfg(unix)]
        return Ok(self.writer.as_raw_fd());
        #[cfg(windows)]
        {
            self.writer.try_clone()
        }
    }

    fn into_receiver(self) -> DaemonReadiness {
        self.receiver
    }
}

#[cfg(unix)]
fn prepare_readiness() -> std::io::Result<PreparedReadiness> {
    let (reader, writer) = readiness_pipe()?;
    // Command may fill closed standard descriptors before pre_exec. Move the
    // writer above 0..=2 now so setup cannot overwrite the coordinate.
    // F_DUPFD_CLOEXEC performs the move without opening an inheritance gap.
    let writer = readiness_writer_above_stdio(&writer)?;
    Ok(PreparedReadiness {
        token: DAEMON_READINESS_TOKEN.to_owned(),
        receiver: DaemonReadiness { reader },
        writer,
    })
}

#[cfg(unix)]
fn prepare_liveness() -> std::io::Result<PreparedLiveness> {
    let (reader, writer) = readiness_pipe()?;
    let reader = readiness_writer_above_stdio(&reader)?;
    Ok(PreparedLiveness {
        token: DAEMON_LIVENESS_TOKEN.to_owned(),
        guard: DaemonLivenessGuard { _writer: writer },
        reader,
    })
}

#[cfg(all(unix, not(target_os = "espidf")))]
fn readiness_writer_above_stdio(writer: &OwnedFd) -> std::io::Result<OwnedFd> {
    // Keep both source descriptors above the fixed daemon coordinates 3 and
    // 4, so installing one can never overwrite the other's source.
    rustix::io::fcntl_dupfd_cloexec(writer, 5).map_err(std::io::Error::from)
}

#[cfg(all(unix, target_os = "espidf"))]
fn readiness_writer_above_stdio(writer: &OwnedFd) -> std::io::Result<OwnedFd> {
    // ESP-IDF lacks F_DUPFD_CLOEXEC. Duplicate above stdio, then apply the
    // same immediate CLOEXEC fallback used by its pipe creation path.
    let duplicated = rustix::io::fcntl_dupfd(writer, 5).map_err(std::io::Error::from)?;
    rustix::io::fcntl_setfd(&duplicated, rustix::io::FdFlags::CLOEXEC)
        .map_err(std::io::Error::from)?;
    Ok(duplicated)
}

#[cfg(all(
    unix,
    not(any(
        target_vendor = "apple",
        target_os = "aix",
        target_os = "espidf",
        target_os = "haiku",
        target_os = "horizon",
        target_os = "nto"
    ))
))]
fn readiness_pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
    rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC | rustix::pipe::PipeFlags::NONBLOCK)
        .map_err(std::io::Error::from)
}

#[cfg(all(
    unix,
    any(
        target_vendor = "apple",
        target_os = "aix",
        target_os = "espidf",
        target_os = "haiku",
        target_os = "horizon",
        target_os = "nto"
    )
))]
fn readiness_pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
    // These targets expose pipe(2), but not atomic pipe2 flags. Apply CLOEXEC
    // immediately, then NONBLOCK to the launcher reader. This is the narrow
    // unavoidable fallback; supported pipe2 targets use the atomic path.
    let (reader, writer) = rustix::pipe::pipe().map_err(std::io::Error::from)?;
    rustix::io::fcntl_setfd(&reader, rustix::io::FdFlags::CLOEXEC).map_err(std::io::Error::from)?;
    rustix::io::fcntl_setfd(&writer, rustix::io::FdFlags::CLOEXEC).map_err(std::io::Error::from)?;
    let flags = rustix::fs::fcntl_getfl(&reader).map_err(std::io::Error::from)?;
    rustix::fs::fcntl_setfl(&reader, flags | rustix::fs::OFlags::NONBLOCK)
        .map_err(std::io::Error::from)?;
    Ok((reader, writer))
}

#[cfg(windows)]
fn prepare_readiness() -> std::io::Result<PreparedReadiness> {
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random).map_err(std::io::Error::other)?;
    let token = format!(
        r"\\.\pipe\haider-readiness-{}-{}",
        std::process::id(),
        random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    let server = ServerOptions::new()
        .access_inbound(true)
        .access_outbound(false)
        .first_pipe_instance(true)
        .max_instances(1)
        .in_buffer_size(1)
        .create(&token)?;
    // Connect in the launcher, then carry the writer through Command's
    // internally locked stdin-inheritance path. The child therefore owns the
    // pipe from CreateProcess onward: even a failure before Rust argument
    // parsing closes the last writer and wakes the readiness wait.
    let writer = std::fs::OpenOptions::new().write(true).open(&token)?;
    Ok(PreparedReadiness {
        token: DAEMON_READINESS_TOKEN.to_owned(),
        receiver: DaemonReadiness { server },
        writer,
    })
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn prepare_liveness() -> std::io::Result<PreparedLiveness> {
    use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};
    use windows_sys::Win32::System::Threading::{GetCurrentProcessId, OpenProcess};

    // Frozen Win32 access-right bit. Pin it locally because windows-sys has
    // moved the public module home of SYNCHRONIZE between supported releases.
    const SYNCHRONIZE: u32 = 0x0010_0000;

    let raw = unsafe { OpenProcess(SYNCHRONIZE, 1, GetCurrentProcessId()) };
    if raw.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: OpenProcess returned one newly owned real handle.
    let launcher = unsafe { OwnedHandle::from_raw_handle(raw.cast()) };
    if unsafe {
        SetHandleInformation(
            launcher.as_raw_handle().cast(),
            HANDLE_FLAG_INHERIT,
            HANDLE_FLAG_INHERIT,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(PreparedLiveness {
        token: (launcher.as_raw_handle() as usize).to_string(),
        guard: DaemonLivenessGuard {},
        _launcher: launcher,
    })
}

#[cfg(unix)]
fn configure_daemon(
    command: &mut std::process::Command,
    readiness: Option<ReadinessChildCoordinate>,
    liveness: Option<LivenessChildCoordinate>,
) {
    use std::os::unix::process::CommandExt as _;
    command.process_group(0);
    let upper_bound = crate::process::inherited_descriptor_upper_bound();
    // SAFETY: the hook runs between fork and exec and uses only raw,
    // async-signal-safe descriptor syscalls; no allocation or runtime state
    // is touched.
    #[allow(unsafe_code)]
    unsafe {
        command.pre_exec(move || {
            if readiness.is_some() || liveness.is_some() {
                crate::process::install_daemon_spawn_descriptors(readiness, liveness, upper_bound)?;
            } else {
                crate::process::close_inherited_descriptors(upper_bound);
            }
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
