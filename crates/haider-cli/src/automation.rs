//! Finite, machine-readable session barriers for external orchestrators.

use std::collections::{BTreeMap, HashSet};
use std::io::{self, Write};
use std::process::ExitCode;
use std::time::Duration;

use haider_client::{
    ObserveError, ProfileEnv, SessionReadinessSnapshot, SessionResumeSnapshot, resolve_profile,
    wait_for_session_resume, wait_for_sessions_ready,
};
use haider_protocol::ids::SessionId;
use haider_rpc::{ObserveRunStateWire, SessionSummary};
use serde::Serialize;

use super::run::{EX_IOERR, EX_PROTOCOL, EX_SOFTWARE, EX_TIMEOUT, EX_UNAVAILABLE, EX_USAGE};

const DEFAULT_WAIT_TIMEOUT: Duration = Duration::from_secs(30);
const SESSION_READY_SCHEMA: &str = "haider.sessions.ready.v1";
const SESSION_RESUME_SCHEMA: &str = "haider.session.resume.v1";

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReadyOptions {
    count: usize,
    sessions: Vec<SessionId>,
    timeout: Duration,
    no_spawn: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResumeOptions {
    session_id: SessionId,
    timeout: Duration,
    no_spawn: bool,
}

#[derive(Serialize)]
struct MachineError {
    code: &'static str,
    message: String,
    retryable: bool,
}

#[derive(Serialize)]
struct SessionReadyDocument<'a> {
    schema: &'static str,
    ready: bool,
    timed_out: bool,
    daemon_ready: bool,
    daemon_generation: u64,
    expected_count: usize,
    ready_count: usize,
    total_session_count: usize,
    expected_session_ids: Vec<&'a str>,
    ready_session_ids: Vec<&'a str>,
    state_counts: BTreeMap<&'static str, usize>,
    sessions: &'a [SessionSummary],
    error: Option<MachineError>,
}

#[derive(Serialize)]
struct SessionResumeDocument<'a> {
    schema: &'static str,
    session_id: &'a str,
    completed: bool,
    timed_out: bool,
    daemon_ready: bool,
    daemon_generation: u64,
    outcome: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    head_seq: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    worker_generation: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    run_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    run_state: Option<&'static str>,
    error: Option<MachineError>,
}

pub(crate) async fn sessions_wait_ready_command(rest: &[String]) -> ExitCode {
    let options = match parse_ready_options(rest) {
        Ok(options) => options,
        Err(message) => return ready_usage_failure(message),
    };
    let profile = match resolve_profile(&ProfileEnv::capture()) {
        Ok(profile) => profile,
        Err(error) => {
            let written = ready_runtime_failure("protocol_mismatch", error.to_string(), false);
            return output_or(written, ExitCode::from(EX_PROTOCOL));
        }
    };
    match wait_for_sessions_ready(
        &profile,
        !options.no_spawn,
        options.count,
        &options.sessions,
        options.timeout,
    )
    .await
    {
        Ok(snapshot) => write_ready_result(&options.sessions, &snapshot),
        Err(error) => observe_ready_failure(error),
    }
}

pub(crate) async fn resume_command(rest: &[String]) -> ExitCode {
    let options = match parse_resume_options(rest) {
        Ok(options) => options,
        Err(message) => return resume_usage_failure(rest.first().map(String::as_str), message),
    };
    let profile = match resolve_profile(&ProfileEnv::capture()) {
        Ok(profile) => profile,
        Err(error) => {
            let written = resume_runtime_failure(
                options.session_id.as_str(),
                "protocol_mismatch",
                error.to_string(),
                false,
            );
            return output_or(written, ExitCode::from(EX_PROTOCOL));
        }
    };
    match wait_for_session_resume(
        &profile,
        !options.no_spawn,
        options.session_id.clone(),
        options.timeout,
    )
    .await
    {
        Ok(snapshot) => write_resume_result(&options.session_id, &snapshot),
        Err(error) => observe_resume_failure(&options.session_id, error),
    }
}

