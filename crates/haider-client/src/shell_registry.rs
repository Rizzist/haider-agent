//! Feature-negotiated unified shell registry helpers.
//!
//! The constructor encodes the absence law: an older daemon makes this
//! surface absent instead of returning a synthetic feature error.

use haider_rpc::{
    ErrorData, FEATURE_SHELL_REGISTRY_V1, RequestBody, ResponseBody, ShellOutputStreamWire,
    ShellWire, Welcome, WireFrame,
};
use tokio::sync::mpsc;

use crate::client::{ClientError, RpcClient};

#[derive(Debug)]
pub enum ShellRegistryClientError {
    Client(ClientError),
    Refused {
        code: String,
        message: String,
        retryable: bool,
        data: Option<ErrorData>,
    },
    UnexpectedBody,
    EventsAlreadyTaken,
}

impl std::fmt::Display for ShellRegistryClientError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Client(error) => error.fmt(formatter),
            Self::Refused { code, message, .. } => write!(formatter, "{code}: {message}"),
            Self::UnexpectedBody => formatter.write_str("daemon answered with an unexpected body"),
            Self::EventsAlreadyTaken => {
                formatter.write_str("the connection event stream is already subscribed")
            }
        }
    }
}

impl std::error::Error for ShellRegistryClientError {}

impl From<ClientError> for ShellRegistryClientError {
    fn from(error: ClientError) -> Self {
        Self::Client(error)
    }
}

pub struct ShellRegistry<'a> {
    client: &'a RpcClient,
}

#[must_use]
pub fn shell_registry(client: &RpcClient) -> Option<ShellRegistry<'_>> {
    shell_registry_available(client.welcome()).then_some(ShellRegistry { client })
}

#[must_use]
pub fn shell_registry_available(welcome: &Welcome) -> bool {
    welcome.features.contains(FEATURE_SHELL_REGISTRY_V1)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellEvent {
    Opened(ShellWire),
    State(ShellWire),
    Closed(ShellWire),
    Output {
        id: String,
        stream: ShellOutputStreamWire,
        chunk_b64: haider_rpc::TerminalOutputWire,
    },
}

pub struct ShellEventSubscription {
    events: mpsc::Receiver<WireFrame>,
}

impl ShellEventSubscription {
    pub async fn next(&mut self) -> Option<ShellEvent> {
        while let Some(frame) = self.events.recv().await {
            if let Some(event) = shell_event_from_frame(frame) {
                return Some(event);
            }
        }
        None
    }
}

#[must_use]
pub fn shell_event_from_frame(frame: WireFrame) -> Option<ShellEvent> {
    match frame {
        WireFrame::ShellOpened { shell } => Some(ShellEvent::Opened(shell)),
        WireFrame::ShellState { shell } => Some(ShellEvent::State(shell)),
        WireFrame::ShellClosed { shell } => Some(ShellEvent::Closed(shell)),
        WireFrame::ShellOutput {
            id,
            stream,
            chunk_b64,
        } => Some(ShellEvent::Output {
            id,
            stream,
            chunk_b64,
        }),
        _ => None,
    }
}

impl ShellRegistry<'_> {
    pub async fn list(&self) -> Result<Vec<ShellWire>, ShellRegistryClientError> {
        match self.client.request(RequestBody::ShellList).await? {
            ResponseBody::ShellList { shells } => Ok(shells),
            body => response_error(body),
        }
    }

    pub async fn close(
        &self,
        id: impl Into<String>,
    ) -> Result<ShellWire, ShellRegistryClientError> {
        match self
            .client
            .request(RequestBody::ShellClose { id: id.into() })
            .await?
        {
            ResponseBody::ShellClose { shell } => Ok(shell),
            body => response_error(body),
        }
    }

    pub async fn subscribe(&self) -> Result<ShellEventSubscription, ShellRegistryClientError> {
        self.list().await?;
        self.client
            .take_events()
            .map(|events| ShellEventSubscription { events })
            .ok_or(ShellRegistryClientError::EventsAlreadyTaken)
    }
}

fn response_error<T>(body: ResponseBody) -> Result<T, ShellRegistryClientError> {
    match body {
        ResponseBody::Error {
            code,
            message,
            retryable,
            data,
        } => Err(ShellRegistryClientError::Refused {
            code,
            message,
            retryable,
            data,
        }),
        _ => Err(ShellRegistryClientError::UnexpectedBody),
    }
}
