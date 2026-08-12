//! Bare-`haider` auto-spawn (report R8): connect first, spawn a detached
//! sibling `haiderd` only on a missing/refused endpoint, then handshake-poll.
//!
//! Laws:
//!
//! - The client NEVER takes the profile lock and NEVER removes a socket;
//!   stale-endpoint recovery belongs exclusively to the lock-winning daemon.
//! - A live daemon is NEVER killed or replaced: version/feature skew is an
//!   explicit diagnostic ([`EnsureError::MissingFeatures`],
//!   [`EnsureError::ProtocolMismatch`]), and the incumbent keeps serving.
//! - Two racing launchers may both spawn; the store lock elects exactly one
//!   winner and the loser child exits 75 (`EX_TEMPFAIL`), which the poll loop
//!   treats as expected — both parents keep polling the endpoint and attach
//!   to whichever daemon won.
//! - The sibling executable next to `current_exe()` is the packaging
//!   authority; `PATH` is diagnostic convenience only, never silently
//!   executed.
//! - Parent exit leaves the daemon running; nothing here implies shutdown.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus, Stdio};
use std::time::Duration;

use haider_rpc::{
    FEATURE_CONTEXT_COMPACTION_V1, FEATURE_SESSION_MUTATION_V1, FEATURE_TURN_CONTROL_V1,
    LifecyclePhase, ProtocolError, Welcome,
};
use tokio::time::{Instant, sleep};

use crate::client::{ClientConfig, ConnectError, Connected, connect};
use crate::profile::ResolvedProfile;

/// Feature families bare `haider` requires for live turns (R7).
///
/// The account/login features are advertised by the same daemon build; the
/// front door requires the turn engine and lets `/login` probe its own
/// feature at use time.
pub fn required_live_features() -> BTreeSet<String> {
    BTreeSet::from([
        FEATURE_CONTEXT_COMPACTION_V1.to_owned(),
        FEATURE_SESSION_MUTATION_V1.to_owned(),
        FEATURE_TURN_CONTROL_V1.to_owned(),
    ])
}

/// Name of the owner-only daemon log inside the profile store directory.
pub const DAEMON_LOG_FILE: &str = "daemon.log";

/// R8 step 7: configurable startup deadline for spawn + handshake polling.
pub const STARTUP_DEADLINE: Duration = Duration::from_secs(30);

/// The race loser's expected exit code (`EX_TEMPFAIL`).
pub const RACE_LOSER_EXIT_CODE: i32 = 75;

/// Returns whether the authenticated endpoint is served by this launcher's
/// retained daemon candidate. Kept public only so the no-wait law can run in
/// a socket-independent test sandbox.
#[doc(hidden)]
#[must_use]
pub fn authenticated_peer_is_candidate(peer_pid: Option<u32>, candidate_pid: u32) -> bool {
    peer_pid == Some(candidate_pid)
}

/// Options for [`ensure_daemon`].
#[derive(Debug, Clone)]
pub struct EnsureOptions {
    /// Feature families this client refuses to run without.
    pub required_features: BTreeSet<String>,
    /// Bound on connect/spawn/handshake polling.
    pub startup_deadline: Duration,
    /// Explicit daemon executable override (tests); default is the sibling
    /// `haiderd` next to `current_exe()`.
    pub daemon_binary: Option<PathBuf>,
    /// Connection parameters for every dial attempt.
    pub client: ClientConfig,
}

impl Default for EnsureOptions {
    fn default() -> Self {
        Self {
            required_features: required_live_features(),
            startup_deadline: STARTUP_DEADLINE,
            daemon_binary: None,
            client: ClientConfig::default(),
        }
    }
}

/// Outcome of [`ensure_daemon`].
pub struct EnsuredDaemon {
    pub client: crate::client::RpcClient,
    pub welcome: Welcome,
    /// Whether this call spawned a daemon candidate (the candidate may still
    /// have lost the store-lock race to a concurrent launcher).
    pub spawned: bool,
    /// A spawned candidate exited 75: another daemon won the race.
    pub race_lost: bool,
}

