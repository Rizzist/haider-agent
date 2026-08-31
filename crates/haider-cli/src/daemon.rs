//! Scriptable daemon lifecycle controls.

use std::fs::{OpenOptions, TryLockError};
use std::io::{self, Write as _};
use std::process::ExitCode;
use std::time::Duration;

#[cfg(unix)]
use haider_client::effective_uid;
use haider_client::{
    ClientConfig, ConnectError, ConnectionUsage, DAEMON_STOP_CLIENT_NAME,
    DAEMON_STOP_COMPLETION_SCHEMA, DaemonStopCompletion, DaemonStopReceipt, ProfileEnv,
    ResolvedProfile, connect, daemon_stop_receipt_path, resolve_profile_read_only,
};
use haider_rpc::{LifecyclePhase, RequestBody, ResponseBody, WireFrame};
use serde::Serialize;
use tokio::sync::mpsc;
use tokio::time::{Instant, sleep_until, timeout_at};

use super::run::{EX_IOERR, EX_PROTOCOL, EX_SOFTWARE, EX_TIMEOUT, EX_UNAVAILABLE, EX_USAGE};

const DEFAULT_STOP_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_STOP_TIMEOUT: Duration = Duration::from_secs(60 * 60);
const LOCK_POLL: Duration = Duration::from_millis(25);
const STOP_SCHEMA: &str = "haider.daemon-stop.v1";
const STOP_HELP: &str = "usage: haider daemon stop [--json] [--timeout <duration>]\n\
durations: integer followed by ms, s, or m (default 20s)\n\
environment: HAIDER_RUNTIME_DIR overrides the per-user runtime root; on Unix, \
XDG_RUNTIME_DIR is used next when it is owner-private. Overlong Unix socket \
paths below an explicit root are errors; derived roots may fall back to the \
short per-user runtime.\n";

#[derive(Debug, Clone, Copy)]
struct StopOptions {
    json: bool,
    timeout: Duration,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum StopOutcome {
    StoppedCleanly,
    NotRunning,
    DidNotStop,
}

#[derive(Debug, Serialize)]
struct StopReport {
    schema: &'static str,
    outcome: StopOutcome,
    elapsed_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    daemon: Option<StoppedDaemon>,
    #[serde(skip_serializing_if = "Option::is_none")]
    phase: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

#[derive(Debug, Serialize)]
struct StoppedDaemon {
    instance_id: String,
    generation: u64,
    pid: Option<u32>,
    shutdown_acknowledged: bool,
    drain_deadline_unix_ms: u64,
    completion: DaemonStopCompletion,
    process_exited: bool,
}

struct StopIdentity {
    instance_id: String,
    generation: u64,
    pid: Option<u32>,
}

struct StopCompletionReport<'a> {
    outcome: StopOutcome,
    started: Instant,
    identity: &'a StopIdentity,
    acknowledged: bool,
    drain_deadline_unix_ms: u64,
    completion: DaemonStopCompletion,
    process_exited: bool,
    phase: Option<&'static str>,
    reason: Option<String>,
}

#[derive(Debug)]
struct StopFailure {
    code: u8,
    message: String,
}

impl StopFailure {
    fn io(message: impl Into<String>) -> Self {
        Self {
            code: EX_IOERR,
            message: message.into(),
        }
    }