fn parse_ready_options(rest: &[String]) -> Result<ReadyOptions, String> {
    let mut count = None;
    let mut sessions = Vec::new();
    let mut timeout = None;
    let mut no_spawn = false;
    let mut json = false;
    let mut index = 0;
    while index < rest.len() {
        match rest[index].as_str() {
            "--count" if count.is_none() => {
                index += 1;
                let value = rest
                    .get(index)
                    .ok_or_else(|| "--count requires a non-negative integer".to_owned())?;
                count = Some(
                    value
                        .parse::<usize>()
                        .map_err(|_| "--count requires a non-negative integer".to_owned())?,
                );
            }
            "--count" => return Err("duplicate --count flag".into()),
            "--session" => {
                index += 1;
                let value = rest
                    .get(index)
                    .filter(|value| !value.is_empty() && !value.starts_with('-'))
                    .ok_or_else(|| "--session requires a session id".to_owned())?;
                sessions.push(SessionId::new(value.clone()));
            }
            "--timeout" if timeout.is_none() => {
                index += 1;
                timeout = Some(super::run::parse_timeout(
                    rest.get(index)
                        .ok_or_else(|| "--timeout requires a duration".to_owned())?,
                )?);
            }
            "--timeout" => return Err("duplicate --timeout flag".into()),
            "--no-spawn" if !no_spawn => no_spawn = true,
            "--no-spawn" => return Err("duplicate --no-spawn flag".into()),
            "--json" if !json => json = true,
            "--json" => return Err("duplicate --json flag".into()),
            flag => return Err(format!("unknown flag `{flag}`")),
        }
        index += 1;
    }
    if !json {
        return Err("--json is required for the headless readiness surface".into());
    }
    let count = count.ok_or_else(|| "--count is required".to_owned())?;
    let unique = sessions
        .iter()
        .map(|session_id| session_id.as_str())
        .collect::<HashSet<_>>();
    if unique.len() != sessions.len() {
        return Err("--session ids must be unique".into());
    }
    if !sessions.is_empty() && sessions.len() != count {
        return Err("the number of --session ids must equal --count".into());
    }
    Ok(ReadyOptions {
        count,
        sessions,
        timeout: timeout.unwrap_or(DEFAULT_WAIT_TIMEOUT),
        no_spawn,
    })
}

fn parse_resume_options(rest: &[String]) -> Result<ResumeOptions, String> {
    let Some((session_id, flags)) = rest.split_first() else {
        return Err("a session id is required".into());
    };
    if session_id.is_empty() || session_id.starts_with('-') {
        return Err("a session id is required before flags".into());
    }
    let mut timeout = None;
    let mut no_spawn = false;
    let mut json = false;
    let mut index = 0;
    while index < flags.len() {
        match flags[index].as_str() {
            "--timeout" if timeout.is_none() => {
                index += 1;
                timeout =
                    Some(super::run::parse_timeout(flags.get(index).ok_or_else(
                        || "--timeout requires a duration".to_owned(),
                    )?)?);
            }
            "--timeout" => return Err("duplicate --timeout flag".into()),
            "--no-spawn" if !no_spawn => no_spawn = true,
            "--no-spawn" => return Err("duplicate --no-spawn flag".into()),
            "--json" if !json => json = true,
            "--json" => return Err("duplicate --json flag".into()),
            flag => return Err(format!("unknown flag `{flag}`")),
        }
        index += 1;
    }
    if !json {
        return Err("--json is required for headless resume".into());
    }
    Ok(ResumeOptions {
        session_id: SessionId::new(session_id.clone()),
        timeout: timeout.unwrap_or(DEFAULT_WAIT_TIMEOUT),
        no_spawn,
    })
}

fn write_ready_result(
    expected_sessions: &[SessionId],
    snapshot: &SessionReadinessSnapshot,
) -> ExitCode {
    let expected_ids = expected_sessions
        .iter()
        .map(SessionId::as_str)
        .collect::<HashSet<_>>();
    let ready_session_ids = snapshot
        .summaries
        .iter()
        .filter(|summary| {
            summary.head_seq > 0
                && summary.metadata.is_some()
                && summary.run_state.is_some()
                && (expected_ids.is_empty() || expected_ids.contains(summary.session_id.as_str()))
        })
        .map(|summary| summary.session_id.as_str())
        .collect();
    let document = SessionReadyDocument {
        schema: SESSION_READY_SCHEMA,
        ready: !snapshot.timed_out,
        timed_out: snapshot.timed_out,
        daemon_ready: snapshot.daemon_ready,
        daemon_generation: snapshot.daemon_generation,
        expected_count: snapshot.expected_count,
        ready_count: snapshot.ready_count,
        total_session_count: snapshot.total_session_count,
        expected_session_ids: expected_sessions.iter().map(SessionId::as_str).collect(),
        ready_session_ids,
        state_counts: state_counts(&snapshot.summaries),
        sessions: &snapshot.summaries,
        error: snapshot.timed_out.then(|| MachineError {
            code: "timeout",
            message: "session-readiness deadline elapsed".into(),
            retryable: true,
        }),
    };
    write_document(&document, snapshot.timed_out)
}

