//! Feature-negotiated provider-lockdown management helpers.

use haider_rpc::{
    CommandId, ErrorData, FEATURE_PROVIDER_LOCKDOWN_V1, LockdownStatusWire, ProviderSummaryWire,
    ProviderTrustWire, RequestBody, ResponseBody, Welcome,
};

use crate::client::{ClientError, RpcClient};

#[derive(Debug)]
pub enum LockdownClientError {
    Client(ClientError),
    Refused {
        code: String,
        message: String,
        retryable: bool,
        data: Option<ErrorData>,
    },
    UnexpectedBody,
}

impl std::fmt::Display for LockdownClientError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Client(error) => error.fmt(formatter),
            Self::Refused { code, message, .. } => write!(formatter, "{code}: {message}"),
            Self::UnexpectedBody => formatter.write_str("daemon answered with an unexpected body"),
        }
    }
}

impl std::error::Error for LockdownClientError {}

impl From<ClientError> for LockdownClientError {
    fn from(error: ClientError) -> Self {
        Self::Client(error)
    }
}

pub struct ProviderLockdown<'a> {
    client: &'a RpcClient,
}

#[must_use]
pub fn provider_lockdown(client: &RpcClient) -> Option<ProviderLockdown<'_>> {
    provider_lockdown_available(client.welcome()).then_some(ProviderLockdown { client })
}

#[must_use]
pub fn provider_lockdown_available(welcome: &Welcome) -> bool {
    welcome.features.contains(FEATURE_PROVIDER_LOCKDOWN_V1)
}

impl ProviderLockdown<'_> {
    pub async fn status(
        &self,
        provider: Option<String>,
    ) -> Result<LockdownStatusWire, LockdownClientError> {
        lockdown_status_response(
            self.client
                .request(RequestBody::LockdownStatus { provider })
                .await?,
        )
    }

    pub async fn set_quota(
        &self,
        command_id: CommandId,
        bytes: u64,
    ) -> Result<LockdownStatusWire, LockdownClientError> {
        lockdown_set_quota_response(
            self.client
                .request(RequestBody::LockdownSetQuota { command_id, bytes })
                .await?,
        )
    }

    pub async fn set_trust(
        &self,
        command_id: CommandId,
        provider: String,
        trust: ProviderTrustWire,
        expected_revision: u64,
    ) -> Result<(ProviderSummaryWire, u64), LockdownClientError> {
        provider_set_trust_response(
            self.client
                .request(RequestBody::ProviderSetTrust {
                    command_id,
                    name: provider,
                    trust,
                    expected_revision,
                })
                .await?,
        )
    }
}

pub fn lockdown_status_response(
    body: ResponseBody,
) -> Result<LockdownStatusWire, LockdownClientError> {
    match body {
        ResponseBody::LockdownStatus { status } => Ok(status),
        ResponseBody::Error {
            code,
            message,
            retryable,
            data,
        } => Err(LockdownClientError::Refused {
            code,
            message,
            retryable,
            data,
        }),
        _ => Err(LockdownClientError::UnexpectedBody),
    }
}

pub fn lockdown_set_quota_response(
    body: ResponseBody,
) -> Result<LockdownStatusWire, LockdownClientError> {
    match body {
        ResponseBody::LockdownSetQuota { status } => Ok(status),
        ResponseBody::Error {
            code,
            message,
            retryable,
            data,
        } => Err(LockdownClientError::Refused {
            code,
            message,
            retryable,
            data,
        }),
        _ => Err(LockdownClientError::UnexpectedBody),
    }
}

pub fn provider_set_trust_response(
    body: ResponseBody,
) -> Result<(ProviderSummaryWire, u64), LockdownClientError> {
    match body {
        ResponseBody::ProviderSetTrust { provider, revision } => Ok((provider, revision)),
        ResponseBody::Error {
            code,
            message,
            retryable,
            data,
        } => Err(LockdownClientError::Refused {
            code,
            message,
            retryable,
            data,
        }),
        _ => Err(LockdownClientError::UnexpectedBody),
    }
}
