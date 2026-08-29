//! Headless door for the daemon-owned per-session attention acknowledgement.

use std::collections::BTreeSet;
use std::io::{self, Write};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use haider_client::{ClientError, EnsureError, EnsureOptions, ProfileEnv, resolve_profile};
use haider_protocol::ids::SessionId;
use haider_rpc::{
    AttachMode, Capability, CapabilitySet, ClientKind, CommandId, RequestBody, ResponseBody,
};
use serde::Serialize;

use super::run::{EX_IOERR, EX_PROTOCOL, EX_SOFTWARE, EX_UNAVAILABLE, EX_USAGE};

const SEEN_SCHEMA: &str = "haider.session_seen.v1";

#[derive(Serialize)]
struct SeenReceiptDocument {
    schema: &'static str,
    session_id: String,
    seen_at_ms: u64,
    seen_seq: u64,
    worker_generation: u64,
}

#[derive(Debug)]
enum SeenError {
    Ensure(EnsureError),
    MissingFeature,
    Client(ClientError),
    Rpc {
        code: String,
        message: String,
        retryable: bool,
    },
    Protocol(&'static str),
    Io(String),
}

impl std::fmt::Display for SeenError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ensure(error) => write!(formatter, "{error}"),
            Self::MissingFeature => {
                formatter.write_str("missing_feature: daemon does not advertise session_seen_v1")
            }
            Self::Client(error) => write!(formatter, "{error}"),
            Self::Rpc {
                code,
                message,
                retryable,
            } => write!(
                formatter,
                "daemon rejected session seen ({code}, retryable={retryable}): {message}"
            ),
            Self::Protocol(message) => formatter.write_str(message),
            Self::Io(message) => formatter.write_str(message),
        }
    }
}

pub(crate) async fn session_seen_command(session_id: &str, rest: &[String]) -> ExitCode {
    if !rest.is_empty() {
        eprintln!("haider session seen: usage: haider session <session-id> seen");
        return ExitCode::from(EX_USAGE);
    }
    let profile = match resolve_profile(&ProfileEnv::capture()) {
        Ok(profile) => profile,
        Err(error) => {
            eprintln!("haider session seen: {error}");
            return ExitCode::from(EX_PROTOCOL);
        }
    };
    let mut ensure = EnsureOptions::default();
    ensure.required_features = BTreeSet::from([haider_rpc::FEATURE_SESSION_SEEN_V1.to_owned()]);
    ensure.client = haider_client::ClientConfig {
        client_name: "haider-session-seen".into(),
        client_kind: ClientKind::Headless,
        capabilities: CapabilitySet::from([Capability::View, Capability::Control]),
        ..ensure.client
    };
    let ensured = match haider_client::ensure_daemon(&profile, ensure).await {
        Ok(ensured) => ensured,
        Err(error) => return failure(SeenError::Ensure(error)),
    };
    if !ensured
        .welcome
        .features
        .contains(haider_rpc::FEATURE_SESSION_SEEN_V1)
    {
        let _ = ensured.client.close();
        return failure(SeenError::MissingFeature);
    }
    let result = execute(&ensured.client, SessionId::new(session_id)).await;
    let _ = ensured.client.close();
    match result {
        Ok(receipt) => write_receipt(&receipt),
        Err(error) => failure(error),
    }
}

async fn execute(
    client: &haider_client::RpcClient,
    session_id: SessionId,
) -> Result<SeenReceiptDocument, SeenError> {
    let (attachment_id, worker_generation) = match client
        .request(RequestBody::SessionAttach {
            session_id: session_id.clone(),
            after_seq: 0,
            mode: AttachMode::Control,
            sealed_replay: false,
        })
        .await
        .map_err(SeenError::Client)?
    {
        ResponseBody::SessionAttach {
            attachment_id,
            attach_state,
        } => (attachment_id, attach_state.worker_generation),
        ResponseBody::Error {
            code,
            message,
            retryable,
            ..
        } => {
            return Err(SeenError::Rpc {
                code,
                message,
                retryable,
            });
        }
        _ => {
            return Err(SeenError::Protocol(
                "session.attach response method mismatch",
            ));
        }
    };
    let result = client
        .request(RequestBody::SessionSeen {
            command_id: CommandId::new(command_id()),
            session_id: session_id.clone(),
            worker_generation,
        })
        .await
        .map_err(SeenError::Client);
    let _ = client
        .request(RequestBody::SessionDetach { attachment_id })
        .await;
    match result? {
        ResponseBody::SessionSeen {
            session_id: returned,
            seen_at_ms,
            seen_seq,
            worker_generation,
        } if returned == session_id => Ok(SeenReceiptDocument {
            schema: SEEN_SCHEMA,
            session_id: returned.as_str().to_owned(),
            seen_at_ms,
            seen_seq,
            worker_generation,
        }),
        ResponseBody::Error {
            code,
            message,
            retryable,
            ..
        } => Err(SeenError::Rpc {
            code,
            message,
            retryable,
        }),
        _ => Err(SeenError::Protocol("session.seen response method mismatch")),
    }
}

fn write_receipt(receipt: &SeenReceiptDocument) -> ExitCode {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    if let Err(error) = serde_json::to_writer(&mut output, receipt)
        .map_err(io::Error::other)
        .and_then(|()| output.write_all(b"\n"))
        .and_then(|()| output.flush())
    {
        return failure(SeenError::Io(format!("stdout failed: {error}")));
    }
    ExitCode::SUCCESS
}

fn command_id() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("session-seen-{}-{now}", std::process::id())
}

fn failure(error: SeenError) -> ExitCode {
    eprintln!("haider session seen: {error}");
    ExitCode::from(match error {
        SeenError::Ensure(EnsureError::ProtocolMismatch(_))
        | SeenError::Ensure(EnsureError::MissingFeatures { .. })
        | SeenError::Ensure(EnsureError::ProfileMismatch { .. })
        | SeenError::MissingFeature
        | SeenError::Protocol(_) => EX_PROTOCOL,
        SeenError::Ensure(_) => EX_UNAVAILABLE,
        SeenError::Client(ClientError::Disconnected(_)) | SeenError::Io(_) => EX_IOERR,
        SeenError::Client(ClientError::Encode(_) | ClientError::MissingFeature(_)) => EX_SOFTWARE,
        SeenError::Rpc { .. } => EX_UNAVAILABLE,
    })
}