fn write_resume_result(session_id: &SessionId, snapshot: &SessionResumeSnapshot) -> ExitCode {
    let summary = snapshot.summary.as_ref();
    let state = summary.and_then(|summary| summary.run_state);
    let document = SessionResumeDocument {
        schema: SESSION_RESUME_SCHEMA,
        session_id: session_id.as_str(),
        completed: !snapshot.timed_out,
        timed_out: snapshot.timed_out,
        daemon_ready: snapshot.daemon_ready,
        daemon_generation: snapshot.daemon_generation,
        outcome: if snapshot.timed_out {
            "timeout"
        } else {
            resume_outcome(state)
        },
        head_seq: summary.map(|summary| summary.head_seq),
        worker_generation: summary.map(|summary| summary.worker_generation),
        run_id: summary.and_then(|summary| summary.run_id.as_ref().map(|run_id| run_id.as_str())),
        run_state: state.map(run_state_name),
        error: snapshot.timed_out.then(|| MachineError {
            code: if summary.is_some() {
                "timeout"
            } else {
                "not_found"
            },
            message: if summary.is_some() {
                "session remained running until the resume deadline".into()
            } else {
                "session was not roster-visible before the resume deadline".into()
            },
            retryable: true,
        }),
    };
    write_document(&document, snapshot.timed_out)
}

fn state_counts(summaries: &[SessionSummary]) -> BTreeMap<&'static str, usize> {
    let mut counts = BTreeMap::new();
    for summary in summaries {
        let state = summary.run_state.map_or("unknown", run_state_name);
        *counts.entry(state).or_insert(0) += 1;
    }
    counts
}

fn run_state_name(state: ObserveRunStateWire) -> &'static str {
    match state {
        ObserveRunStateWire::Idle => "idle",
        ObserveRunStateWire::Running => "running",
        ObserveRunStateWire::WaitingForRoute => "waiting_for_route",
        ObserveRunStateWire::EffectUnknown => "effect_unknown",
        ObserveRunStateWire::ParkedPermission => "parked_permission",
        ObserveRunStateWire::ParkedInput => "parked_input",
        ObserveRunStateWire::Errored => "errored",
        ObserveRunStateWire::Cancelled => "cancelled",
        ObserveRunStateWire::Unknown => "unknown",
        _ => "unknown",
    }
}

fn resume_outcome(state: Option<ObserveRunStateWire>) -> &'static str {
    match state {
        Some(ObserveRunStateWire::Idle) => "idle",
        Some(ObserveRunStateWire::EffectUnknown) => "recovery_required",
        Some(ObserveRunStateWire::ParkedPermission | ObserveRunStateWire::ParkedInput) => {
            "input_required"
        }
        Some(ObserveRunStateWire::Errored) => "errored",
        Some(ObserveRunStateWire::Cancelled) => "cancelled",
        Some(
            ObserveRunStateWire::Running
            | ObserveRunStateWire::WaitingForRoute
            | ObserveRunStateWire::Unknown,
        )
        | None => "unknown",
        _ => "unknown",
    }
}

fn observe_ready_failure(error: ObserveError) -> ExitCode {
    let (code, retryable, exit) = classify_observe_error(&error);
    let written = ready_runtime_failure(code, error.to_string(), retryable);
    output_or(written, exit)
}

fn observe_resume_failure(session_id: &SessionId, error: ObserveError) -> ExitCode {
    let (code, retryable, exit) = classify_observe_error(&error);
    let written = resume_runtime_failure(session_id.as_str(), code, error.to_string(), retryable);
    if written == ExitCode::from(EX_IOERR) {
        written
    } else {
        exit
    }
}