/// Typed failure of the auto-spawn front door.
#[derive(Debug)]
pub enum EnsureError {
    /// A non-spawnable connect/handshake failure (permission, malformed
    /// handshake, transport fault). Never answered by spawning.
    Connect(ConnectError),
    /// No wire-version overlap with the running daemon: fatal protocol
    /// mismatch. Never spawn a competing daemon — the singleton is already
    /// serving active state.
    ProtocolMismatch(ProtocolError),
    /// Wire overlap but the running daemon lacks required feature families.
    MissingFeatures {
        missing: BTreeSet<String>,
        daemon_version: String,
    },
    /// The endpoint serves a different profile than this client resolved.
    ProfileMismatch { expected: String, actual: String },
    /// The sibling `haiderd` executable is missing or could not be spawned.
    Spawn { binary: PathBuf, message: String },
    /// The spawned candidate exited abnormally while the endpoint stayed
    /// unreachable.
    DaemonExited {
        status: ExitStatus,
        log_path: PathBuf,
    },
    /// The startup deadline elapsed before a ready daemon answered.
    StartupTimeout {
        last_error: Option<ConnectError>,
        child_status: Option<ExitStatus>,
        log_path: PathBuf,
    },
}

impl std::fmt::Display for EnsureError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connect(error) => write!(formatter, "{error}"),
            Self::ProtocolMismatch(error) => write!(
                formatter,
                "running daemon speaks an incompatible wire protocol ({error}); \
                 stop/upgrade it manually — it was not killed"
            ),
            Self::MissingFeatures {
                missing,
                daemon_version,
            } => write!(
                formatter,
                "running daemon (version {daemon_version}) is too old for live turns/login \
                 (missing feature families: {}); stop/upgrade the running daemon — it was not killed",
                missing.iter().cloned().collect::<Vec<_>>().join(", ")
            ),
            Self::ProfileMismatch { expected, actual } => write!(
                formatter,
                "daemon at this endpoint serves profile {actual}, expected {expected}"
            ),
            Self::Spawn { binary, message } => write!(
                formatter,
                "cannot launch daemon {}: {message}",
                binary.display()
            ),
            Self::DaemonExited { status, log_path } => write!(
                formatter,
                "daemon candidate exited ({status}) before the endpoint became reachable; \
                 see {}",
                log_path.display()
            ),
            Self::StartupTimeout {
                last_error,
                child_status,
                log_path,
            } => {
                write!(
                    formatter,
                    "daemon did not become ready before the startup deadline"
                )?;
                if let Some(error) = last_error {
                    write!(formatter, " (last attempt: {error})")?;
                }
                if let Some(status) = child_status {
                    write!(formatter, " (candidate exit: {status})")?;
                }
                write!(formatter, "; see {}", log_path.display())
            }
        }
    }
}

impl std::error::Error for EnsureError {}

enum Attach {
    Ready(Box<Connected>),
    /// Connect refused/missing — the spawn trigger class.
    Spawnable(ConnectError),
    /// Connected but not yet attachable (daemon still starting or draining
    /// out); poll again without spawning a competitor.
    NotYet,
    Fatal(Box<EnsureError>),
}

async fn try_attach(profile: &ResolvedProfile, options: &EnsureOptions) -> Attach {
    match connect(&profile.endpoint_path, options.client.clone()).await {
        Ok(connected) => {
            let welcome = &connected.welcome;
            if !welcome.profile_id.is_empty() && welcome.profile_id != profile.profile_id {
                connected.client.close();
                return Attach::Fatal(Box::new(EnsureError::ProfileMismatch {
                    expected: profile.profile_id.clone(),
                    actual: welcome.profile_id.clone(),
                }));
            }
            let missing: BTreeSet<String> = options
                .required_features
                .iter()
                .filter(|feature| !welcome.features.contains(*feature))
                .cloned()
                .collect();
            if !missing.is_empty() {
                // Wire overlap but missing W3c features: explicit skew
                // diagnostic; NEVER kill the incumbent.
                let daemon_version = welcome.daemon_version.clone();
                connected.client.close();
                return Attach::Fatal(Box::new(EnsureError::MissingFeatures {
                    missing,
                    daemon_version,
                }));
            }
            match welcome.lifecycle_phase {
                LifecyclePhase::Ready => Attach::Ready(Box::new(connected)),
                // A draining/starting daemon is live; wait for it to finish
                // rather than racing a competitor against its endpoint.
                _ => {
                    connected.client.close();
                    Attach::NotYet
                }
            }
        }
        Err(error) if error.is_spawnable() => Attach::Spawnable(error),
        Err(ConnectError::Rejected(protocol_error)) => {
            if protocol_error.code == "protocol_version_mismatch" {
                Attach::Fatal(Box::new(EnsureError::ProtocolMismatch(protocol_error)))
            } else {
                Attach::Fatal(Box::new(EnsureError::Connect(ConnectError::Rejected(
                    protocol_error,
                ))))
            }
        }
        Err(error) => Attach::Fatal(Box::new(EnsureError::Connect(error))),
    }
}

