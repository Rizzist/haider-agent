//! Restore an accepted prompt, or cancel its exact run when response won.

use std::io::{self, Write};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use haider_client::{
    ClientConfig, ClientError, EnsureOptions, ProfileEnv, RpcClient, TurnRetraction,
    TurnRetractionError, TurnRetractionOutcome, resolve_profile,
};
use haider_protocol::ids::SessionId;
use haider_rpc::{
    AttachMode, Capability, CapabilitySet, ClientKind, CommandId, RequestBody, ResponseBody,
};
use serde_json::{Value, json};

use super::run::{EX_IOERR, EX_PROTOCOL, EX_UNAVAILABLE, EX_USAGE};

const USAGE: &str = "usage: haider session retract --session <id> [--json]";

fn parse(args: &[String]) -> Result<SessionId, String> {
    let mut session = None;
    let mut json = false;
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--session" if session.is_none() => {
                let value = args
                    .next()
                    .filter(|value| !value.trim().is_empty() && !value.starts_with('-'))
                    .ok_or("--session requires a value")?;
                session = Some(SessionId::new(value.clone()));
            }
            "--json" if !json => json = true,
            "--session" | "--json" => return Err(format!("duplicate {flag}")),
            _ => return Err(format!("unknown argument {flag}")),
        }
    }
    session.ok_or_else(|| "--session is required".into())
}

pub(crate) async fn command(args: &[String]) -> ExitCode {
    if matches!(args, [flag] if flag == "--help" || flag == "-h") {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let session_id = match parse(args) {
        Ok(session_id) => session_id,
        Err(error) => {
            eprintln!("haider session retract: {error}\n{USAGE}");
            return ExitCode::from(EX_USAGE);
        }
    };
    let profile = match resolve_profile(&ProfileEnv::capture()) {
        Ok(profile) => profile,
        Err(error) => {
            eprintln!("haider session retract: {error}");
            return ExitCode::from(EX_PROTOCOL);
        }
    };
    let ensure = EnsureOptions {
        required_features: [haider_rpc::FEATURE_TURN_RETRACT_V1.to_owned()].into(),
        client: ClientConfig {
            client_name: "haider-session-retract".into(),
            client_kind: ClientKind::Headless,
            capabilities: CapabilitySet::from([Capability::View, Capability::Control]),
            ..Default::default()
        },
        ..Default::default()
    };
    let mut pending = None;
    // One lost-receipt redial, with the ordinary per-request/startup bounds.
    // A retry never re-observes an already-pinned run or mints another receipt.
    let mut retried = false;
    loop {
        let ensured = match haider_client::ensure_daemon(&profile, ensure.clone()).await {
            Ok(ensured) => ensured,
            Err(error) => {
                eprintln!("haider session retract: {error}");
                return ExitCode::from(EX_UNAVAILABLE);
            }
        };
        // The transient control attachment can replay history while the
        // command waits. Drain it so the bounded reader stays healthy; the
        // retraction's returned receipt is the restoration authority here.
        let drain = ensured
            .client
            .take_events()
            .map(|mut events| tokio::spawn(async move { while events.recv().await.is_some() {} }));
        let result = execute_connected(&ensured.client, &session_id, &mut pending).await;
        let _ = ensured.client.close();
        if let Some(drain) = drain {
            drain.abort();
        }
        match result {
            Ok(outcome) => return write_outcome(outcome),
            Err(TurnRetractionError::Client(ClientError::Disconnected(_))) if !retried => {
                retried = true;
            }
            Err(error) => {
                eprintln!("haider session retract: {error}");
                return ExitCode::from(match error {
                    TurnRetractionError::Client(ClientError::Disconnected(_)) => EX_IOERR,
                    TurnRetractionError::Daemon { .. } => EX_UNAVAILABLE,
                    _ => EX_PROTOCOL,
                });
            }
        }
    }
}

async fn execute_connected(
    client: &RpcClient,
    session_id: &SessionId,
    pending: &mut Option<TurnRetraction>,
) -> Result<TurnRetractionOutcome, TurnRetractionError> {
    let attached = client
        .request(RequestBody::SessionAttach {
            session_id: session_id.clone(),
            after_seq: 0,
            mode: AttachMode::Control,
            sealed_replay: false,
        })
        .await
        .map_err(TurnRetractionError::Client)?;
    match attached {
        ResponseBody::SessionAttach { attach_state, .. }
            if attach_state.session_id == *session_id => {}
        response => {
            return Err(response_error(
                response,
                "unexpected session.attach response",
            ));
        }
    }
    if pending.is_none() {
        let response = client
            .request(RequestBody::SessionObserve {
                session_id: session_id.clone(),
                last_event_limit: 0,
                metadata_only: false,
            })
            .await
            .map_err(TurnRetractionError::Client)?;
        let (run_id, worker_generation) = match response {
            ResponseBody::SessionObserve { digest } if digest.session_id == *session_id => {
                let run_id = digest.run_id.ok_or_else(|| TurnRetractionError::Daemon {
                    code: haider_rpc::ERROR_CODE_RUN_NOT_ACTIVE.into(),
                    message: "session has no active accepted run".into(),
                    retryable: false,
                })?;
                (run_id, digest.worker_generation)
            }
            response => {
                return Err(response_error(
                    response,
                    "unexpected session.observe response",
                ));
            }
        };
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        *pending = Some(TurnRetraction::new(
            session_id.clone(),
            run_id,
            worker_generation,
            CommandId::new(format!("session-retract-{}-{nonce}", std::process::id())),
        ));
    }
    match pending.as_mut() {
        Some(request) => request.execute(client).await,
        None => Err(TurnRetractionError::Protocol(
            "retraction coordinates were not resolved",
        )),
    }
}

fn response_error(response: ResponseBody, expected: &'static str) -> TurnRetractionError {
    match response {
        ResponseBody::Error {
            code,
            message,
            retryable,
            ..
        } => TurnRetractionError::Daemon {
            code,
            message,
            retryable,
        },
        _ => TurnRetractionError::Protocol(expected),
    }
}

fn outcome_document(outcome: TurnRetractionOutcome) -> Value {
    match outcome {
        TurnRetractionOutcome::Retracted(prompt) => json!({
            "schema": "haider.session_retract.v1",
            "session_id": prompt.session_id,
            "run_id": prompt.run_id,
            "status": "retracted",
            "prompt_seq": prompt.prompt_seq,
            "retracted_seq": prompt.retracted_seq,
            "text": prompt.text,
            "attachments": prompt.attachments,
        }),
        TurnRetractionOutcome::Cancelled {
            session_id,
            run_id,
            status,
            terminal_seq,
        } => json!({
            "schema": "haider.session_retract.v1",
            "session_id": session_id,
            "run_id": run_id,
            "status": "cancelled",
            "reason": "too_late",
            "cancel_status": status,
            "terminal_seq": terminal_seq,
        }),
    }
}

fn write_outcome(outcome: TurnRetractionOutcome) -> ExitCode {
    let mut output = io::stdout().lock();
    if let Err(error) = serde_json::to_writer(&mut output, &outcome_document(outcome))
        .map_err(io::Error::other)
        .and_then(|()| output.write_all(b"\n"))
        .and_then(|()| output.flush())
    {
        eprintln!("haider session retract: stdout failed: {error}");
        return ExitCode::from(EX_IOERR);
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
#[path = "session_retract_tests.rs"]
mod tests;
