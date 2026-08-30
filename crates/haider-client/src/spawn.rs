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
//! - Persistent parent exit leaves the daemon running. Ephemeral and bounded
//!   linger launches carry a kernel liveness channel so the daemon can shut
//!   itself down after the launcher vanishes, immediately or after a declared
//!   idle interval.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::{Child, ExitStatus};
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
///
/// Retained as the legacy basename for diagnostics that predate per-process
/// logs. New candidates use `daemon-logs/haiderd-<launch>.log` so racing
/// processes never share a writable file.
pub const DAEMON_LOG_FILE: &str = haider_platform::DAEMON_LOG_FILE;

/// R8 step 7: configurable startup deadline for spawn + handshake polling.
pub const STARTUP_DEADLINE: Duration = Duration::from_secs(30);

/// The race loser's expected exit code (`EX_TEMPFAIL`).
pub const RACE_LOSER_EXIT_CODE: i32 = 75;

const RACE_LOSER_REAP_ATTEMPTS: u8 = 40;
const RACE_LOSER_REAP_POLL: Duration = Duration::from_millis(25);

/// Whether a caller keeps an auto-spawned daemon persistent or tears down
/// only the exact authenticated child it launched.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DaemonLifetime {
    #[default]
    Persistent,
    /// Keep the authenticated child available for subsequent clients, then
    /// let the daemon shut itself down after this much time with no clients.
    LingerIfSpawned {
        idle_ttl: Duration,
    },
    EphemeralIfSpawned,
}

/// Returns whether the authenticated endpoint is served by this launcher's
/// retained daemon candidate. Kept public only so the no-wait law can run in
/// a socket-independent test sandbox.
#[doc(hidden)]
#[must_use]
pub fn authenticated_peer_is_candidate(peer_pid: Option<u32>, candidate_pid: u32) -> bool {
    peer_pid == Some(candidate_pid)
}

enum CandidatePoll {
    Running,
    Exited { code: Option<i32> },
}

trait CandidateProcess: Sized {
    fn id(&self) -> u32;
    fn try_wait(&mut self) -> std::io::Result<CandidatePoll>;
}

impl CandidateProcess for Child {
    fn id(&self) -> u32 {
        Child::id(self)
    }

    fn try_wait(&mut self) -> std::io::Result<CandidatePoll> {
        Child::try_wait(self).map(|status| match status {
            Some(status) => CandidatePoll::Exited {
                code: status.code(),
            },
            None => CandidatePoll::Running,
        })
    }
}

enum ReadyCandidate<C> {
    OwnChild(C),
    CompetingLauncher { exit_code: Option<i32> },
}

/// PID-qualify a retained candidate after an attachable daemon answers.
///
/// Our authenticated child is the daemon we came to launch, so returning its
/// handle must be immediate. Only a child belonging to a launcher that
/// attached to somebody else's winner receives the bounded zombie-reap
/// grace.
async fn qualify_ready_candidate<C>(peer_pid: Option<u32>, mut child: C) -> ReadyCandidate<C>
where
    C: CandidateProcess,
{
    if authenticated_peer_is_candidate(peer_pid, child.id()) {
        return ReadyCandidate::OwnChild(child);
    }

    let mut exit_code = None;
    for _ in 0..RACE_LOSER_REAP_ATTEMPTS {
        match child.try_wait() {
            Ok(CandidatePoll::Exited { code }) => {
                exit_code = code;
                break;
            }
            Ok(CandidatePoll::Running) => sleep(RACE_LOSER_REAP_POLL).await,
            Err(_) => break,
        }
    }
    ReadyCandidate::CompetingLauncher { exit_code }
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
    /// Lifetime policy consumed by higher-level one-shot clients. Daemon
    /// discovery itself never infers ownership from this value.
    pub daemon_lifetime: DaemonLifetime,
}

impl Default for EnsureOptions {
    fn default() -> Self {
        Self {
            required_features: required_live_features(),
            startup_deadline: STARTUP_DEADLINE,
            daemon_binary: None,
            client: ClientConfig::default(),
            daemon_lifetime: DaemonLifetime::Persistent,
        }
    }
}

