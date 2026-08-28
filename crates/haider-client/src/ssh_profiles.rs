//! Feature-negotiated SSH profile helpers.
//!
//! The constructor encodes the absence law: an older daemon makes this
//! entire surface absent instead of turning normal feature negotiation into
//! an RPC failure.

use haider_rpc::{
    ErrorData, FEATURE_SSH_PROFILES_V1, RequestBody, ResponseBody, SecretWire, SessionId,
    ShellWire, SshProfileInputWire, SshProfileUpdateWire, SshProfileWire, SshPtySizeWire,
    SshScopeWire, SshShellResultWire, SshTestResultWire, Welcome,
};

use crate::client::{ClientError, RpcClient};

#[derive(Debug)]
pub enum SshProfilesClientError {
    Client(ClientError),
    Refused {
        code: String,
        message: String,
        retryable: bool,
        data: Option<ErrorData>,
    },
    UnexpectedBody,
}

impl std::fmt::Display for SshProfilesClientError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Client(error) => error.fmt(formatter),
            Self::Refused { code, message, .. } => write!(formatter, "{code}: {message}"),
            Self::UnexpectedBody => formatter.write_str("daemon answered with an unexpected body"),
        }
    }
}

impl std::error::Error for SshProfilesClientError {}

impl From<ClientError> for SshProfilesClientError {
    fn from(error: ClientError) -> Self {
        Self::Client(error)
    }
}

/// Present only when `ssh_profiles_v1` was negotiated.
pub struct SshProfiles<'a> {
    client: &'a RpcClient,
}

#[must_use]
pub fn ssh_profiles(client: &RpcClient) -> Option<SshProfiles<'_>> {
    ssh_profiles_available(client.welcome()).then_some(SshProfiles { client })
}

#[must_use]
pub fn ssh_profiles_available(welcome: &Welcome) -> bool {
    welcome.features.contains(FEATURE_SSH_PROFILES_V1)
}

impl SshProfiles<'_> {
    pub async fn list(
        &self,
        session_id: Option<SessionId>,
    ) -> Result<Vec<SshProfileWire>, SshProfilesClientError> {
        ssh_list_response(
            self.client
                .request(RequestBody::SshList { session_id })
                .await?,
        )
    }

    pub async fn add(
        &self,
        profile: SshProfileInputWire,
    ) -> Result<SshProfileWire, SshProfilesClientError> {
        ssh_profile_response(self.client.request(RequestBody::SshAdd { profile }).await?)
    }

    pub async fn update(
        &self,
        name: impl Into<String>,
        changes: SshProfileUpdateWire,
    ) -> Result<SshProfileWire, SshProfilesClientError> {
        ssh_profile_response(
            self.client
                .request(RequestBody::SshUpdate {
                    name: name.into(),
                    changes,
                })
                .await?,
        )
    }

    pub async fn remove(&self, name: impl Into<String>) -> Result<String, SshProfilesClientError> {
        match_response(
            self.client
                .request(RequestBody::SshRemove { name: name.into() })
                .await?,
            |body| match body {
                ResponseBody::SshRemove { removed } => Some(removed),
                _ => None,
            },
        )
    }

    pub async fn test(
        &self,
        name: impl Into<String>,
        timeout_s: Option<u32>,
    ) -> Result<SshTestResultWire, SshProfilesClientError> {
        match_response(
            self.client
                .request(RequestBody::SshTest {
                    name: name.into(),
                    timeout_s,
                })
                .await?,
            |body| match body {
                ResponseBody::SshTest { result } => Some(result),
                _ => None,
            },
        )
    }

    pub async fn set_scope(
        &self,
        session_id: SessionId,
        scope: SshScopeWire,
    ) -> Result<SshScopeWire, SshProfilesClientError> {
        match_response(
            self.client
                .request(RequestBody::SessionSetSshScope { session_id, scope })
                .await?,
            |body| match body {
                ResponseBody::SessionSetSshScope { scope, .. } => Some(scope),
                _ => None,
            },
        )
    }

    pub async fn shell(
        &self,
        name: impl Into<String>,
        command: impl Into<String>,
        cwd: Option<String>,
        timeout_s: Option<u32>,
    ) -> Result<SshShellResultWire, SshProfilesClientError> {
        match_response(
            self.client
                .request(RequestBody::SshShell {
                    name: name.into(),
                    command: command.into(),
                    cwd,
                    timeout_s,
                })
                .await?,
            |body| match body {
                ResponseBody::SshShell { result } => Some(result),
                _ => None,
            },
        )
    }

    pub async fn open_pty(
        &self,
        name: impl Into<String>,
        session_id: Option<SessionId>,
        term: impl Into<String>,
        size: SshPtySizeWire,
    ) -> Result<ShellWire, SshProfilesClientError> {
        match_response(
            self.client
                .request(RequestBody::SshShellOpen {
                    name: name.into(),
                    session_id,
                    term: term.into(),
                    size,
                })
                .await?,
            |body| match body {
                ResponseBody::SshShellOpen { shell } => Some(shell),
                _ => None,
            },
        )
    }

    pub async fn input_b64(
        &self,
        id: impl Into<String>,
        data_b64: impl Into<String>,
    ) -> Result<ShellWire, SshProfilesClientError> {
        match_response(
            self.client
                .request(RequestBody::SshShellInput {
                    id: id.into(),
                    data_b64: SecretWire::new(data_b64),
                })
                .await?,
            |body| match body {
                ResponseBody::SshShellInput { shell } => Some(shell),
                _ => None,
            },
        )
    }

    pub async fn resize(
        &self,
        id: impl Into<String>,
        size: SshPtySizeWire,
    ) -> Result<ShellWire, SshProfilesClientError> {
        match_response(
            self.client
                .request(RequestBody::SshShellResize {
                    id: id.into(),
                    size,
                })
                .await?,
            |body| match body {
                ResponseBody::SshShellResize { shell } => Some(shell),
                _ => None,
            },
        )
    }

    pub async fn eof(&self, id: impl Into<String>) -> Result<ShellWire, SshProfilesClientError> {
        match_response(
            self.client
                .request(RequestBody::SshShellEof { id: id.into() })
                .await?,
            |body| match body {
                ResponseBody::SshShellEof { shell } => Some(shell),
                _ => None,
            },
        )
    }
}

pub fn ssh_list_response(
    body: ResponseBody,
) -> Result<Vec<SshProfileWire>, SshProfilesClientError> {
    match_response(body, |body| match body {
        ResponseBody::SshList { profiles } => Some(profiles),
        _ => None,
    })
}

fn ssh_profile_response(body: ResponseBody) -> Result<SshProfileWire, SshProfilesClientError> {
    match_response(body, |body| match body {
        ResponseBody::SshAdd { profile } | ResponseBody::SshUpdate { profile } => Some(profile),
        _ => None,
    })
}

fn match_response<T>(
    body: ResponseBody,
    project: impl FnOnce(ResponseBody) -> Option<T>,
) -> Result<T, SshProfilesClientError> {
    match body {
        ResponseBody::Error {
            code,
            message,
            retryable,
            data,
        } => Err(SshProfilesClientError::Refused {
            code,
            message,
            retryable,
            data,
        }),
        body => project(body).ok_or(SshProfilesClientError::UnexpectedBody),
    }
}
