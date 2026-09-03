//! Headless receipt-backed session workspace recovery door.

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

#[derive(Serialize)]
struct WorkspaceReceiptDocument {
    schema: &'static str,
    session_id: String,
    path: String,
    selected_seq: u64,
    worker_generation: u64,
}

#[derive(Debug)]
enum WorkspaceError {
    Ensure(EnsureError),
    MissingFeature,
    Target(String),
    Client(ClientError),
    Rpc(String, String),
    Protocol(&'static str),
    Io(String),
}

impl std::fmt::Display for WorkspaceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ensure(error) => write!(formatter, "{error}"),
            Self::MissingFeature => formatter
                .write_str("missing_feature: daemon does not advertise session_workspace_set_v1"),
            Self::Target(message) => formatter.write_str(message),
            Self::Client(error) => write!(formatter, "{error}"),
            Self::Rpc(code, message) => write!(
                formatter,
                "daemon rejected workspace set ({code}): {message}"
            ),
            Self::Protocol(message) => formatter.write_str(message),
            Self::Io(message) => formatter.write_str(message),
        }
    }
}

pub(crate) async fn session_workspace_command(
    session_id: Option<&str>,
    rest: &[String],
) -> ExitCode {
    let [action, path] = rest else {
        eprintln!(
            "haider session workspace: usage: haider session workspace set <path>\n       haider session <session-id> workspace set <path>"
        );
        return ExitCode::from(EX_USAGE);
    };
    if action != "set" {
        eprintln!("haider session workspace: expected `set`");
        return ExitCode::from(EX_USAGE);
    }
    let profile = match resolve_profile(&ProfileEnv::capture()) {
        Ok(profile) => profile,
        Err(error) => {
            eprintln!("haider session workspace: {error}");
            return ExitCode::from(EX_PROTOCOL);
        }
    };
    let mut ensure = EnsureOptions::default();
    ensure.required_features =
        BTreeSet::from([haider_rpc::FEATURE_SESSION_WORKSPACE_SET_V1.to_owned()]);
    ensure.client = haider_client::ClientConfig {
        client_name: "haider-session-workspace".into(),
        client_kind: ClientKind::Headless,
        capabilities: CapabilitySet::from([Capability::View, Capability::Control]),
        ..ensure.client
    };
    let ensured = match haider_client::ensure_daemon(&profile, ensure).await {
        Ok(ensured) => ensured,
        Err(error) => return failure(WorkspaceError::Ensure(error)),
    };
    if !ensured
        .welcome
        .features
        .contains(haider_rpc::FEATURE_SESSION_WORKSPACE_SET_V1)
    {
        let _ = ensured.client.close();
        return failure(WorkspaceError::MissingFeature);
    }
    let session_id = match session_id {
        Some(session_id) => SessionId::new(session_id),
        None => match resolve_implicit_session(&ensured.client).await {
            Ok(session_id) => session_id,
            Err(error) => {
                let _ = ensured.client.close();
                return failure(error);
            }
        },
    };
    let result = execute(&ensured.client, session_id, path.clone()).await;
    let _ = ensured.client.close();
    match result {
        Ok(receipt) => write_receipt(&receipt),
        Err(error) => failure(error),
    }
}

/// The shorthand has no ambient TUI binding on a headless connection. It is
/// therefore safe only when the profile contains exactly one session; profiles
/// with zero or multiple sessions must use the explicit-id spelling.
async fn resolve_implicit_session(
    client: &haider_client::RpcClient,
) -> Result<SessionId, WorkspaceError> {
    match client
        .request(RequestBody::SessionList {
            cursor: None,
            limit: 2,
        })
        .await
        .map_err(WorkspaceError::Client)?
    {
        ResponseBody::SessionList {
            sessions,
            next_cursor,
        } => require_unique_session(
            sessions.into_iter().map(|session| session.session_id),
            next_cursor.is_some(),
        ),
        ResponseBody::Error { code, message, .. } => Err(WorkspaceError::Rpc(code, message)),
        _ => Err(WorkspaceError::Protocol(
            "session.list response method mismatch",
        )),
    }
}