    fn protocol(message: impl Into<String>) -> Self {
        Self {
            code: EX_PROTOCOL,
            message: message.into(),
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self {
            code: EX_SOFTWARE,
            message: message.into(),
        }
    }
}

pub(crate) async fn daemon_command(rest: &[String]) -> ExitCode {
    let options = match parse_options(rest) {
        Ok(Some(options)) => options,
        Ok(None) => return write_help(),
        Err(message) => {
            eprintln!("haider daemon: {message}");
            return ExitCode::from(EX_USAGE);
        }
    };
    let profile = match resolve_profile_read_only(&ProfileEnv::capture()) {
        Ok(profile) => profile,
        Err(error) => {
            eprintln!("haider daemon: {error}");
            return ExitCode::from(EX_SOFTWARE);
        }
    };
    match stop_daemon(&profile, options.timeout).await {
        Ok(report) => {
            let code = match report.outcome {
                StopOutcome::StoppedCleanly => 0,
                StopOutcome::NotRunning => EX_UNAVAILABLE,
                StopOutcome::DidNotStop => EX_TIMEOUT,
            };
            match write_report(&report, options.json) {
                Ok(()) => ExitCode::from(code),
                Err(error) if error.kind() == io::ErrorKind::BrokenPipe => ExitCode::from(code),
                Err(error) => {
                    eprintln!("haider daemon: stdout failed: {error}");
                    ExitCode::from(EX_IOERR)
                }
            }
        }
        Err(error) => {
            eprintln!("haider daemon: {}", error.message);
            ExitCode::from(error.code)
        }
    }
}

fn parse_options(rest: &[String]) -> Result<Option<StopOptions>, String> {
    if matches!(rest, [flag] if matches!(flag.as_str(), "--help" | "-h"))
        || matches!(rest, [command, flag] if command == "stop" && matches!(flag.as_str(), "--help" | "-h"))
    {
        return Ok(None);
    }
    let Some((command, flags)) = rest.split_first() else {
        return Err("missing subcommand; expected `stop`".into());
    };
    if command != "stop" {
        return Err(format!("unknown subcommand `{command}`; expected `stop`"));
    }
    let mut json = false;
    let mut timeout = None;
    let mut index = 0;
    while index < flags.len() {
        match flags[index].as_str() {
            "--json" if !json => json = true,
            "--json" => return Err("duplicate --json flag".into()),
            "--timeout" if timeout.is_none() => {
                index += 1;
                let value = flags
                    .get(index)
                    .ok_or_else(|| "--timeout requires a duration".to_owned())?;
                timeout = Some(parse_timeout(value)?);
            }
            "--timeout" => return Err("duplicate --timeout flag".into()),
            flag => return Err(format!("unknown flag `{flag}`")),
        }
        index += 1;
    }
    Ok(Some(StopOptions {
        json,
        timeout: timeout.unwrap_or(DEFAULT_STOP_TIMEOUT),
    }))
}

fn parse_timeout(value: &str) -> Result<Duration, String> {
    let (digits, multiplier) = if let Some(digits) = value.strip_suffix("ms") {
        (digits, 1_u64)
    } else if let Some(digits) = value.strip_suffix('s') {
        (digits, 1_000)
    } else if let Some(digits) = value.strip_suffix('m') {
        (digits, 60_000)
    } else {
        return Err("--timeout requires an integer followed by ms, s, or m".into());
    };
    let amount = digits
        .parse::<u64>()
        .map_err(|_| "--timeout requires a positive integer duration".to_owned())?;
    let millis = amount
        .checked_mul(multiplier)
        .ok_or_else(|| "--timeout is too large".to_owned())?;
    let timeout = Duration::from_millis(millis);
    if timeout.is_zero() {
        return Err("--timeout must be greater than zero".into());
    }
    if timeout > MAX_STOP_TIMEOUT {
        return Err("--timeout must not exceed 60m".into());
    }
    Ok(timeout)
}

async fn stop_daemon(
    profile: &ResolvedProfile,
    timeout: Duration,
) -> Result<StopReport, StopFailure> {
    let started = Instant::now();
    let deadline = started + timeout;
    let connected = loop {
        let mut config = ClientConfig {
            client_name: DAEMON_STOP_CLIENT_NAME.into(),
            client_instance_id: format!("daemon-stop-{}", std::process::id()),
            connection_usage: ConnectionUsage::LongLived,
            ..ClientConfig::default()
        };
        config.request_timeout = remaining(deadline).unwrap_or(Duration::from_millis(1));
        match timeout_at(deadline, connect(&profile.endpoint_path, config)).await {
            Ok(Ok(connected)) => break connected,
            Ok(Err(ConnectError::NotFound(_) | ConnectError::Refused(_))) => {
                if !profile.store_dir.exists() {
                    return Ok(terminal_report(
                        StopOutcome::NotRunning,
                        started,
                        None,
                        None,
                    ));
                }
                if !profile_lock_held(&profile.store_dir)? {
                    return Ok(terminal_report(
                        StopOutcome::NotRunning,
                        started,
                        None,
                        None,
                    ));
                }
                if !wait_to_retry(deadline).await {
                    return Ok(terminal_report(
                        StopOutcome::DidNotStop,
                        started,
                        Some("connect"),
                        Some(
                            "profile lock remained held while the daemon endpoint was unavailable"
                                .into(),
                        ),
                    ));
                }
            }
            Ok(Err(error)) => {
                return Err(StopFailure::io(format!(
                    "cannot authenticate the current-profile daemon: {error}"
                )));
            }
            Err(_) => {
                return Ok(terminal_report(
                    StopOutcome::DidNotStop,
                    started,
                    Some("connect"),
                    Some("daemon connection did not complete before the caller deadline".into()),
                ));
            }
        }
    };

    if !connected.welcome.profile_id.is_empty()
        && connected.welcome.profile_id != profile.profile_id
    {
        let _ = connected.client.close();
        return Err(StopFailure::protocol(format!(
            "daemon serves profile {}, expected {}",
            connected.welcome.profile_id, profile.profile_id
        )));
    }
    #[cfg(unix)]
    if connected.peer_credentials.uid != effective_uid() {
        let _ = connected.client.close();
        return Err(StopFailure::protocol(
            "daemon socket peer is not owned by this user",
        ));
    }
    let identity = StopIdentity {
        instance_id: connected.welcome.instance_id.clone(),
        generation: connected.welcome.daemon_generation,
        pid: connected.peer_credentials.pid,
    };
    let pid = identity.pid.ok_or_else(|| {
        StopFailure::protocol("daemon peer did not expose a process identity for exit confirmation")
    })?;
    let process_id = haider_platform::process_id(Some(pid))
        .ok_or_else(|| StopFailure::protocol("daemon peer exposed an invalid process identity"))?;
    let process_exit =
        haider_platform::ProcessExitMonitor::capture(process_id).map_err(|error| {
            StopFailure::io(format!(
                "cannot retain daemon process identity for exit confirmation: {error}"
            ))
        })?;
    let receipt_path = daemon_stop_receipt_path(
        &profile.store_dir,
        identity.generation,
        &identity.instance_id,
    )
    .ok_or_else(|| StopFailure::protocol("daemon exposed an invalid instance identity"))?;
    let mut events = connected.client.take_events().ok_or_else(|| {
        StopFailure::internal("daemon stop could not retain the lifecycle event stream")
    })?;
    let mut acknowledged = false;
    if connected.welcome.lifecycle_phase == LifecyclePhase::Ready {
        match timeout_at(
            deadline,
            connected.client.request(RequestBody::DaemonShutdown {}),
        )
        .await
        {
            Ok(Ok(ResponseBody::DaemonShutdown {})) => acknowledged = true,
            Ok(Ok(ResponseBody::Error { code, message, .. })) => {
                let _ = connected.client.close();
                return Ok(terminal_report(
                    StopOutcome::DidNotStop,
                    started,
                    Some("shutdown_ack"),
                    Some(format!("daemon refused graceful stop ({code}): {message}")),
                ));
            }
            Ok(Ok(_)) => {
                let _ = connected.client.close();
                return Err(StopFailure::protocol(
                    "daemon returned the wrong shutdown response method",
                ));
            }
            Ok(Err(_)) => {}
            Err(_) => {
                return Ok(terminal_report(
                    StopOutcome::DidNotStop,
                    started,
                    Some("shutdown_ack"),
                    Some("graceful stop acknowledgement exceeded the caller deadline".into()),
                ));
            }
        }
    } else if connected.welcome.lifecycle_phase != LifecyclePhase::Draining {
        let phase = lifecycle_name(connected.welcome.lifecycle_phase);
        let _ = connected.client.close();
        return Ok(terminal_report(
            StopOutcome::DidNotStop,
            started,
            Some("lifecycle"),
            Some(format!(
                "daemon lifecycle phase `{phase}` cannot accept a stop request"
            )),
        ));
    }

    let drain_deadline_unix_ms = match matching_drain_notice(&identity, &mut events, deadline).await
    {
        Ok(value) => value,
        Err(reason) => {
            return Ok(terminal_report(
                StopOutcome::DidNotStop,
                started,
                Some("drain_notice"),
                Some(reason),
            ));
        }
    };
    if timeout_at(deadline, connected.client.disconnected())
        .await
        .is_err()
    {
        let _ = connected.client.close();
        return Ok(terminal_report(
            StopOutcome::DidNotStop,
            started,
            Some("disconnect"),
            Some("daemon did not disconnect before the caller deadline".into()),
        ));
    }
    let receipt = match wait_for_stop_receipt(&receipt_path, &identity, pid, deadline).await {
        Ok(receipt) => receipt,
        Err(reason) => {
            return Ok(terminal_report(
                StopOutcome::DidNotStop,
                started,
                Some("completion_receipt"),
                Some(reason),
            ));
        }
    };
    if let Some(reason) = wait_for_profile_lock(profile, deadline).await? {
        return Ok(terminal_report(
            StopOutcome::DidNotStop,
            started,
            Some("profile_lock_release"),
            Some(reason),
        ));
    }

    let process_exited = match timeout_at(deadline, process_exit.wait()).await {
        Ok(Ok(())) => true,
        Ok(Err(error)) => {
            return Ok(stop_completion_report(StopCompletionReport {
                outcome: StopOutcome::DidNotStop,
                started,
                identity: &identity,
                acknowledged,
                drain_deadline_unix_ms,
                completion: receipt.completion,
                process_exited: false,
                phase: Some("process_exit"),
                reason: Some(format!("daemon process-exit confirmation failed: {error}")),
            }));
        }
        Err(_) => {
            return Ok(stop_completion_report(StopCompletionReport {
                outcome: StopOutcome::DidNotStop,
                started,
                identity: &identity,
                acknowledged,
                drain_deadline_unix_ms,
                completion: receipt.completion,
                process_exited: false,
                phase: Some("process_exit"),
                reason: Some("daemon process remained alive past the caller deadline".into()),
            }));
        }
    };
    let _ = std::fs::remove_file(&receipt_path);

    let (outcome, phase, reason) = match receipt.completion {
        DaemonStopCompletion::Graceful => (StopOutcome::StoppedCleanly, None, None),
        DaemonStopCompletion::Forced => (
            StopOutcome::DidNotStop,
            Some("completion"),
            Some("daemon reported forced shutdown completion".into()),
        ),
        DaemonStopCompletion::Failed => (
            StopOutcome::DidNotStop,
            Some("completion"),
            Some("daemon reported failed shutdown completion".into()),
        ),
    };
    Ok(stop_completion_report(StopCompletionReport {
        outcome,
        started,
        identity: &identity,
        acknowledged,
        drain_deadline_unix_ms,
        completion: receipt.completion,
        process_exited,
        phase,
        reason,
    }))
}

async fn wait_for_stop_receipt(
    path: &std::path::Path,
    identity: &StopIdentity,
    pid: u32,
    deadline: Instant,
) -> Result<DaemonStopReceipt, String> {
    loop {
        match std::fs::read(path) {
            Ok(bytes) => {
                let receipt = serde_json::from_slice::<DaemonStopReceipt>(&bytes)
                    .map_err(|error| format!("daemon stop receipt is invalid: {error}"))?;
                if receipt.schema != DAEMON_STOP_COMPLETION_SCHEMA
                    || receipt.instance_id != identity.instance_id
                    || receipt.generation != identity.generation
                    || receipt.pid != pid
                {
                    return Err(
                        "daemon stop receipt does not match the authenticated process".into(),
                    );
                }
                return Ok(receipt);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if !wait_to_retry(deadline).await {
                    return Err(
                        "daemon did not publish a completion receipt before the caller deadline"
                            .into(),
                    );
                }
            }
            Err(error) => return Err(format!("cannot read daemon stop receipt: {error}")),
        }
    }
}

async fn matching_drain_notice(
    identity: &StopIdentity,
    events: &mut mpsc::Receiver<WireFrame>,
    deadline: Instant,
) -> Result<u64, String> {
    match timeout_at(deadline, async {
        while let Some(frame) = events.recv().await {
            if let WireFrame::ServerDraining {
                instance_id,
                daemon_generation,
                deadline_unix_ms,
                ..
            } = frame
            {
                return Some((instance_id, daemon_generation, deadline_unix_ms));
            }
        }
        None
    })
    .await
    {
        Ok(Some((instance_id, generation, deadline_unix_ms)))
            if instance_id == identity.instance_id && generation == identity.generation =>
        {
            Ok(deadline_unix_ms)
        }
        Ok(Some(_)) => Err("daemon drain notice did not match the authenticated instance".into()),
        Ok(None) => Err("daemon disconnected without a matching drain notice".into()),
        Err(_) => Err("daemon drain notice exceeded the caller deadline".into()),
    }
}

async fn wait_for_profile_lock(
    profile: &ResolvedProfile,
    deadline: Instant,
) -> Result<Option<String>, StopFailure> {
    loop {
        if !profile_lock_held(&profile.store_dir)? {
            return Ok(None);
        }
        if !wait_to_retry(deadline).await {
            return Ok(Some(
                "daemon disconnected but did not release the profile lock before the caller deadline"
                    .into(),
            ));
        }
    }
}

fn profile_lock_held(store_dir: &std::path::Path) -> Result<bool, StopFailure> {
    let lock_path = store_dir.join("lock");
    let file = match OpenOptions::new().read(true).write(true).open(&lock_path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(StopFailure::io(format!(
                "cannot open existing profile lock {}: {error}",
                lock_path.display()
            )));
        }
    };
    match file.try_lock() {
        Ok(()) => {
            file.unlock().map_err(|error| {
                StopFailure::io(format!(
                    "cannot release profile lock probe {}: {error}",
                    lock_path.display()
                ))
            })?;
            Ok(false)
        }
        Err(TryLockError::WouldBlock) => Ok(true),
        Err(TryLockError::Error(error)) => Err(StopFailure::io(format!(
            "cannot inspect profile lock {}: {error}",
            lock_path.display()
        ))),
    }
}

async fn wait_to_retry(deadline: Instant) -> bool {
    let Some(remaining) = remaining(deadline) else {
        return false;
    };
    sleep_until(Instant::now() + remaining.min(LOCK_POLL)).await;
    Instant::now() < deadline
}

fn remaining(deadline: Instant) -> Option<Duration> {
    deadline.checked_duration_since(Instant::now())
}

fn terminal_report(
    outcome: StopOutcome,
    started: Instant,
    phase: Option<&'static str>,
    reason: Option<String>,
) -> StopReport {
    StopReport {
        schema: STOP_SCHEMA,
        outcome,
        elapsed_ms: elapsed_ms(started),
        daemon: None,
        phase: phase.map(str::to_owned),
        reason,
    }
}

fn stop_completion_report(report: StopCompletionReport<'_>) -> StopReport {
    StopReport {
        schema: STOP_SCHEMA,
        outcome: report.outcome,
        elapsed_ms: elapsed_ms(report.started),
        daemon: Some(StoppedDaemon {
            instance_id: report.identity.instance_id.clone(),
            generation: report.identity.generation,
            pid: report.identity.pid,
            shutdown_acknowledged: report.acknowledged,
            drain_deadline_unix_ms: report.drain_deadline_unix_ms,
            completion: report.completion,
            process_exited: report.process_exited,
        }),
        phase: report.phase.map(str::to_owned),
        reason: report.reason,
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn lifecycle_name(phase: LifecyclePhase) -> &'static str {
    match phase {
        LifecyclePhase::Starting => "starting",
        LifecyclePhase::Recovering => "recovering",
        LifecyclePhase::Ready => "ready",
        LifecyclePhase::Draining => "draining",
        LifecyclePhase::Finalizing => "finalizing",
        LifecyclePhase::Stopped => "stopped",
        LifecyclePhase::Failed => "failed",
        LifecyclePhase::Unknown => "unknown",
        _ => "unknown",
    }
}

fn write_report(report: &StopReport, json: bool) -> io::Result<()> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    if json {
        serde_json::to_writer(&mut output, report).map_err(io::Error::other)?;
        output.write_all(b"\n")?;
    } else {
        match report.outcome {
            StopOutcome::StoppedCleanly => {
                writeln!(output, "daemon stopped cleanly in {} ms", report.elapsed_ms)?
            }
            StopOutcome::NotRunning => writeln!(output, "daemon was not running")?,
            StopOutcome::DidNotStop => writeln!(
                output,
                "daemon did not stop in {} ms ({}: {})",
                report.elapsed_ms,
                report.phase.as_deref().unwrap_or("unknown"),
                report.reason.as_deref().unwrap_or("no reason reported")
            )?,
        }
    }
    output.flush()
}

fn write_help() -> ExitCode {
    let mut output = io::stdout().lock();
    match output
        .write_all(STOP_HELP.as_bytes())
        .and_then(|()| output.flush())
    {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("haider daemon: stdout failed: {error}");
            ExitCode::from(EX_IOERR)
        }
    }
}