/// Connects to the profile daemon, spawning a detached sibling `haiderd`
/// only when the endpoint is missing/refused (R8).
pub async fn ensure_daemon(
    profile: &ResolvedProfile,
    options: EnsureOptions,
) -> Result<EnsuredDaemon, EnsureError> {
    let log_path = profile.store_dir.join(DAEMON_LOG_FILE);
    let mut child: Option<Child> = None;
    let mut spawned = false;
    let mut race_lost = false;
    let mut child_status: Option<ExitStatus> = None;
    let mut last_error: Option<ConnectError> = None;

    let deadline = Instant::now() + options.startup_deadline;
    let mut backoff = Duration::from_millis(25);
    loop {
        match try_attach(profile, &options).await {
            Attach::Ready(connected) => {
                let Connected {
                    client,
                    welcome,
                    peer_credentials,
                } = *connected;
                // Reap a still-running race-loser candidate before returning:
                // its store-lock loss makes exit 75 imminent, and the parent
                // process is long-running from W3c3 on — an unreaped child
                // would linger as a zombie for the parent's lifetime (W3c2
                // review finding 6). One bounded grace poll, then release.
                if let Some(mut active) = child.take() {
                    // The authenticated endpoint PID distinguishes our
                    // healthy winner from a candidate that lost to another
                    // launcher. Waiting for the healthy child to exit would
                    // add the entire one-second loser grace to every cold
                    // launch even though that daemon is meant to outlive us.
                    if !authenticated_peer_is_candidate(peer_credentials.pid, active.id()) {
                        for _ in 0..40u8 {
                            match active.try_wait() {
                                Ok(Some(status)) => {
                                    if status.code() == Some(RACE_LOSER_EXIT_CODE) {
                                        race_lost = true;
                                    }
                                    break;
                                }
                                Ok(None) => sleep(Duration::from_millis(25)).await,
                                Err(_) => break,
                            }
                        }
                    }
                }
                return Ok(EnsuredDaemon {
                    client,
                    welcome,
                    spawned,
                    race_lost,
                });
            }
            Attach::Fatal(error) => return Err(*error),
            Attach::Spawnable(error) => {
                last_error = Some(error);
                if !spawned {
                    // The one spawn decision (R8 step 5): only a missing or
                    // refused endpoint ever launches a candidate, and only
                    // one candidate per launcher.
                    child = Some(spawn_daemon(profile, &options, &log_path)?);
                    spawned = true;
                }
            }
            Attach::NotYet => {}
        }
        if let Some(active) = child.as_mut()
            && let Ok(Some(status)) = active.try_wait()
        {
            child_status = Some(status);
            child = None;
            if status.code() == Some(RACE_LOSER_EXIT_CODE) {
                // Expected: a concurrent launcher's daemon won the store
                // lock; keep polling the shared endpoint (R8 step 7).
                race_lost = true;
            } else if !status.success() {
                // A candidate that died for any other reason will never
                // serve; if the endpoint is also unreachable, fail loudly
                // with the log path instead of burning the whole deadline.
                if let Attach::Spawnable(_) = try_attach(profile, &options).await {
                    return Err(EnsureError::DaemonExited { status, log_path });
                }
            }
        }
        if Instant::now() + backoff > deadline {
            return Err(EnsureError::StartupTimeout {
                last_error,
                child_status,
                log_path,
            });
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_millis(400));
    }
}