fn classify_observe_error(error: &ObserveError) -> (&'static str, bool, ExitCode) {
    match error {
        ObserveError::NoDaemon(_) | ObserveError::Connect(_) | ObserveError::Ensure(_) => {
            ("unavailable", true, ExitCode::from(EX_UNAVAILABLE))
        }
        ObserveError::UnknownSession(_) => ("not_found", false, ExitCode::from(EX_SOFTWARE)),
        ObserveError::Rpc {
            retryable, code, ..
        } if code == "not_found" => ("not_found", *retryable, ExitCode::from(EX_SOFTWARE)),
        ObserveError::Rpc { retryable, .. } => {
            ("rpc_rejected", *retryable, ExitCode::from(EX_SOFTWARE))
        }
        ObserveError::ProfileMismatch { .. }
        | ObserveError::NotReady(_)
        | ObserveError::MissingFeature(_)
        | ObserveError::Protocol(_) => ("protocol_mismatch", false, ExitCode::from(EX_PROTOCOL)),
        ObserveError::Client(_) | ObserveError::OutputClosed | ObserveError::StreamTask(_) => {
            ("unavailable", true, ExitCode::from(EX_UNAVAILABLE))
        }
    }
}

fn ready_usage_failure(message: String) -> ExitCode {
    let written = ready_runtime_failure("invalid_argument", message, false);
    output_or(written, ExitCode::from(EX_USAGE))
}

fn resume_usage_failure(session_id: Option<&str>, message: String) -> ExitCode {
    let written =
        resume_runtime_failure(session_id.unwrap_or(""), "invalid_argument", message, false);
    output_or(written, ExitCode::from(EX_USAGE))
}

fn output_or(written: ExitCode, intended: ExitCode) -> ExitCode {
    if written == ExitCode::from(EX_IOERR) {
        written
    } else {
        intended
    }
}

fn ready_runtime_failure(code: &'static str, message: String, retryable: bool) -> ExitCode {
    let document = SessionReadyDocument {
        schema: SESSION_READY_SCHEMA,
        ready: false,
        timed_out: false,
        daemon_ready: false,
        daemon_generation: 0,
        expected_count: 0,
        ready_count: 0,
        total_session_count: 0,
        expected_session_ids: Vec::new(),
        ready_session_ids: Vec::new(),
        state_counts: BTreeMap::new(),
        sessions: &[],
        error: Some(MachineError {
            code,
            message,
            retryable,
        }),
    };
    write_document(&document, false)
}

fn resume_runtime_failure(
    session_id: &str,
    code: &'static str,
    message: String,
    retryable: bool,
) -> ExitCode {
    let document = SessionResumeDocument {
        schema: SESSION_RESUME_SCHEMA,
        session_id,
        completed: false,
        timed_out: false,
        daemon_ready: false,
        daemon_generation: 0,
        outcome: "error",
        head_seq: None,
        worker_generation: None,
        run_id: None,
        run_state: None,
        error: Some(MachineError {
            code,
            message,
            retryable,
        }),
    };
    write_document(&document, false)
}

fn write_document(document: &impl Serialize, timed_out: bool) -> ExitCode {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    if serde_json::to_writer(&mut output, document)
        .map_err(io::Error::other)
        .and_then(|()| output.write_all(b"\n"))
        .and_then(|()| output.flush())
        .is_err()
    {
        ExitCode::from(EX_IOERR)
    } else if timed_out {
        ExitCode::from(EX_TIMEOUT)
    } else {
        ExitCode::SUCCESS
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn readiness_parser_requires_exact_named_count_and_finite_json() {
        let options = parse_ready_options(&[
            "--count".into(),
            "2".into(),
            "--session".into(),
            "session-a".into(),
            "--session".into(),
            "session-b".into(),
            "--timeout".into(),
            "5s".into(),
            "--json".into(),
        ])
        .expect("valid readiness options");
        assert_eq!(options.count, 2);
        assert_eq!(options.timeout, Duration::from_secs(5));
        assert!(parse_ready_options(&["--count".into(), "1".into()]).is_err());
        assert!(
            parse_ready_options(&[
                "--count".into(),
                "2".into(),
                "--session".into(),
                "session-a".into(),
                "--json".into(),
            ])
            .is_err()
        );
    }

    #[test]
    fn resume_parser_preserves_interactive_spelling() {
        assert!(parse_resume_options(&["session-a".into()]).is_err());
        let options = parse_resume_options(&[
            "session-a".into(),
            "--json".into(),
            "--timeout".into(),
            "1s".into(),
        ])
        .expect("valid resume options");
        assert_eq!(options.session_id.as_str(), "session-a");
        assert_eq!(options.timeout, Duration::from_secs(1));
    }
}