fn require_unique_session(
    session_ids: impl IntoIterator<Item = SessionId>,
    has_more: bool,
) -> Result<SessionId, WorkspaceError> {
    let mut session_ids = session_ids.into_iter();
    let first = session_ids.next();
    if has_more || session_ids.next().is_some() {
        return Err(WorkspaceError::Target(
            "multiple sessions exist; use `haider session <session-id> workspace set <path>`"
                .to_owned(),
        ));
    }
    first.ok_or_else(|| {
        WorkspaceError::Target(
            "no session exists; use an explicit session id after creating one".to_owned(),
        )
    })
}

async fn execute(
    client: &haider_client::RpcClient,
    session_id: SessionId,
    path: String,
) -> Result<WorkspaceReceiptDocument, WorkspaceError> {
    let (attachment_id, worker_generation) = match client
        .request(RequestBody::SessionAttach {
            session_id: session_id.clone(),
            after_seq: 0,
            mode: AttachMode::Control,
            sealed_replay: false,
        })
        .await
        .map_err(WorkspaceError::Client)?
    {
        ResponseBody::SessionAttach {
            attachment_id,
            attach_state,
        } => (attachment_id, attach_state.worker_generation),
        ResponseBody::Error { code, message, .. } => {
            return Err(WorkspaceError::Rpc(code, message));
        }
        _ => {
            return Err(WorkspaceError::Protocol(
                "session.attach response method mismatch",
            ));
        }
    };
    let result = client
        .request(RequestBody::SessionWorkspaceSet {
            command_id: CommandId::new(command_id()),
            session_id: session_id.clone(),
            worker_generation,
            path,
        })
        .await
        .map_err(WorkspaceError::Client);
    let _ = client
        .request(RequestBody::SessionDetach { attachment_id })
        .await;
    match result? {
        ResponseBody::SessionWorkspaceSet {
            session_id: returned,
            path,
            selected_seq,
            worker_generation,
        } if returned == session_id => Ok(WorkspaceReceiptDocument {
            schema: "haider.session_workspace.v1",
            session_id: returned.as_str().to_owned(),
            path,
            selected_seq,
            worker_generation,
        }),
        ResponseBody::Error { code, message, .. } => Err(WorkspaceError::Rpc(code, message)),
        _ => Err(WorkspaceError::Protocol(
            "session.workspace.set response method mismatch",
        )),
    }
}

fn write_receipt(receipt: &WorkspaceReceiptDocument) -> ExitCode {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    if let Err(error) = serde_json::to_writer(&mut output, receipt)
        .map_err(io::Error::other)
        .and_then(|()| output.write_all(b"\n"))
        .and_then(|()| output.flush())
    {
        return failure(WorkspaceError::Io(format!("stdout failed: {error}")));
    }
    ExitCode::SUCCESS
}

fn command_id() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("session-workspace-{}-{now}", std::process::id())
}

fn failure(error: WorkspaceError) -> ExitCode {
    eprintln!("haider session workspace: {error}");
    ExitCode::from(match error {
        WorkspaceError::Ensure(EnsureError::ProtocolMismatch(_))
        | WorkspaceError::Ensure(EnsureError::MissingFeatures { .. })
        | WorkspaceError::Ensure(EnsureError::ProfileMismatch { .. })
        | WorkspaceError::MissingFeature
        | WorkspaceError::Protocol(_) => EX_PROTOCOL,
        WorkspaceError::Ensure(_) | WorkspaceError::Target(_) | WorkspaceError::Rpc(_, _) => {
            EX_UNAVAILABLE
        }
        WorkspaceError::Client(ClientError::Disconnected(_)) | WorkspaceError::Io(_) => EX_IOERR,
        WorkspaceError::Client(ClientError::Encode(_) | ClientError::MissingFeature(_)) => {
            EX_SOFTWARE
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shorthand_requires_one_unambiguous_session() {
        assert!(matches!(
            require_unique_session([SessionId::new("only")], false),
            Ok(session) if session.as_str() == "only"
        ));
        assert!(require_unique_session(Vec::<SessionId>::new(), false).is_err());
        assert!(require_unique_session([SessionId::new("a"), SessionId::new("b")], false).is_err());
        assert!(require_unique_session([SessionId::new("a")], true).is_err());
    }
}