/// Proof that this launcher owns the authenticated daemon currently serving
/// the endpoint. The retained child handle makes the PID non-reusable until
/// the owner either observes exit or drops the token.
pub struct DaemonOwnershipToken {
    pub(crate) child: Child,
    pub(crate) authenticated_pid: u32,
    pub(crate) instance_id: String,
    pub(crate) daemon_generation: u64,
    pub(crate) _liveness: Option<haider_platform::DaemonLivenessGuard>,
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
    /// Present only when the authenticated endpoint peer PID exactly equals
    /// this launcher's retained child PID.
    pub ownership: Option<DaemonOwnershipToken>,
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
                let _ = connected.client.close();
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
                let _ = connected.client.close();
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
                    let _ = connected.client.close();
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
    let mut log_path = None;
    let mut child: Option<Child> = None;
    let mut readiness: Option<haider_platform::DaemonReadiness> = None;
    let mut liveness: Option<haider_platform::DaemonLivenessGuard> = None;
    let mut readiness_channel_error = None;
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
                let mut ownership = None;
                if let Some(active) = child.take() {
                    // The authenticated endpoint PID distinguishes our
                    // healthy winner from a candidate that lost to another
                    // launcher. Waiting for the healthy child to exit would
                    // add the entire one-second loser grace to every cold
                    // launch even though that daemon is meant to outlive us.
                    match qualify_ready_candidate(peer_credentials.pid, active).await {
                        ReadyCandidate::OwnChild(active) => {
                            ownership = Some(DaemonOwnershipToken {
                                authenticated_pid: active.id(),
                                child: active,
                                instance_id: welcome.instance_id.clone(),
                                daemon_generation: welcome.daemon_generation,
                                _liveness: liveness.take(),
                            });
                        }
                        ReadyCandidate::CompetingLauncher { exit_code } => {
                            race_lost = exit_code == Some(RACE_LOSER_EXIT_CODE);
                        }
                    }
                }
                return Ok(EnsuredDaemon {
                    client,
                    welcome,
                    spawned,
                    race_lost,
                    ownership,
                });
            }
            Attach::Fatal(error) => match *error {
                // After a missing/refused endpoint has authorized exactly one
                // spawn, a stale Unix socket can disappear between connect
                // and Hello and surface EOF instead of another refusal. Keep
                // polling the already-authorized candidate; the same error
                // remains fatal before spawning, so it can never authorize a
                // competitor against an incumbent.
                EnsureError::Connect(ConnectError::ClosedDuringHandshake) if spawned => {
                    last_error = Some(ConnectError::ClosedDuringHandshake);
                }
                error => return Err(error),
            },
            Attach::Spawnable(error) => {
                last_error = Some(error);
                if !spawned {
                    // The one spawn decision (R8 step 5): only a missing or
                    // refused endpoint ever launches a candidate, and only
                    // one candidate per launcher.
                    let candidate = spawn_daemon(profile, &options)?;
                    child = Some(candidate.child);
                    readiness = Some(candidate.readiness);
                    liveness = candidate.liveness;
                    log_path = Some(candidate.log_path);
                    spawned = true;
                } else if let Some(status) = child_status.take() {
                    if status.code() == Some(RACE_LOSER_EXIT_CODE) {
                        child_status = Some(status);
                    } else if !status.success() {
                        return Err(EnsureError::DaemonExited {
                            status,
                            log_path: log_path
                                .unwrap_or_else(|| profile.store_dir.join(DAEMON_LOG_FILE)),
                        });
                    } else {
                        let readiness_log = log_path
                            .as_ref()
                            .cloned()
                            .unwrap_or_else(|| profile.store_dir.join(DAEMON_LOG_FILE));
                        return Err(EnsureError::Spawn {
                            binary: daemon_binary(&options)?,
                            message: format!(
                                "daemon candidate exited without publishing listener readiness \
                                 ({}); see {}",
                                readiness_channel_error
                                    .as_deref()
                                    .unwrap_or("channel closed"),
                                readiness_log.display()
                            ),
                        });
                    }
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
                    return Err(EnsureError::DaemonExited {
                        status,
                        log_path: log_path
                            .unwrap_or_else(|| profile.store_dir.join(DAEMON_LOG_FILE)),
                    });
                }
            }
        }
        if let Some(notification) = readiness.take() {
            // Gate 20 contract: this wait replaces post-spawn polling only.
            // A handshake EOF never authorizes spawning; it remains retryable
            // solely after NotFound/ConnectionRefused authorized this exact
            // candidate. The buffered byte also covers Ready-before-await.
            match tokio::time::timeout_at(deadline, notification.wait()).await {
                Ok(Ok(())) => continue,
                Ok(Err(error)) => {
                    readiness_channel_error = Some(error.to_string());
                    let Some(active) = child.take() else {
                        continue;
                    };
                    match tokio::time::timeout_at(
                        deadline,
                        haider_platform::wait_for_child_exit(active),
                    )
                    .await
                    {
                        Ok(Ok(status)) => {
                            if status.code() == Some(RACE_LOSER_EXIT_CODE) {
                                race_lost = true;
                            }
                            child_status = Some(status);
                        }
                        Ok(Err(wait_error)) => {
                            return Err(EnsureError::Spawn {
                                binary: daemon_binary(&options)?,
                                message: format!(
                                    "cannot observe daemon candidate exit after readiness channel \
                                     failure: {wait_error}"
                                ),
                            });
                        }
                        Err(_) => {
                            return Err(EnsureError::StartupTimeout {
                                last_error,
                                child_status,
                                log_path: log_path
                                    .unwrap_or_else(|| profile.store_dir.join(DAEMON_LOG_FILE)),
                            });
                        }
                    }
                    continue;
                }
                Err(_) => {
                    return Err(EnsureError::StartupTimeout {
                        last_error,
                        child_status,
                        log_path: log_path
                            .unwrap_or_else(|| profile.store_dir.join(DAEMON_LOG_FILE)),
                    });
                }
            }
        }
        if Instant::now() + backoff > deadline {
            return Err(EnsureError::StartupTimeout {
                last_error,
                child_status,
                log_path: log_path.unwrap_or_else(|| profile.store_dir.join(DAEMON_LOG_FILE)),
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

struct SpawnedCandidate {
    child: Child,
    readiness: haider_platform::DaemonReadiness,
    liveness: Option<haider_platform::DaemonLivenessGuard>,
    log_path: PathBuf,
}

fn spawn_daemon(
    profile: &ResolvedProfile,
    options: &EnsureOptions,
) -> Result<SpawnedCandidate, EnsureError> {
    let binary = daemon_binary(options)?;
    let log_path =
        haider_platform::allocate_daemon_log_path(&profile.store_dir).map_err(|error| {
            EnsureError::Spawn {
                binary: binary.clone(),
                message: format!("cannot allocate per-process daemon log: {error}"),
            }
        })?;
    let spec = haider_platform::DaemonSpawn {
        binary: &binary,
        profile_id: &profile.profile_id,
        store_dir: &profile.store_dir,
        runtime_dir: &profile.runtime_dir,
        log_path: &log_path,
    };
    let spawned = match options.daemon_lifetime {
        DaemonLifetime::Persistent => {
            haider_platform::spawn_daemon_with_readiness(spec).map(|spawned| (spawned, None))
        }
        DaemonLifetime::LingerIfSpawned { idle_ttl } => {
            haider_platform::spawn_daemon_with_readiness_and_liveness_and_idle_ttl(spec, idle_ttl)
                .map(|(spawned, liveness)| (spawned, Some(liveness)))
        }
        DaemonLifetime::EphemeralIfSpawned => {
            haider_platform::spawn_daemon_with_readiness_and_liveness(spec)
                .map(|(spawned, liveness)| (spawned, Some(liveness)))
        }
    }
    .map_err(|error| {
        let message = match error {
            haider_platform::DaemonSpawnError::OpenLog(error) => {
                format!("cannot open daemon log {}: {error}", log_path.display())
            }
            haider_platform::DaemonSpawnError::CloneLog(error) => {
                format!("cannot clone daemon log handle: {error}")
            }
            haider_platform::DaemonSpawnError::Readiness(error) => {
                format!("cannot create daemon readiness channel: {error}")
            }
            haider_platform::DaemonSpawnError::Spawn(error) => error.to_string(),
        };
        EnsureError::Spawn { binary, message }
    })?;
    let (spawned, liveness) = spawned;
    Ok(SpawnedCandidate {
        child: spawned.child,
        readiness: spawned.readiness,
        liveness,
        log_path,
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
    let binary = binary.into();
    let log_path =
        haider_platform::allocate_daemon_log_path(&profile.store_dir).map_err(|error| {
            EnsureError::Spawn {
                binary: binary.clone(),
                message: format!("cannot allocate per-process daemon log: {error}"),
            }
        })?;
    haider_platform::spawn_daemon(haider_platform::DaemonSpawn {
        binary: &binary,
        profile_id: &profile.profile_id,
        store_dir: &profile.store_dir,
        runtime_dir: &profile.runtime_dir,
        log_path: &log_path,
    })
    .map_err(|error| {
        let message = match error {
            haider_platform::DaemonSpawnError::OpenLog(error) => {
                format!("cannot open daemon log {}: {error}", log_path.display())
            }
            haider_platform::DaemonSpawnError::CloneLog(error) => {
                format!("cannot clone daemon log handle: {error}")
            }
            haider_platform::DaemonSpawnError::Readiness(error) => {
                format!("cannot create daemon readiness channel: {error}")
            }
            haider_platform::DaemonSpawnError::Spawn(error) => error.to_string(),
        };
        EnsureError::Spawn { binary, message }
    })
}

/// Sends the one graceful termination signal to an authenticated UDS peer.
///
/// MUTATION SAFETY: this changes process state. The function performs one
/// `SIGTERM` syscall and never retries; a runtime failure is returned to the
/// caller so update cannot accidentally turn a timeout into a second signal.
pub fn signal_authenticated_peer(pid: u32) -> std::io::Result<()> {
    haider_platform::signal_process(pid, haider_platform::ProcessSignal::Terminate)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::{
        CandidatePoll, CandidateProcess, RACE_LOSER_EXIT_CODE, RACE_LOSER_REAP_POLL,
        ReadyCandidate, qualify_ready_candidate,
    };
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    struct FakeCandidate {
        id: u32,
        polls: Arc<AtomicUsize>,
        outcomes: VecDeque<CandidatePoll>,
    }

    impl CandidateProcess for FakeCandidate {
        fn id(&self) -> u32 {
            self.id
        }

        fn try_wait(&mut self) -> std::io::Result<CandidatePoll> {
            self.polls.fetch_add(1, Ordering::SeqCst);
            Ok(self.outcomes.pop_front().unwrap_or(CandidatePoll::Running))
        }
    }

    #[tokio::test(start_paused = true)]
    async fn own_healthy_child_skips_the_one_second_loser_grace() {
        let polls = Arc::new(AtomicUsize::new(0));
        let child = FakeCandidate {
            id: 41,
            polls: Arc::clone(&polls),
            outcomes: VecDeque::new(),
        };
        let started = tokio::time::Instant::now();
        let decision = qualify_ready_candidate(Some(41), child).await;

        let ReadyCandidate::OwnChild(_) = decision else {
            panic!("authenticated child must remain owned");
        };
        assert_eq!(started.elapsed(), Duration::ZERO);
        assert_eq!(polls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn genuine_lost_race_keeps_the_loser_reap_wait() {
        let polls = Arc::new(AtomicUsize::new(0));
        let child = FakeCandidate {
            id: 41,
            polls: Arc::clone(&polls),
            outcomes: VecDeque::from([
                CandidatePoll::Running,
                CandidatePoll::Running,
                CandidatePoll::Exited {
                    code: Some(RACE_LOSER_EXIT_CODE),
                },
            ]),
        };
        let started = tokio::time::Instant::now();
        let decision = qualify_ready_candidate(Some(42), child).await;

        let ReadyCandidate::CompetingLauncher { exit_code } = decision else {
            panic!("competing launcher must reap its losing child");
        };
        assert_eq!(exit_code, Some(RACE_LOSER_EXIT_CODE));
        assert_eq!(started.elapsed(), RACE_LOSER_REAP_POLL * 2);
        assert_eq!(polls.load(Ordering::SeqCst), 3);
    }
}
