//! Feature-negotiated peer messaging helpers.
//!
//! The absence law is encoded in the constructor: an older daemon produces
//! `None`, not a synthetic feature error. A present handle can list, send, and
//! take the connection's unsolicited event stream for typed peer events.

pub use haider_rpc::haider_protocol::peer::{
    PeerDelivery, PeerDeliveryReason, PeerDescriptor, PeerKind, PeerMessage, PeerReceipt,
    PeerSender, PeerState, PeerTrust,
};
use haider_rpc::{
    ErrorData, FEATURE_PEER_MESSAGING_V1, RequestBody, ResponseBody, Welcome, WireFrame,
};
use tokio::sync::mpsc;

use crate::client::{ClientError, RpcClient};

#[derive(Debug)]
pub enum PeerClientError {
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

impl std::fmt::Display for PeerClientError {
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

impl std::error::Error for PeerClientError {}

impl From<ClientError> for PeerClientError {
    fn from(error: ClientError) -> Self {
        Self::Client(error)
    }
}

/// The peer surface exists only after feature negotiation succeeds.
pub struct PeerMessaging<'a> {
    client: &'a RpcClient,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerEvent {
    Received(PeerMessage),
    DeliveryChanged(PeerReceipt),
}

pub struct PeerEventSubscription {
    events: mpsc::Receiver<WireFrame>,
}

impl PeerEventSubscription {
    /// Returns the next peer event, skipping unrelated connection events.
    pub async fn next(&mut self) -> Option<PeerEvent> {
        while let Some(frame) = self.events.recv().await {
            if let Some(event) = peer_event_from_frame(frame) {
                return Some(event);
            }
        }
        None
    }
}

/// Maps one unsolicited frame without manufacturing an event for unrelated
/// connection traffic.
#[must_use]
pub fn peer_event_from_frame(frame: WireFrame) -> Option<PeerEvent> {
    match frame {
        WireFrame::PeerMessageReceived { message } => Some(PeerEvent::Received(message)),
        WireFrame::PeerDeliveryChanged { receipt } => Some(PeerEvent::DeliveryChanged(receipt)),
        _ => None,
    }
}

/// Returns the typed surface only when the daemon advertises
/// `peer_messaging_v1`. Feature absence is ordinary negotiation, not failure.
#[must_use]
pub fn peer_messaging(client: &RpcClient) -> Option<PeerMessaging<'_>> {
    peer_messaging_available(client.welcome()).then_some(PeerMessaging { client })
}

/// Pure negotiation predicate used by embedders that retain the handshake.
#[must_use]
pub fn peer_messaging_available(welcome: &Welcome) -> bool {
    welcome.features.contains(FEATURE_PEER_MESSAGING_V1)
}

impl PeerMessaging<'_> {
    pub async fn list(&self) -> Result<Vec<PeerDescriptor>, PeerClientError> {
        peer_list_response(self.client.request(RequestBody::PeerList {}).await?)
    }

    pub async fn send(
        &self,
        to: impl Into<String>,
        message: impl Into<String>,
        summary: Option<String>,
    ) -> Result<PeerReceipt, PeerClientError> {
        peer_send_response(
            self.client
                .request(RequestBody::PeerSend {
                    to: to.into(),
                    message: message.into(),
                    summary,
                })
                .await?,
        )
    }

    pub async fn set_name(
        &self,
        name: impl Into<String>,
    ) -> Result<PeerDescriptor, PeerClientError> {
        peer_name_response(
            self.client
                .request(RequestBody::PeerName { name: name.into() })
                .await?,
        )
    }

    /// Enables peer events with an initial registry read, then takes the
    /// connection-wide unsolicited event receiver. Embedders that also
    /// consume non-peer events should multiplex one receiver themselves.
    pub async fn subscribe(&self) -> Result<PeerEventSubscription, PeerClientError> {
        self.list().await?;
        self.client
            .take_events()
            .map(|events| PeerEventSubscription { events })
            .ok_or(PeerClientError::EventsAlreadyTaken)
    }
}

pub fn peer_list_response(body: ResponseBody) -> Result<Vec<PeerDescriptor>, PeerClientError> {
    match body {
        ResponseBody::PeerList { agents } => Ok(agents),
        ResponseBody::Error {
            code,
            message,
            retryable,
            data,
        } => Err(PeerClientError::Refused {
            code,
            message,
            retryable,
            data,
        }),
        _ => Err(PeerClientError::UnexpectedBody),
    }
}

pub fn peer_send_response(body: ResponseBody) -> Result<PeerReceipt, PeerClientError> {
    match body {
        ResponseBody::PeerSend { receipt } => Ok(receipt),
        ResponseBody::Error {
            code,
            message,
            retryable,
            data,
        } => Err(PeerClientError::Refused {
            code,
            message,
            retryable,
            data,
        }),
        _ => Err(PeerClientError::UnexpectedBody),
    }
}

pub fn peer_name_response(body: ResponseBody) -> Result<PeerDescriptor, PeerClientError> {
    match body {
        ResponseBody::PeerName { agent } => Ok(agent),
        ResponseBody::Error {
            code,
            message,
            retryable,
            data,
        } => Err(PeerClientError::Refused {
            code,
            message,
            retryable,
            data,
        }),
        _ => Err(PeerClientError::UnexpectedBody),
    }
}