/// Resolves the daemon executable: the sibling next to `current_exe()` is
/// the packaging authority (an explicit override serves tests).
fn daemon_binary(options: &EnsureOptions) -> Result<PathBuf, EnsureError> {
    if let Some(binary) = &options.daemon_binary {
        return Ok(binary.clone());
    }
    let current = std::env::current_exe().map_err(|error| EnsureError::Spawn {
        binary: PathBuf::from("haiderd"),
        message: format!("cannot resolve current executable: {error}"),
    })?;
    let sibling = current
        .parent()
        .map(|dir| dir.join(format!("haiderd{}", std::env::consts::EXE_SUFFIX)))
        .unwrap_or_else(|| PathBuf::from("haiderd"));
    if sibling.exists() {
        return Ok(sibling);
    }
    // PATH is diagnostic convenience only — named, never executed.
    Err(EnsureError::Spawn {
        binary: sibling,
        message: "sibling haiderd executable is missing; install haiderd next to haider \
                  (a haiderd elsewhere on PATH is not silently trusted)"
            .into(),
    })
}

fn spawn_daemon(
    profile: &ResolvedProfile,
    options: &EnsureOptions,
    log_path: &Path,
) -> Result<Child, EnsureError> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::process::CommandExt;

    let binary = daemon_binary(options)?;
    // Owner-only (0600) profile daemon log; stdout and stderr both append.
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(log_path)
        .map_err(|error| EnsureError::Spawn {
            binary: binary.clone(),
            message: format!("cannot open daemon log {}: {error}", log_path.display()),
        })?;
    let log_err = log.try_clone().map_err(|error| EnsureError::Spawn {
        binary: binary.clone(),
        message: format!("cannot clone daemon log handle: {error}"),
    })?;
    let mut command = std::process::Command::new(&binary);
    command
        .arg("--profile")
        .arg(&profile.profile_id)
        .arg("--store-dir")
        .arg(&profile.store_dir)
        .arg("--runtime-dir")
        .arg(&profile.runtime_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        // Detach from the launcher's terminal/process group so terminal
        // signals never reach the daemon and parent exit leaves it running.
        .process_group(0);
    // The daemon outlives its launcher, so an accidentally inherited
    // descriptor outlives the caller's expectations with it. macOS creates
    // pipes without atomic CLOEXEC (`pipe()` then `fcntl`), a concurrent
    // spawn on another thread can capture a sibling's descriptor inside that
    // window, and `Command` demonstrably forwards non-CLOEXEC descriptors on
    // this platform — a leaked pipe write end held by the long-lived daemon
    // then starves the original reader of EOF forever. Close every
    // descriptor above stderr between fork and exec so the daemon starts
    // with exactly its three configured stdio descriptors (`close(2)` is
    // async-signal-safe).
    //
    // SAFETY: the hook runs between fork and exec and calls only the
    // async-signal-safe `close(2)`; no allocation, locking, or Rust runtime
    // machinery is touched.
    #[allow(unsafe_code)]
    unsafe {
        use std::os::unix::process::CommandExt as _;
        command.pre_exec(|| {
            for fd in 3..65_536i32 {
                rustix::io::close(fd);
            }
            Ok(())
        });
    }
    command.spawn().map_err(|error| EnsureError::Spawn {
        binary,
        message: error.to_string(),
    })
}

/// Starts an explicitly named daemon sibling and returns the live child.
///
/// This update-only seam has the same exact profile arguments, owner-only
/// log, and detached process group as ordinary auto-spawn, but retaining the
/// child lets the updater stop and reap a candidate that fails its exact
/// Welcome-version health check.
///
/// MUTATION SAFETY: this starts a daemon process and appends to the profile's
/// owner-only daemon log. Runtime failures are returned as [`EnsureError`];
/// callers must retain the child until health succeeds.
pub fn spawn_daemon_retained(
    profile: &ResolvedProfile,
    binary: impl Into<PathBuf>,
) -> Result<Child, EnsureError> {
    let options = EnsureOptions {
        daemon_binary: Some(binary.into()),
        ..EnsureOptions::default()
    };
    let log_path = profile.store_dir.join(DAEMON_LOG_FILE);
    spawn_daemon(profile, &options, &log_path)
}

/// Sends the one graceful termination signal to an authenticated UDS peer.
///
/// MUTATION SAFETY: this changes process state. The function performs one
/// `SIGTERM` syscall and never retries; a runtime failure is returned to the
/// caller so update cannot accidentally turn a timeout into a second signal.
pub fn signal_authenticated_peer(pid: u32) -> std::io::Result<()> {
    use rustix::process::{Pid, Signal, kill_process};

    let raw = i32::try_from(pid)
        .ok()
        .and_then(Pid::from_raw)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid peer PID"))?;
    kill_process(raw, Signal::TERM).map_err(std::io::Error::from)
}
