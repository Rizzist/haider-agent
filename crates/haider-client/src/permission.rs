//! Typed client actions for an in-session computer OS-permission card.

use std::time::Duration;

use haider_protocol::ids::SessionId;
use haider_protocol::permission::SystemPermission;
use haider_rpc::{
    FEATURE_COMPUTER_PERMISSION_ACTIONS_V1, LifecyclePhase, RequestBody, ResponseBody, WireFrame,
};

use crate::client::{ClientConfig, ClientError, ConnectError, RpcClient, connect};
use crate::profile::{ResolvedProfile, effective_uid};
use crate::spawn::{
    EnsureError, EnsureOptions, EnsuredDaemon, ensure_daemon, signal_authenticated_peer,
};

const DRAIN_DEADLINE: Duration = Duration::from_secs(20);

#[derive(Debug)]
pub enum ComputerPermissionClientError {
    Client(ClientError),
    Connect(ConnectError),
    Ensure(EnsureError),
    Refused { code: String, message: String },
    Protocol(&'static str),
    Restart(String),
}

impl std::fmt::Display for ComputerPermissionClientError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Client(error) => error.fmt(formatter),
            Self::Connect(error) => error.fmt(formatter),
            Self::Ensure(error) => error.fmt(formatter),
            Self::Refused { code, message } => write!(formatter, "{code}: {message}"),
            Self::Protocol(message) => formatter.write_str(message),
            Self::Restart(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for ComputerPermissionClientError {}

/// Builds the narrow action request. A URL is intentionally not accepted:
/// the daemon maps the enum to one compiled macOS System Settings deep link.
#[must_use]
pub fn open_permission_settings_request(
    session_id: SessionId,
    request_id: impl Into<String>,
    permission: SystemPermission,
) -> RequestBody {
    RequestBody::ComputerPermissionOpenSettings {
        session_id,
        request_id: request_id.into(),
        permission,
    }
}

pub async fn open_permission_settings(
    client: &RpcClient,
    session_id: SessionId,
    request_id: impl Into<String>,
    permission: SystemPermission,
) -> Result<(), ComputerPermissionClientError> {
    let response = client
        .request(open_permission_settings_request(
            session_id, request_id, permission,
        ))
        .await
        .map_err(ComputerPermissionClientError::Client)?;
    match response {
        ResponseBody::ComputerPermissionOpenSettings { permission: opened }
            if opened == permission =>
        {
            Ok(())
        }
        ResponseBody::Error { code, message, .. } => {
            Err(ComputerPermissionClientError::Refused { code, message })
        }
        _ => Err(ComputerPermissionClientError::Protocol(
            "computer.permission_open_settings response method mismatch",
        )),
    }
}

/// Gracefully restarts the authenticated profile daemon for a permission
/// card, then returns a connection to a fresh healthy generation. Exactly one
/// signal is sent. The unresolved menu remains durable and startup recovery
/// re-activates its poll/preflight before the parked tool call continues.
pub async fn restart_daemon_for_permission(
    profile: &ResolvedProfile,
    mut options: EnsureOptions,
) -> Result<EnsuredDaemon, ComputerPermissionClientError> {
    options
        .required_features
        .insert(FEATURE_COMPUTER_PERMISSION_ACTIONS_V1.into());
    let mut config = ClientConfig {
        client_name: "haider-permission-restart".into(),
        client_instance_id: format!("permission-restart-{}", std::process::id()),
        ..options.client.clone()
    };
    config.ping_interval = Duration::from_secs(5);
    let connected = connect(&profile.endpoint_path, config)
        .await
        .map_err(ComputerPermissionClientError::Connect)?;
    if connected.welcome.lifecycle_phase != LifecyclePhase::Ready
        || connected.welcome.profile_id != profile.profile_id
        || !connected
            .welcome
            .features
            .contains(FEATURE_COMPUTER_PERMISSION_ACTIONS_V1)
    {
        connected.client.close();
        return Err(ComputerPermissionClientError::Restart(
            "authenticated daemon Welcome does not match the ready profile".into(),
        ));
    }
    if connected.peer_credentials.uid != effective_uid() {
        connected.client.close();
        return Err(ComputerPermissionClientError::Restart(
            "daemon socket peer is not owned by this user".into(),
        ));
    }
    let Some(pid) = connected.peer_credentials.pid.filter(|pid| *pid != 0) else {
        connected.client.close();
        return Err(ComputerPermissionClientError::Restart(
            "daemon socket did not expose an authenticated peer PID".into(),
        ));
    };
    let old_instance = connected.welcome.instance_id.clone();
    let old_generation = connected.welcome.daemon_generation;
    let mut events =
        connected
            .client
            .take_events()
            .ok_or(ComputerPermissionClientError::Protocol(
                "permission restart could not retain events",
            ))?;
    signal_authenticated_peer(pid).map_err(|error| {
        ComputerPermissionClientError::Restart(format!(
            "could not signal authenticated daemon peer: {error}"
        ))
    })?;
    let notice = tokio::time::timeout(DRAIN_DEADLINE, async {
        while let Some(frame) = events.recv().await {
            if let WireFrame::ServerDraining {
                instance_id,
                daemon_generation,
                ..
            } = frame
            {
                return Some((instance_id, daemon_generation));
            }
        }
        None
    })
    .await
    .map_err(|_| ComputerPermissionClientError::Restart("daemon drain notice timed out".into()))?
    .ok_or_else(|| {
        ComputerPermissionClientError::Restart("daemon disconnected without a drain notice".into())
    })?;
    if notice != (old_instance.clone(), old_generation) {
        connected.client.close();
        return Err(ComputerPermissionClientError::Restart(
            "daemon drain notice did not match its authenticated Welcome".into(),
        ));
    }
    tokio::time::timeout(DRAIN_DEADLINE, connected.client.disconnected())
        .await
        .map_err(|_| {
            ComputerPermissionClientError::Restart("daemon disconnect timed out".into())
        })?;

    let ensured = ensure_daemon(profile, options)
        .await
        .map_err(ComputerPermissionClientError::Ensure)?;
    if ensured.welcome.instance_id == old_instance
        && ensured.welcome.daemon_generation == old_generation
    {
        ensured.client.close();
        return Err(ComputerPermissionClientError::Restart(
            "permission restart reattached to the old daemon generation".into(),
        ));
    }
    Ok(ensured)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_settings_request_accepts_no_url() -> Result<(), serde_json::Error> {
        let request = open_permission_settings_request(
            SessionId::new("session-1"),
            "permission-1",
            SystemPermission::Accessibility,
        );
        assert_eq!(
            serde_json::to_value(request)?,
            serde_json::json!({
                "method": "computer.permission_open_settings",
                "session_id": "session-1",
                "request_id": "permission-1",
                "permission": "accessibility"
            })
        );
        Ok(())
    }
}
