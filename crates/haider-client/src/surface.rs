//! Thin typed helpers for daemon-owned volatile session input and status.
//!
//! Unsolicited [`haider_rpc::WireFrame::SessionSurfaceDelta`] and
//! [`haider_rpc::WireFrame::SessionInputInjected`] values use the ordinary
//! [`RpcClient::take_events`] stream; the generic reader already forwards
//! them without blocking correlated replies.

use haider_rpc::haider_protocol::ids::SessionId;
use haider_rpc::{ErrorData, RequestBody, ResponseBody};
pub use haider_rpc::{
    SurfaceInjectOp, SurfaceInputPublishWire, SurfaceInputWire, SurfaceStatusPublishWire,
    SurfaceStatusWire,
};

use crate::client::{ClientError, RpcClient};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfacePublishAck {
    pub session_id: SessionId,
    pub accepted_input_revision: Option<u64>,
    pub accepted_status_revision: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceWatchSnapshot {
    pub session_id: SessionId,
    pub input: Option<SurfaceInputWire>,
    pub status: Option<SurfaceStatusWire>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceInjectAck {
    pub session_id: SessionId,
    pub delivered: bool,
}

/// A transport failure, typed daemon refusal, or mismatched response method.
#[derive(Debug)]
pub enum SurfaceClientError {
    Client(ClientError),
    Refused {
        code: String,
        message: String,
        retryable: bool,
        data: Option<ErrorData>,
    },
    UnexpectedBody,
}

impl std::fmt::Display for SurfaceClientError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Client(error) => error.fmt(formatter),
            Self::Refused { code, message, .. } => write!(formatter, "{code}: {message}"),
            Self::UnexpectedBody => formatter.write_str("daemon answered with an unexpected body"),
        }
    }
}

impl std::error::Error for SurfaceClientError {}

impl From<ClientError> for SurfaceClientError {
    fn from(error: ClientError) -> Self {
        Self::Client(error)
    }
}

#[must_use]
pub fn surface_publish_request(
    session_id: SessionId,
    input: Option<SurfaceInputPublishWire>,
    status: Option<SurfaceStatusPublishWire>,
) -> RequestBody {
    RequestBody::SessionSurfacePublish {
        session_id,
        input,
        status,
    }
}

#[must_use]
pub fn surface_watch_request(session_id: SessionId) -> RequestBody {
    RequestBody::SessionSurfaceWatch { session_id }
}

#[must_use]
pub fn input_inject_request(session_id: SessionId, op: SurfaceInjectOp) -> RequestBody {
    RequestBody::SessionInputInject { session_id, op }
}

pub fn surface_publish_ack(body: ResponseBody) -> Result<SurfacePublishAck, SurfaceClientError> {
    match body {
        ResponseBody::SessionSurfacePublished {
            session_id,
            accepted_input_revision,
            accepted_status_revision,
        } => Ok(SurfacePublishAck {
            session_id,
            accepted_input_revision,
            accepted_status_revision,
        }),
        ResponseBody::Error {
            code,
            message,
            retryable,
            data,
        } => Err(SurfaceClientError::Refused {
            code,
            message,
            retryable,
            data,
        }),
        _ => Err(SurfaceClientError::UnexpectedBody),
    }
}

pub fn surface_watch_snapshot(
    body: ResponseBody,
) -> Result<SurfaceWatchSnapshot, SurfaceClientError> {
    match body {
        ResponseBody::SessionSurfaceWatching {
            session_id,
            input,
            status,
            ..
        } => Ok(SurfaceWatchSnapshot {
            session_id,
            input,
            status,
        }),
        ResponseBody::Error {
            code,
            message,
            retryable,
            data,
        } => Err(SurfaceClientError::Refused {
            code,
            message,
            retryable,
            data,
        }),
        _ => Err(SurfaceClientError::UnexpectedBody),
    }
}

pub fn input_inject_ack(body: ResponseBody) -> Result<SurfaceInjectAck, SurfaceClientError> {
    match body {
        ResponseBody::SessionInputInjectAck {
            session_id,
            delivered,
        } => Ok(SurfaceInjectAck {
            session_id,
            delivered,
        }),
        ResponseBody::Error {
            code,
            message,
            retryable,
            data,
        } => Err(SurfaceClientError::Refused {
            code,
            message,
            retryable,
            data,
        }),
        _ => Err(SurfaceClientError::UnexpectedBody),
    }
}

pub async fn session_surface_publish(
    client: &RpcClient,
    session_id: SessionId,
    input: Option<SurfaceInputPublishWire>,
    status: Option<SurfaceStatusPublishWire>,
) -> Result<SurfacePublishAck, SurfaceClientError> {
    let body = client
        .request(surface_publish_request(session_id, input, status))
        .await?;
    surface_publish_ack(body)
}

pub async fn session_surface_watch(
    client: &RpcClient,
    session_id: SessionId,
) -> Result<SurfaceWatchSnapshot, SurfaceClientError> {
    let body = client.request(surface_watch_request(session_id)).await?;
    surface_watch_snapshot(body)
}

pub async fn session_input_inject(
    client: &RpcClient,
    session_id: SessionId,
    op: SurfaceInjectOp,
) -> Result<SurfaceInjectAck, SurfaceClientError> {
    let body = client.request(input_inject_request(session_id, op)).await?;
    input_inject_ack(body)
}
