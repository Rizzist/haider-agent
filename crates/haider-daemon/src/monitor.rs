//! Session-scoped event monitors.
//!
//! The subsystem has two intentionally narrow integration seams:
//!
//! - [`publish_sms_incoming`] accepts a validated transport event through an
//!   instance-owned [`MonitorSourceHub`]. It never accepts a session id; the
//!   durable registry alone chooses which sessions receive the event.
//! - [`MonitorDeliverySink`] receives a resolved session and bounded report.
//!   [`SessionMonitorDeliverySink`] is the default and wakes the existing
//!   turn/session engine rather than creating a parallel executor.
//!
//! Registry mutations are additive `PromptRender::Omit` journal facts. The
//! in-memory registry is only a projection, rebuilt on daemon startup and on
//! demand, so watches survive restart without a second persistence system.

use crate::session_hub::{HubStoreHandle, SessionHub, WeakSessionHub};
use async_trait::async_trait;
use haider_core::{StoreHandle as _, TurnAcceptCommand, TurnAdmissionDisposition};
use haider_protocol::DeliveryMode;
use haider_protocol::envelope::{
    EventEnvelope, PromptRender, RawEnvelope, RenderTargets, SCHEMA_VERSION,
};
use haider_protocol::ids::{AgentId, BranchId, EventId, RunId, SessionId};
use haider_protocol::tool::{BoundedResult, ToolResultStatus};
use haider_tools::{
    MonitorAction, MonitorFilter, MonitorFilterField, MonitorFilterOperator, MonitorLifetime,
    MonitorOccurrence, MonitorRequest, MonitorSource, MonitorSourceKind, ToolError, ToolResult,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, PoisonError, RwLock as StdRwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use tokio::task::JoinHandle;

/// Hard cap on durable active watches owned by one session.
pub const MAX_MONITORS_PER_SESSION: usize = 32;
/// At most one active report and one durable coalescing follow-up per monitor
/// can wait in the outbox, even if delivery is stalled.
pub const MAX_PENDING_MONITOR_REPORTS_PER_SESSION: usize = MAX_MONITORS_PER_SESSION * 2;
/// A burst waits this long so one owner notification can represent it.
pub const MONITOR_COALESCE_WINDOW: Duration = Duration::from_millis(250);
/// Bounded subscription queue per source consumer.
pub const MONITOR_SOURCE_QUEUE_CAPACITY: usize = 256;
/// A report retains at most this many event previews; the omitted count is
/// explicit when more matching events were coalesced.
pub const MAX_MONITOR_REPORT_EVENTS: usize = 16;
/// Sustained matches over this count in one window auto-stop the watch.
pub const MONITOR_RATE_LIMIT_MATCHES: u32 = 64;
pub const MONITOR_RATE_LIMIT_WINDOW: Duration = Duration::from_secs(60);

const MAX_SMS_ADDRESS_BYTES: usize = 1_024;
const MAX_SMS_BODY_BYTES: usize = 64 * 1024;
const MAX_REPORT_BODY_CHARS: usize = 4_096;
const MONITOR_DELIVERY_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(10);
const MONITOR_TRANSPORT_MIRROR_TIMEOUT: Duration = Duration::from_secs(2);
const MONITOR_DELIVERY_RETRY_MIN: Duration = Duration::from_secs(1);
const MONITOR_DELIVERY_RETRY_MAX: Duration = Duration::from_secs(30);

/// Transport-neutral SMS event accepted by the source seam.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SmsIncomingEvent {
    pub address: String,
    pub body: String,
    pub received_at_ms: i64,
}

/// Payload carried on the source hub. It remains free of session coordinates
/// so a transport can never choose which conversation gets woken.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MonitorEventPayload {
    Sms(SmsIncomingEvent),
    Process { line: String },
    File { payload: String },
    Poll { payload: String },
    Timer { fired_at_ms: u64 },
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum MonitorEventPayloadWire {
    Sms(SmsIncomingEvent),
    Process(MonitorProcessEvent),
    File(MonitorFileEvent),
    Poll(MonitorPollEvent),
    Timer(MonitorTimerEvent),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MonitorProcessEvent {
    line: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MonitorFileEvent {
    payload: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MonitorPollEvent {
    payload: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MonitorTimerEvent {
    fired_at_ms: u64,
}

impl<'de> Deserialize<'de> for MonitorEventPayload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(match MonitorEventPayloadWire::deserialize(deserializer)? {
            MonitorEventPayloadWire::Sms(event) => Self::Sms(event),
            MonitorEventPayloadWire::Process(event) => Self::Process { line: event.line },
            MonitorEventPayloadWire::File(event) => Self::File {
                payload: event.payload,
            },
            MonitorEventPayloadWire::Poll(event) => Self::Poll {
                payload: event.payload,
            },
            MonitorEventPayloadWire::Timer(event) => Self::Timer {
                fired_at_ms: event.fired_at_ms,
            },
        })
    }
}

impl MonitorEventPayload {
    fn source_kind(&self) -> MonitorSourceKind {
        match self {
            Self::Sms(_) => MonitorSourceKind::Sms,
            Self::Process { .. } => MonitorSourceKind::Process,
            Self::File { .. } => MonitorSourceKind::File,
            Self::Poll { .. } => MonitorSourceKind::Poll,
            Self::Timer { .. } => MonitorSourceKind::Timer,
        }
    }

    fn field(&self, field: MonitorFilterField) -> Option<&str> {
        match (self, field) {
            (Self::Sms(sms), MonitorFilterField::Address) => Some(&sms.address),
            (Self::Sms(sms), MonitorFilterField::Body) => Some(&sms.body),
            (Self::Process { line }, MonitorFilterField::Payload) => Some(line),
            (Self::File { payload }, MonitorFilterField::Payload)
            | (Self::Poll { payload }, MonitorFilterField::Payload) => Some(payload),
            _ => None,
        }
    }
}

/// One source observation. Sequence is assigned by the instance-owned hub.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorEvent {
    pub sequence: u64,
    pub observed_at_ms: u64,
    pub payload: MonitorEventPayload,
}

impl MonitorEvent {
    #[must_use]
    pub fn source_kind(&self) -> MonitorSourceKind {
        self.payload.source_kind()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorPublishReceipt {
    pub sequence: u64,
    pub subscriber_count: usize,
    pub saturated_subscribers: usize,
}

/// Typed monitor failures. Source publishers may log these without failing
/// their own transport protocol; registry and delivery callers preserve the
/// failure instead of panicking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MonitorError {
    InvalidEvent(String),
    SubscriptionClosed,
    Store(String),
    StoreUnavailable { message: String, retryable: bool },
    Delivery(String),
}

impl fmt::Display for MonitorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidEvent(message) => write!(formatter, "invalid monitor event: {message}"),
            Self::SubscriptionClosed => formatter.write_str("monitor source subscription closed"),
            Self::Store(message) => write!(formatter, "monitor store failure: {message}"),
            Self::StoreUnavailable { message, .. } => {
                write!(formatter, "monitor store failure: {message}")
            }
            Self::Delivery(message) => write!(formatter, "monitor delivery failure: {message}"),
        }
    }
}

impl std::error::Error for MonitorError {}

fn monitor_store_error(error: haider_protocol::error::HaiderError) -> MonitorError {
    MonitorError::StoreUnavailable {
        message: error.message,
        retryable: error.retryable,
    }
}

/// One bounded per-source subscription.
pub struct MonitorSubscription {
    receiver: mpsc::Receiver<MonitorEvent>,
    enqueued_sequence: Arc<AtomicU64>,
}

impl MonitorSubscription {
    pub async fn recv(&mut self) -> Result<MonitorEvent, MonitorError> {
        self.receiver
            .recv()
            .await
            .ok_or(MonitorError::SubscriptionClosed)
    }

    fn try_recv(&mut self) -> Result<MonitorEvent, mpsc::error::TryRecvError> {
        self.receiver.try_recv()
    }

    fn enqueued_sequence(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.enqueued_sequence)
    }
}

struct MonitorSourceSubscriber {
    sender: mpsc::Sender<MonitorEvent>,
    enqueued_sequence: Arc<AtomicU64>,
}

struct MonitorSourceHubInner {
    sequence: AtomicU64,
    subscribers: StdMutex<HashMap<MonitorSourceKind, Vec<MonitorSourceSubscriber>>>,
}

/// Instance-owned, bounded source fan-out. A daemon/profile gets one hub;
/// separate test profiles cannot leak SMS events into each other.
#[derive(Clone)]
pub struct MonitorSourceHub {
    inner: Arc<MonitorSourceHubInner>,
}

impl Default for MonitorSourceHub {
    fn default() -> Self {
        Self {
            inner: Arc::new(MonitorSourceHubInner {
                sequence: AtomicU64::new(0),
                subscribers: StdMutex::new(HashMap::new()),
            }),
        }
    }
}

impl MonitorSourceHub {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Last sequence allocated by this daemon instance. A registration keeps
    /// this cursor so events observed in the same wall-clock millisecond just
    /// before registration remain ineligible.
    #[must_use]
    pub fn current_sequence(&self) -> u64 {
        self.inner.sequence.load(Ordering::Acquire)
    }

    /// Subscribes to exactly one source family through a bounded queue.
    #[must_use]
    pub fn subscribe(&self, source: MonitorSourceKind) -> MonitorSubscription {
        let (sender, receiver) = mpsc::channel(MONITOR_SOURCE_QUEUE_CAPACITY);
        let mut subscribers = self
            .inner
            .subscribers
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let enqueued_sequence = Arc::new(AtomicU64::new(self.current_sequence()));
        subscribers
            .entry(source)
            .or_default()
            .push(MonitorSourceSubscriber {
                sender,
                enqueued_sequence: Arc::clone(&enqueued_sequence),
            });
        drop(subscribers);
        MonitorSubscription {
            receiver,
            enqueued_sequence,
        }
    }

    /// Publishes without awaiting a consumer. Full subscribers are reported
    /// honestly, never hidden behind an unbounded queue.
    pub fn publish(
        &self,
        payload: MonitorEventPayload,
    ) -> Result<MonitorPublishReceipt, MonitorError> {
        validate_event_payload(&payload)?;
        // Sequence allocation and bounded fan-out share one lock, making
        // every subscriber cursor a contiguous prefix even with concurrent
        // transport publishers.
        let mut subscribers = self
            .inner
            .subscribers
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let sequence = self
            .inner
            .sequence
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1);
        let event = MonitorEvent {
            sequence,
            observed_at_ms: now_ms(),
            payload,
        };
        let source_subscribers = subscribers.entry(event.source_kind()).or_default();
        let mut accepted = 0_usize;
        let mut saturated = 0_usize;
        source_subscribers.retain(
            |subscriber| match subscriber.sender.try_send(event.clone()) {
                Ok(()) => {
                    subscriber
                        .enqueued_sequence
                        .store(sequence, Ordering::Release);
                    accepted = accepted.saturating_add(1);
                    true
                }
                Err(mpsc::error::TrySendError::Full(_)) => {
                    saturated = saturated.saturating_add(1);
                    true
                }
                Err(mpsc::error::TrySendError::Closed(_)) => false,
            },
        );
        Ok(MonitorPublishReceipt {
            sequence,
            subscriber_count: accepted,
            saturated_subscribers: saturated,
        })
    }
}

/// The exact merge seam for the future `sms.incoming` call site. Call after
/// the transport's own size validation; a zero-subscriber receipt is valid.
pub fn publish_sms_incoming(
    hub: &MonitorSourceHub,
    address: &str,
    body: &str,
    received_at_ms: i64,
) -> Result<MonitorPublishReceipt, MonitorError> {
    hub.publish(MonitorEventPayload::Sms(SmsIncomingEvent {
        address: address.to_owned(),
        body: body.to_owned(),
        received_at_ms,
    }))
}

fn validate_event_payload(payload: &MonitorEventPayload) -> Result<(), MonitorError> {
    match payload {
        MonitorEventPayload::Sms(sms) => {
            if sms.address.len() > MAX_SMS_ADDRESS_BYTES {
                return Err(MonitorError::InvalidEvent(format!(
                    "SMS address exceeds {MAX_SMS_ADDRESS_BYTES} bytes"
                )));
            }
            if sms.body.len() > MAX_SMS_BODY_BYTES {
                return Err(MonitorError::InvalidEvent(format!(
                    "SMS body exceeds {MAX_SMS_BODY_BYTES} bytes"
                )));
            }
            if sms.received_at_ms < 0 {
                return Err(MonitorError::InvalidEvent(
                    "SMS received_at_ms must not be negative".into(),
                ));
            }
        }
        MonitorEventPayload::Process { line }
        | MonitorEventPayload::File { payload: line }
        | MonitorEventPayload::Poll { payload: line } => {
            if line.len() > MAX_SMS_BODY_BYTES {
                return Err(MonitorError::InvalidEvent(format!(
                    "source payload exceeds {MAX_SMS_BODY_BYTES} bytes"
                )));
            }
        }
        MonitorEventPayload::Timer { .. } => {}
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MonitorReportStatus {
    Matched,
    RateLimited,
    TimedOut,
}

/// A bounded wake/delivery record. Chat transports may implement
/// [`MonitorDeliverySink`] over this type; the default turns it into normal
/// session input and lets ordinary item deltas reach attachments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorReport {
    pub report_id: String,
    pub monitor_id: String,
    pub session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_id: Option<BranchId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<AgentId>,
    pub source: MonitorSourceKind,
    pub status: MonitorReportStatus,
    pub events: Vec<MonitorEvent>,
    pub coalesced_count: usize,
    pub omitted_count: usize,
    pub action: MonitorAction,
}

impl MonitorReport {
    /// Canonical bounded agent input. Full transport payloads never flow into
    /// the prompt unboundedly.
    #[must_use]
    pub fn prompt_text(&self) -> String {
        let mut previews = Vec::with_capacity(self.events.len());
        for event in &self.events {
            let payload = match &event.payload {
                MonitorEventPayload::Sms(sms) => json!({
                    "address": sms.address,
                    "body": bounded_chars(&sms.body, MAX_REPORT_BODY_CHARS),
                    "received_at_ms": sms.received_at_ms,
                }),
                MonitorEventPayload::Process { line }
                | MonitorEventPayload::File { payload: line }
                | MonitorEventPayload::Poll { payload: line } => json!({
                    "payload": bounded_chars(line, MAX_REPORT_BODY_CHARS),
                }),
                MonitorEventPayload::Timer { fired_at_ms } => json!({
                    "fired_at_ms": fired_at_ms,
                }),
            };
            previews.push(json!({
                "sequence": event.sequence,
                "observed_at_ms": event.observed_at_ms,
                "payload": payload,
            }));
        }
        let follow_up = self.action.follow_up.as_deref().unwrap_or("");
        json!({
            "type": "monitor_event",
            "monitor_id": self.monitor_id,
            "source": self.source,
            "status": self.status,
            "coalesced_count": self.coalesced_count,
            "omitted_count": self.omitted_count,
            "report_to_owner": self.action.report,
            "follow_up": follow_up,
            "events": previews,
        })
        .to_string()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorDeliveryReceipt {
    pub durable: bool,
    pub handed_off: bool,
    pub disposition: &'static str,
}

/// Delivery seam implemented by chat/transport layers at merge time. The
/// service always invokes its canonical normal-turn implementation first,
/// then mirrors the same report to an installed transport implementation.
/// A mirror failure never rolls back or suppresses the durable agent wake.
#[async_trait]
pub trait MonitorDeliverySink: Send + Sync {
    async fn deliver(
        &self,
        session: &SessionId,
        report: MonitorReport,
    ) -> Result<MonitorDeliveryReceipt, MonitorError>;
}

/// Default delivery: admit through the existing Subturn path. An active or
/// parked run receives the durable event directly; an idle session starts a
/// normal turn, and a session busy on another branch queues it.
pub struct SessionMonitorDeliverySink {
    hub: WeakSessionHub,
}

impl SessionMonitorDeliverySink {
    #[must_use]
    pub fn new(hub: &SessionHub) -> Self {
        Self {
            hub: hub.downgrade(),
        }
    }

    fn from_weak(hub: WeakSessionHub) -> Self {
        Self { hub }
    }
}

#[async_trait]
impl MonitorDeliverySink for SessionMonitorDeliverySink {
    async fn deliver(
        &self,
        session: &SessionId,
        report: MonitorReport,
    ) -> Result<MonitorDeliveryReceipt, MonitorError> {
        if &report.session_id != session {
            return Err(MonitorError::Delivery(
                "delivery session does not match monitor report owner".into(),
            ));
        }
        let hub = self
            .hub
            .upgrade()
            .ok_or_else(|| MonitorError::Delivery("session hub is no longer available".into()))?;
        hub.wake_monitor_report(report).await
    }
}

struct UnavailableMonitorDeliverySink;

#[async_trait]
impl MonitorDeliverySink for UnavailableMonitorDeliverySink {
    async fn deliver(
        &self,
        _session: &SessionId,
        _report: MonitorReport,
    ) -> Result<MonitorDeliveryReceipt, MonitorError> {
        Err(MonitorError::Delivery(
            "monitor delivery sink is not installed".into(),
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct MonitorRegistration {
    monitor_id: String,
    /// Origin fence: session forks copy historical envelopes. A copied
    /// registration must not become a live watch in the independent child.
    owner_session_id: SessionId,
    source: MonitorSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    filter: Option<MonitorFilter>,
    action: MonitorAction,
    occurrence: MonitorOccurrence,
    created_at_ms: u64,
    #[serde(default)]
    start_sequence: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expires_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    branch_id: Option<BranchId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent_id: Option<AgentId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum MonitorRemovalReason {
    Removed,
    OneShotComplete,
    TimedOut,
    RateLimited,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PendingMonitorReport {
    report: MonitorReport,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    terminal_reason: Option<MonitorRemovalReason>,
    /// Per-monitor durable FIFO key. It is independent of wall-clock motion
    /// and remains stable when the follow-up slot is revised.
    #[serde(default)]
    queue_order: u64,
    queued_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct StoredMonitorToolReceipt {
    request_digest: String,
    result: BoundedResult,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
enum StoredMonitorClientReceiptBody {
    Register {
        receipt: haider_rpc::MonitorRegisterReceiptWire,
    },
    Remove {
        receipt: haider_rpc::MonitorRemoveReceiptWire,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct StoredMonitorClientReceipt {
    request_digest: String,
    body: StoredMonitorClientReceiptBody,
}

enum MonitorClientReceiptReplay {
    Missing,
    Found {
        body: StoredMonitorClientReceiptBody,
        accepted_seq: u64,
    },
    Conflict,
}

#[derive(Clone)]
pub(crate) struct MonitorClientRegistrationRequest {
    pub command_id: haider_rpc::CommandId,
    pub session_id: SessionId,
    pub worker_generation: u64,
    pub source: haider_rpc::MonitorSourceWire,
    pub filter: Option<haider_rpc::MonitorFilterWire>,
    pub action: haider_rpc::MonitorActionWire,
    pub occurrence: haider_rpc::MonitorOccurrenceWire,
    pub lifetime: haider_rpc::MonitorLifetimeWire,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
// The `Monitor` prefix is part of each durable on-wire journal event tag.
#[allow(clippy::enum_variant_names)]
enum MonitorJournalEvent {
    MonitorRegistered {
        registration: MonitorRegistration,
    },
    MonitorRemoved {
        monitor_id: String,
        reason: MonitorRemovalReason,
        removed_at_ms: u64,
    },
    MonitorToolReceipt {
        operation_id: String,
        receipt: StoredMonitorToolReceipt,
    },
    MonitorClientReceipt {
        operation_id: String,
        receipt: StoredMonitorClientReceipt,
    },
    MonitorReportPending {
        pending: PendingMonitorReport,
    },
    MonitorReportDelivered {
        report_id: String,
        delivered_at_ms: u64,
    },
}

impl MonitorJournalEvent {
    fn from_value(value: &serde_json::Value) -> Option<Self> {
        serde_json::from_value(value.clone()).ok()
    }

    fn to_value(&self) -> Result<serde_json::Value, MonitorError> {
        serde_json::to_value(self)
            .map_err(|error| MonitorError::Store(format!("cannot encode monitor fact: {error}")))
    }
}

#[derive(Default)]
struct SessionMonitors {
    adopted: bool,
    monitors: BTreeMap<String, MonitorRegistration>,
    pending_reports: BTreeMap<String, PendingMonitorReport>,
}

#[derive(Default)]
struct MonitorRegistry {
    sessions: StdMutex<HashMap<SessionId, SessionMonitors>>,
}

impl MonitorRegistry {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<SessionId, SessionMonitors>> {
        self.sessions.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn is_adopted(&self, session: &SessionId) -> bool {
        self.lock().get(session).is_some_and(|state| state.adopted)
    }

    fn install(
        &self,
        session: SessionId,
        monitors: BTreeMap<String, MonitorRegistration>,
        pending_reports: BTreeMap<String, PendingMonitorReport>,
    ) {
        self.lock().insert(
            session,
            SessionMonitors {
                adopted: true,
                monitors,
                pending_reports,
            },
        );
    }

    fn snapshot(&self, session: &SessionId) -> Vec<MonitorRegistration> {
        self.lock()
            .get(session)
            .map_or_else(Vec::new, |state| state.monitors.values().cloned().collect())
    }

    fn all(&self) -> Vec<(SessionId, MonitorRegistration)> {
        self.lock()
            .iter()
            .flat_map(|(session, state)| {
                state
                    .monitors
                    .values()
                    .cloned()
                    .map(|registration| (session.clone(), registration))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    fn insert(&self, session: &SessionId, registration: MonitorRegistration) {
        self.lock()
            .entry(session.clone())
            .or_default()
            .monitors
            .insert(registration.monitor_id.clone(), registration);
    }

    fn get(&self, session: &SessionId, monitor_id: &str) -> Option<MonitorRegistration> {
        self.lock()
            .get(session)
            .and_then(|state| state.monitors.get(monitor_id))
            .cloned()
    }

    fn remove(&self, session: &SessionId, monitor_id: &str) -> Option<MonitorRegistration> {
        self.lock()
            .get_mut(session)
            .and_then(|state| state.monitors.remove(monitor_id))
    }

    fn pending_summary(&self, session: &SessionId, monitor_id: &str) -> (usize, bool) {
        self.lock().get(session).map_or((0, false), |state| {
            let matching = state
                .pending_reports
                .values()
                .filter(|pending| pending.report.monitor_id == monitor_id);
            matching.fold((0_usize, false), |(count, terminal), pending| {
                (
                    count.saturating_add(1),
                    terminal || pending.terminal_reason.is_some(),
                )
            })
        })
    }

    fn pending_for_monitor(
        &self,
        session: &SessionId,
        monitor_id: &str,
    ) -> Vec<PendingMonitorReport> {
        let mut pending = self.lock().get(session).map_or_else(Vec::new, |state| {
            state
                .pending_reports
                .values()
                .filter(|pending| pending.report.monitor_id == monitor_id)
                .cloned()
                .collect::<Vec<_>>()
        });
        pending.sort_by(|left, right| {
            left.queue_order
                .cmp(&right.queue_order)
                .then_with(|| left.queued_at_ms.cmp(&right.queued_at_ms))
                .then_with(|| left.report.report_id.cmp(&right.report.report_id))
        });
        pending
    }

    fn pending_count(&self, session: &SessionId) -> usize {
        self.lock()
            .get(session)
            .map_or(0, |state| state.pending_reports.len())
    }

    fn pending(&self, session: &SessionId, report_id: &str) -> Option<PendingMonitorReport> {
        self.lock()
            .get(session)
            .and_then(|state| state.pending_reports.get(report_id))
            .cloned()
    }

    fn insert_pending(&self, session: &SessionId, pending: PendingMonitorReport) {
        self.lock()
            .entry(session.clone())
            .or_default()
            .pending_reports
            .insert(pending.report.report_id.clone(), pending);
    }

    fn remove_pending(&self, session: &SessionId, report_id: &str) -> Option<PendingMonitorReport> {
        self.lock()
            .get_mut(session)
            .and_then(|state| state.pending_reports.remove(report_id))
    }

    fn forget_session(&self, session: &SessionId) {
        self.lock().remove(session);
    }
}

struct RateWindow {
    started: tokio::time::Instant,
    matches: u32,
}

struct TimeoutTask {
    token: u64,
    cancel: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

struct DeliveryTask {
    token: u64,
    cancel: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

struct EnqueueTask {
    token: u64,
    pending: Arc<StdMutex<EnqueueRetryQueue>>,
    cancel: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

#[derive(Clone)]
struct EnqueueRetryItem {
    registration: MonitorRegistration,
    report: MonitorReport,
    terminal_reason: Option<MonitorRemovalReason>,
    wait_for_source_sequence: Option<u64>,
}

#[derive(Default)]
struct EnqueueRetryQueue {
    items: VecDeque<EnqueueRetryItem>,
}

impl EnqueueRetryQueue {
    fn push(&mut self, item: EnqueueRetryItem) {
        if let Some(terminal_index) = self
            .items
            .iter()
            .position(|queued| queued.terminal_reason.is_some())
        {
            if item.terminal_reason.is_some() {
                if let Some(terminal) = self.items.get_mut(terminal_index)
                    && terminal.terminal_reason == Some(MonitorRemovalReason::TimedOut)
                    && item.terminal_reason != Some(MonitorRemovalReason::TimedOut)
                {
                    // A classified matching terminal (one-shot/rate limit)
                    // semantically precedes the timeout that was waiting on
                    // its source watermark. Preserve the event and upgrade
                    // the terminal reason in place.
                    coalesce_monitor_report(&mut terminal.report, item.report, true);
                    terminal.terminal_reason = item.terminal_reason;
                    terminal.wait_for_source_sequence = item.wait_for_source_sequence;
                }
                return;
            }
            if self.items.len() == 1 {
                self.items.push_front(item);
            } else if let Some(terminal) = self.items.get_mut(terminal_index) {
                // The active item is immutable while the worker may be
                // appending it. Fold a late eligible occurrence into the
                // terminal follow-up instead.
                coalesce_monitor_report(&mut terminal.report, item.report, false);
            }
            return;
        }
        if self.items.len() < 2 {
            self.items.push_back(item);
            return;
        }
        if let Some(follow_up) = self.items.back_mut() {
            coalesce_monitor_report(
                &mut follow_up.report,
                item.report,
                item.terminal_reason.is_some(),
            );
            if item.terminal_reason.is_some() {
                follow_up.terminal_reason = item.terminal_reason;
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReportQueueOutcome {
    Queued,
    AlreadyPending,
    CapacityReached,
    Inactive,
}

struct MonitorServiceInner {
    sources: MonitorSourceHub,
    registry: MonitorRegistry,
    mutations: Mutex<()>,
    rates: StdMutex<HashMap<(SessionId, String), RateWindow>>,
    /// Deletion tombstones fence late adoption/scheduling until daemon exit.
    /// Session ids are immutable identities and are never reused in-place.
    retired_sessions: StdMutex<HashSet<SessionId>>,
    /// Canonical wake sink; activation always installs the normal turn path.
    sink: StdRwLock<Arc<dyn MonitorDeliverySink>>,
    /// Optional transport mirror. It cannot replace or suppress agent wake.
    transport_sink: StdRwLock<Option<Arc<dyn MonitorDeliverySink>>>,
    runtime_tasks: StdMutex<Vec<JoinHandle<()>>>,
    timeout_tasks: StdMutex<HashMap<(SessionId, String), TimeoutTask>>,
    timeout_sequence: AtomicU64,
    delivery_tasks: StdMutex<HashMap<(SessionId, String), DeliveryTask>>,
    delivery_sequence: AtomicU64,
    enqueue_tasks: StdMutex<HashMap<(SessionId, String), EnqueueTask>>,
    enqueue_sequence: AtomicU64,
    sms_enqueued_sequence: StdRwLock<Option<Arc<AtomicU64>>>,
    sms_classified: watch::Sender<u64>,
    ready: watch::Sender<bool>,
    activated: AtomicBool,
    shutdown: watch::Sender<bool>,
}

/// Hub-owned monitor service. Clone is cheap and every clone shares the same
/// bounded registry, subscriptions, delivery sink, and mutation serializer.
#[derive(Clone)]
pub(crate) struct MonitorService {
    inner: Arc<MonitorServiceInner>,
}

impl Default for MonitorService {
    fn default() -> Self {
        let (shutdown, _) = watch::channel(false);
        let (sms_classified, _) = watch::channel(0);
        let (ready, _) = watch::channel(false);
        Self {
            inner: Arc::new(MonitorServiceInner {
                sources: MonitorSourceHub::new(),
                registry: MonitorRegistry::default(),
                mutations: Mutex::new(()),
                rates: StdMutex::new(HashMap::new()),
                retired_sessions: StdMutex::new(HashSet::new()),
                sink: StdRwLock::new(Arc::new(UnavailableMonitorDeliverySink)),
                transport_sink: StdRwLock::new(None),
                runtime_tasks: StdMutex::new(Vec::new()),
                timeout_tasks: StdMutex::new(HashMap::new()),
                timeout_sequence: AtomicU64::new(0),
                delivery_tasks: StdMutex::new(HashMap::new()),
                delivery_sequence: AtomicU64::new(0),
                enqueue_tasks: StdMutex::new(HashMap::new()),
                enqueue_sequence: AtomicU64::new(0),
                sms_enqueued_sequence: StdRwLock::new(None),
                sms_classified,
                ready,
                activated: AtomicBool::new(false),
                shutdown,
            }),
        }
    }
}

impl MonitorService {
    fn is_retired(&self, session: &SessionId) -> bool {
        self.inner
            .retired_sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(session)
    }

    pub(crate) fn activate(&self, hub: WeakSessionHub) {
        if *self.inner.shutdown.borrow() {
            return;
        }
        if self
            .inner
            .activated
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        *self
            .inner
            .sink
            .write()
            .unwrap_or_else(PoisonError::into_inner) =
            Arc::new(SessionMonitorDeliverySink::from_weak(hub.clone()));
        let mut subscription = self.inner.sources.subscribe(MonitorSourceKind::Sms);
        let enqueued_sequence = subscription.enqueued_sequence();
        let initial_sequence = enqueued_sequence.load(Ordering::Acquire);
        *self
            .inner
            .sms_enqueued_sequence
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Some(enqueued_sequence);
        self.inner.sms_classified.send_replace(initial_sequence);
        let mut shutdown = self.inner.shutdown.subscribe();
        let weak_service = Arc::downgrade(&self.inner);
        let event_hub = hub.clone();
        let event_task = tokio::spawn(async move {
            loop {
                let received = tokio::select! {
                    event = subscription.recv() => event,
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() {
                            return;
                        }
                        continue;
                    }
                };
                let first = match received {
                    Ok(event) => event,
                    Err(MonitorError::SubscriptionClosed) => return,
                    Err(error) => {
                        tracing::warn!(%error, "monitor source subscription failed");
                        return;
                    }
                };
                tokio::select! {
                    () = tokio::time::sleep(MONITOR_COALESCE_WINDOW) => {}
                    changed = shutdown.changed() => {
                        let _ = changed;
                        return;
                    }
                }
                let mut events = vec![first];
                while events.len() < MONITOR_SOURCE_QUEUE_CAPACITY {
                    match subscription.try_recv() {
                        Ok(event) => events.push(event),
                        Err(mpsc::error::TryRecvError::Empty) => break,
                        Err(mpsc::error::TryRecvError::Disconnected) => break,
                    }
                }
                let (Some(inner), Some(hub)) = (weak_service.upgrade(), event_hub.upgrade()) else {
                    return;
                };
                let service = MonitorService { inner };
                let classified_through = events
                    .iter()
                    .map(|event| event.sequence)
                    .max()
                    .unwrap_or(initial_sequence);
                let mut backoff = MONITOR_DELIVERY_RETRY_MIN;
                loop {
                    let result = tokio::select! {
                        result = service.dispatch_batch(&hub, events.clone()) => Some(result),
                        changed = shutdown.changed() => {
                            let _ = changed;
                            None
                        }
                    };
                    match result {
                        Some(Ok(())) => {
                            service.mark_sms_classified(classified_through);
                            break;
                        }
                        Some(Err(error)) => {
                            tracing::warn!(%error, "monitor source batch classification will retry");
                        }
                        None => return,
                    }
                    let retry = tokio::select! {
                        () = tokio::time::sleep(backoff) => true,
                        changed = shutdown.changed() => {
                            let _ = changed;
                            false
                        }
                    };
                    if !retry {
                        return;
                    }
                    backoff = std::cmp::min(backoff.saturating_mul(2), MONITOR_DELIVERY_RETRY_MAX);
                }
            }
        });
        let weak_service = Arc::downgrade(&self.inner);
        let boot_hub = hub;
        let mut boot_shutdown = self.inner.shutdown.subscribe();
        let boot_task = tokio::spawn(async move {
            let mut backoff = MONITOR_DELIVERY_RETRY_MIN;
            loop {
                let (Some(inner), Some(hub)) = (weak_service.upgrade(), boot_hub.upgrade()) else {
                    return;
                };
                let service = MonitorService { inner };
                let result = tokio::select! {
                    result = service.adopt_all(&hub) => Some(result),
                    changed = boot_shutdown.changed() => {
                        let _ = changed;
                        None
                    }
                };
                match result {
                    Some(Ok(())) => {
                        if let Some(inner) = weak_service.upgrade() {
                            inner.ready.send_replace(true);
                        }
                        return;
                    }
                    Some(Err(error)) => {
                        tracing::warn!(%error, "monitor registry startup adoption will retry");
                    }
                    None => return,
                }
                let retry = tokio::select! {
                    () = tokio::time::sleep(backoff) => true,
                    changed = boot_shutdown.changed() => {
                        let _ = changed;
                        false
                    }
                };
                if !retry {
                    return;
                }
                backoff = std::cmp::min(backoff.saturating_mul(2), MONITOR_DELIVERY_RETRY_MAX);
            }
        });
        self.inner
            .runtime_tasks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend([event_task, boot_task]);
    }

    pub(crate) fn source_hub(&self) -> MonitorSourceHub {
        self.inner.sources.clone()
    }

    pub(crate) async fn wait_ready(&self) {
        if *self.inner.ready.borrow() {
            return;
        }
        let mut ready = self.inner.ready.subscribe();
        while !*ready.borrow() {
            if ready.changed().await.is_err() {
                return;
            }
        }
    }

    pub(crate) fn install_delivery_sink(&self, sink: Arc<dyn MonitorDeliverySink>) {
        *self
            .inner
            .transport_sink
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Some(sink);
    }

    pub(crate) async fn shutdown(&self) -> Result<(), MonitorError> {
        self.inner.shutdown.send_replace(true);
        // Fence registry/outbox mutations that began before the shutdown bit.
        let mutation = self.inner.mutations.lock().await;
        drop(mutation);
        let timeout_tasks = {
            let mut tasks = self
                .inner
                .timeout_tasks
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            tasks.drain().map(|(_, task)| task).collect::<Vec<_>>()
        };
        let delivery_tasks = {
            let mut tasks = self
                .inner
                .delivery_tasks
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            tasks.drain().map(|(_, task)| task).collect::<Vec<_>>()
        };
        let enqueue_tasks = {
            let mut tasks = self
                .inner
                .enqueue_tasks
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            tasks.drain().map(|(_, task)| task).collect::<Vec<_>>()
        };
        let runtime_tasks = std::mem::take(
            &mut *self
                .inner
                .runtime_tasks
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        let mut first_join_error = None::<String>;
        for TimeoutTask { cancel, task, .. } in timeout_tasks {
            let _ = cancel.send(());
            if let Err(error) = task.await
                && first_join_error.is_none()
            {
                first_join_error = Some(format!("monitor timeout task failed: {error}"));
            }
        }
        for DeliveryTask { cancel, task, .. } in delivery_tasks {
            let _ = cancel.send(());
            if let Err(error) = task.await
                && first_join_error.is_none()
            {
                first_join_error = Some(format!("monitor delivery task failed: {error}"));
            }
        }
        for EnqueueTask { cancel, task, .. } in enqueue_tasks {
            let _ = cancel.send(());
            if let Err(error) = task.await
                && first_join_error.is_none()
            {
                first_join_error = Some(format!("monitor enqueue task failed: {error}"));
            }
        }
        for task in runtime_tasks {
            if let Err(error) = task.await
                && first_join_error.is_none()
            {
                first_join_error = Some(format!("monitor runtime task failed: {error}"));
            }
        }
        if let Some(error) = first_join_error {
            return Err(MonitorError::Delivery(error));
        }
        Ok(())
    }

    pub(crate) async fn forget_session(
        &self,
        hub: &SessionHub,
        session: &SessionId,
    ) -> Result<(), MonitorError> {
        let mutation = self.inner.mutations.lock().await;
        self.inner
            .retired_sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(session.clone());
        self.inner.registry.forget_session(session);
        self.inner
            .rates
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|(owner, _), _| owner != session);
        let timeout_tasks = {
            let mut tasks = self
                .inner
                .timeout_tasks
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let keys = tasks
                .keys()
                .filter(|(owner, _)| owner == session)
                .cloned()
                .collect::<Vec<_>>();
            keys.into_iter()
                .filter_map(|key| tasks.remove(&key))
                .collect::<Vec<_>>()
        };
        let delivery_tasks = {
            let mut tasks = self
                .inner
                .delivery_tasks
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let keys = tasks
                .keys()
                .filter(|(owner, _)| owner == session)
                .cloned()
                .collect::<Vec<_>>();
            keys.into_iter()
                .filter_map(|key| tasks.remove(&key))
                .collect::<Vec<_>>()
        };
        let enqueue_tasks = {
            let mut tasks = self
                .inner
                .enqueue_tasks
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let keys = tasks
                .keys()
                .filter(|(owner, _)| owner == session)
                .cloned()
                .collect::<Vec<_>>();
            keys.into_iter()
                .filter_map(|key| tasks.remove(&key))
                .collect::<Vec<_>>()
        };
        drop(mutation);
        let mut first_join_error = None::<String>;
        for TimeoutTask { cancel, task, .. } in timeout_tasks {
            let _ = cancel.send(());
            if let Err(error) = task.await
                && first_join_error.is_none()
            {
                first_join_error = Some(format!("monitor timeout task failed: {error}"));
            }
        }
        for DeliveryTask { cancel, task, .. } in delivery_tasks {
            let _ = cancel.send(());
            if let Err(error) = task.await
                && first_join_error.is_none()
            {
                first_join_error = Some(format!("monitor delivery task failed: {error}"));
            }
        }
        for EnqueueTask { cancel, task, .. } in enqueue_tasks {
            let _ = cancel.send(());
            if let Err(error) = task.await
                && first_join_error.is_none()
            {
                first_join_error = Some(format!("monitor enqueue task failed: {error}"));
            }
        }
        if let Some(error) = first_join_error {
            return match self.restore_session(hub, session).await {
                Ok(()) => Err(MonitorError::Delivery(error)),
                Err(restore) => Err(MonitorError::Delivery(format!(
                    "{error}; monitor deletion rollback failed: {restore}"
                ))),
            };
        }
        Ok(())
    }

    pub(crate) async fn restore_session(
        &self,
        hub: &SessionHub,
        session: &SessionId,
    ) -> Result<(), MonitorError> {
        self.release_session_tombstone(session);
        self.adopt_session(hub, session).await
    }

    pub(crate) fn release_session_tombstone(&self, session: &SessionId) {
        self.inner
            .retired_sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(session);
    }

    async fn adopt_all(&self, hub: &SessionHub) -> Result<(), MonitorError> {
        let sessions = hub
            .session_ids()
            .await
            .map_err(|error| MonitorError::Store(error.to_string()))?;
        let mut failure_count = 0_usize;
        let mut first_failure = None::<String>;
        for session in sessions {
            if let Err(error) = self.adopt_session(hub, &session).await {
                // One damaged/unavailable session must not starve monitors in
                // every later session. A later event retries adoption.
                tracing::warn!(%session, %error, "monitor registry adoption failed for session");
                failure_count = failure_count.saturating_add(1);
                if first_failure.is_none() {
                    first_failure = Some(format!("{session}: {error}"));
                }
            }
        }
        if failure_count == 0 {
            Ok(())
        } else {
            Err(MonitorError::Store(format!(
                "monitor adoption failed for {failure_count} session(s); first: {}",
                first_failure.unwrap_or_else(|| "unknown session failure".into())
            )))
        }
    }

    async fn adopt_session(
        &self,
        hub: &SessionHub,
        session: &SessionId,
    ) -> Result<(), MonitorError> {
        let _mutation = self.inner.mutations.lock().await;
        self.adopt_session_locked(hub, session).await
    }

    async fn adopt_session_locked(
        &self,
        hub: &SessionHub,
        session: &SessionId,
    ) -> Result<(), MonitorError> {
        if *self.inner.shutdown.borrow() || self.is_retired(session) {
            return Ok(());
        }
        if self.inner.registry.is_adopted(session) {
            return Ok(());
        }
        let mut cursor = 0_u64;
        let mut monitors = BTreeMap::<String, MonitorRegistration>::new();
        let mut pending_reports = BTreeMap::<String, PendingMonitorReport>::new();
        loop {
            let page = hub
                .read_internal_session(session, cursor, 256)
                .await
                .map_err(monitor_store_error)?;
            if page.is_empty() {
                break;
            }
            cursor = page.last().map_or(cursor, |envelope| envelope.seq);
            for envelope in page {
                match MonitorJournalEvent::from_value(&envelope.payload) {
                    Some(MonitorJournalEvent::MonitorRegistered { registration })
                        if registration.owner_session_id == *session =>
                    {
                        if monitors.contains_key(&registration.monitor_id)
                            || monitors.len() < MAX_MONITORS_PER_SESSION
                        {
                            monitors.insert(registration.monitor_id.clone(), registration);
                        } else {
                            tracing::warn!(%session, "ignored monitor registration beyond durable session cap");
                        }
                    }
                    // Session forks copy historical envelopes. The embedded
                    // origin fence keeps the copied fact historical-only.
                    Some(MonitorJournalEvent::MonitorRegistered { .. }) => {}
                    Some(MonitorJournalEvent::MonitorRemoved { monitor_id, .. }) => {
                        monitors.remove(&monitor_id);
                    }
                    Some(
                        MonitorJournalEvent::MonitorToolReceipt { .. }
                        | MonitorJournalEvent::MonitorClientReceipt { .. },
                    ) => {}
                    Some(MonitorJournalEvent::MonitorReportPending { pending })
                        if pending.report.session_id == *session =>
                    {
                        if let Some(existing) = pending_reports.get(&pending.report.report_id) {
                            // A repeated pending fact is the bounded
                            // follow-up slot being durably coalesced. Never
                            // permit a revision to move an id across watches.
                            if existing.report.monitor_id == pending.report.monitor_id {
                                pending_reports.insert(pending.report.report_id.clone(), pending);
                            }
                            continue;
                        }
                        let matching = pending_reports
                            .values()
                            .filter(|existing| {
                                existing.report.monitor_id == pending.report.monitor_id
                            })
                            .collect::<Vec<_>>();
                        let terminal_exists = matching
                            .iter()
                            .any(|existing| existing.terminal_reason.is_some());
                        if pending_reports.len() < MAX_PENDING_MONITOR_REPORTS_PER_SESSION
                            && matching.len() < 2
                            && !terminal_exists
                        {
                            pending_reports.insert(pending.report.report_id.clone(), pending);
                        }
                    }
                    Some(MonitorJournalEvent::MonitorReportPending { .. }) => {}
                    Some(MonitorJournalEvent::MonitorReportDelivered { report_id, .. }) => {
                        pending_reports.remove(&report_id);
                    }
                    None => {}
                }
            }
        }
        let scheduled = monitors.values().cloned().collect::<Vec<_>>();
        let pending = oldest_pending_per_monitor(&pending_reports);
        self.inner
            .registry
            .install(session.clone(), monitors, pending_reports);
        for registration in scheduled {
            self.schedule_timeout(hub.downgrade(), session.clone(), registration);
        }
        for report in pending {
            self.schedule_delivery(hub.downgrade(), session.clone(), report);
        }
        Ok(())
    }

    async fn persist_tool_receipt_locked(
        &self,
        store: &HubStoreHandle,
        coordinates: &MonitorToolCoordinates,
        operation_id: &str,
        request_digest: &str,
        result: &BoundedResult,
    ) -> ToolResult<()> {
        let receipt = StoredMonitorToolReceipt {
            request_digest: request_digest.to_owned(),
            result: result.clone(),
        };
        let fact = MonitorJournalEvent::MonitorToolReceipt {
            operation_id: operation_id.to_owned(),
            receipt: receipt.clone(),
        };
        let mut envelopes = [monitor_envelope(
            store.session_id(),
            None,
            coordinates.branch_id.as_ref(),
            coordinates.agent_id.as_ref(),
            &format!("monitor-receipt-{}", &operation_id[..24]),
            coordinates.device_id.clone(),
            store.worker_generation(),
            fact.to_value().map_err(monitor_tool_error)?,
        )];
        store
            .append(&mut envelopes)
            .await
            .map_err(|error| monitor_tool_error(monitor_store_error(error)))?;
        Ok(())
    }

    async fn replay_tool_receipt(
        &self,
        hub: &SessionHub,
        session: &SessionId,
        operation_id: &str,
        request_digest: &str,
    ) -> ToolResult<Option<BoundedResult>> {
        let mut cursor = 0_u64;
        loop {
            let page = hub
                .read_internal_session(session, cursor, 256)
                .await
                .map_err(|error| monitor_tool_error(monitor_store_error(error)))?;
            if page.is_empty() {
                return Ok(None);
            }
            cursor = page.last().map_or(cursor, |envelope| envelope.seq);
            for envelope in page {
                let Some(MonitorJournalEvent::MonitorToolReceipt {
                    operation_id: stored_operation,
                    receipt,
                }) = MonitorJournalEvent::from_value(&envelope.payload)
                else {
                    continue;
                };
                if stored_operation != operation_id {
                    continue;
                }
                if receipt.request_digest != request_digest {
                    return Err(ToolError::Runtime {
                        message: "monitor tool call replayed with different arguments".into(),
                    });
                }
                return Ok(Some(receipt.result));
            }
        }
    }

    async fn replay_client_receipt(
        &self,
        hub: &SessionHub,
        session: &SessionId,
        operation_id: &str,
        request_digest: &str,
    ) -> Result<MonitorClientReceiptReplay, MonitorError> {
        let mut cursor = 0_u64;
        loop {
            let page = hub
                .read_internal_session(session, cursor, 256)
                .await
                .map_err(monitor_store_error)?;
            if page.is_empty() {
                return Ok(MonitorClientReceiptReplay::Missing);
            }
            cursor = page.last().map_or(cursor, |envelope| envelope.seq);
            for envelope in page {
                let Some(MonitorJournalEvent::MonitorClientReceipt {
                    operation_id: stored_operation,
                    receipt,
                }) = MonitorJournalEvent::from_value(&envelope.payload)
                else {
                    continue;
                };
                if stored_operation != operation_id {
                    continue;
                }
                if receipt.request_digest != request_digest {
                    return Ok(MonitorClientReceiptReplay::Conflict);
                }
                return Ok(MonitorClientReceiptReplay::Found {
                    body: receipt.body,
                    accepted_seq: envelope.seq,
                });
            }
        }
    }

    async fn persist_client_receipt_locked(
        &self,
        hub: &SessionHub,
        session: &SessionId,
        branch_id: Option<&BranchId>,
        agent_id: Option<&AgentId>,
        operation_id: &str,
        receipt: StoredMonitorClientReceipt,
    ) -> Result<u64, MonitorError> {
        let fact = MonitorJournalEvent::MonitorClientReceipt {
            operation_id: operation_id.to_owned(),
            receipt,
        };
        let mut envelopes = [monitor_envelope(
            session,
            None,
            branch_id,
            agent_id,
            &format!("monitor-client-receipt-{}", &operation_id[..24]),
            hub.device_id(),
            hub.worker_generation(),
            fact.to_value()?,
        )];
        hub.append(&mut envelopes)
            .await
            .map_err(monitor_store_error)?;
        Ok(envelopes[0].seq)
    }

    async fn finalize_client_receipt(
        &self,
        hub: &SessionHub,
        command_id: &haider_rpc::CommandId,
        session_id: &SessionId,
        accepted_seq: u64,
        body: &StoredMonitorClientReceiptBody,
    ) -> Result<(), MonitorError> {
        let response = serde_json::to_value(body).map_err(|error| {
            MonitorError::Store(format!("cannot encode global monitor receipt: {error}"))
        })?;
        hub.finalize_monitor_control_receipt(command_id, session_id, accepted_seq, response)
            .await
            .map_err(monitor_store_error)
    }

    #[allow(clippy::too_many_arguments)]
    async fn persist_and_finalize_client_receipt_locked(
        &self,
        hub: &SessionHub,
        command_id: &haider_rpc::CommandId,
        session_id: &SessionId,
        operation_id: &str,
        request_digest: String,
        body: StoredMonitorClientReceiptBody,
    ) -> Result<(), MonitorError> {
        let accepted_seq = self
            .persist_client_receipt_locked(
                hub,
                session_id,
                None,
                None,
                operation_id,
                StoredMonitorClientReceipt {
                    request_digest,
                    body: body.clone(),
                },
            )
            .await?;
        self.finalize_client_receipt(hub, command_id, session_id, accepted_seq, &body)
            .await
    }

    pub(crate) async fn client_list(
        &self,
        hub: &SessionHub,
        session: SessionId,
    ) -> haider_rpc::MonitorListReceiptWire {
        if *self.inner.shutdown.borrow() || self.is_retired(&session) {
            return monitor_list_rejected(
                session,
                haider_rpc::MonitorControlRejectionWire::ServiceStopped,
            );
        }
        let _mutation = self.inner.mutations.lock().await;
        if *self.inner.shutdown.borrow() || self.is_retired(&session) {
            return monitor_list_rejected(
                session,
                haider_rpc::MonitorControlRejectionWire::ServiceStopped,
            );
        }
        match hub.latest_internal_session_seq(&session).await {
            Ok(0) => {
                return monitor_list_rejected(
                    session,
                    haider_rpc::MonitorControlRejectionWire::SessionNotFound,
                );
            }
            Ok(_) => {}
            Err(error) => {
                return monitor_list_rejected(session, monitor_store_haider_rejection(error));
            }
        }
        if let Err(error) = self.adopt_session_locked(hub, &session).await {
            return monitor_list_rejected(session, monitor_store_rejection(error));
        }
        let monitors = self
            .inner
            .registry
            .snapshot(&session)
            .iter()
            .map(monitor_registration_wire)
            .collect();
        haider_rpc::MonitorListReceiptWire {
            session_id: session,
            policy: monitor_control_policy(),
            sources: monitor_source_availability(),
            outcome: haider_rpc::MonitorListOutcomeWire::Listed { monitors },
        }
    }

    pub(crate) async fn client_register(
        &self,
        hub: &SessionHub,
        request: MonitorClientRegistrationRequest,
    ) -> haider_rpc::MonitorRegisterReceiptWire {
        let current_generation = hub.worker_generation();
        let MonitorClientRegistrationRequest {
            command_id,
            session_id,
            worker_generation,
            source,
            filter,
            action,
            occurrence,
            lifetime,
        } = request;
        let parsed = match monitor_register_request_from_wire(
            source, filter, action, occurrence, lifetime,
        ) {
            Ok(parsed) => parsed,
            Err(error) => {
                return monitor_register_rejected(
                    command_id,
                    session_id,
                    current_generation,
                    invalid_monitor_request(error),
                );
            }
        };
        if command_id.as_str().trim().is_empty() {
            return monitor_register_rejected(
                command_id,
                session_id,
                current_generation,
                haider_rpc::MonitorControlRejectionWire::InvalidRequest {
                    field: Some("command_id".into()),
                    detail: "command_id must not be empty".into(),
                },
            );
        }
        let request_value = json!({
            "session_id": session_id,
            "worker_generation": worker_generation,
            "request": parsed,
        });
        let request_json = match serde_json::to_string(&request_value) {
            Ok(json) => json,
            Err(error) => {
                return monitor_register_rejected(
                    command_id,
                    session_id,
                    current_generation,
                    haider_rpc::MonitorControlRejectionWire::InvalidRequest {
                        field: None,
                        detail: format!("cannot encode canonical monitor request: {error}"),
                    },
                );
            }
        };
        let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();
        let operation_id = stable_digest(&[
            session_id.as_str(),
            command_id.as_str(),
            "monitor-client-register",
        ]);

        match hub
            .monitor_control_receipt(
                &command_id,
                "monitor.register",
                &request_digest,
                &request_json,
            )
            .await
        {
            Ok(Some(response)) => {
                return match decode_client_receipt(response) {
                    Ok(StoredMonitorClientReceiptBody::Register { receipt }) => receipt,
                    Ok(StoredMonitorClientReceiptBody::Remove { .. }) => monitor_register_rejected(
                        command_id,
                        session_id,
                        current_generation,
                        haider_rpc::MonitorControlRejectionWire::CommandConflict,
                    ),
                    Err(error) => monitor_register_rejected(
                        command_id,
                        session_id,
                        current_generation,
                        monitor_store_rejection(error),
                    ),
                };
            }
            Ok(None) => {}
            Err(error) => {
                return monitor_register_rejected(
                    command_id,
                    session_id,
                    current_generation,
                    monitor_receipt_rejection(error),
                );
            }
        }
        match hub.latest_internal_session_seq(&session_id).await {
            Ok(0) => {
                return monitor_register_rejected(
                    command_id,
                    session_id,
                    current_generation,
                    haider_rpc::MonitorControlRejectionWire::SessionNotFound,
                );
            }
            Ok(_) => {}
            Err(error) => {
                return monitor_register_rejected(
                    command_id,
                    session_id,
                    current_generation,
                    monitor_store_haider_rejection(error),
                );
            }
        }

        if *self.inner.shutdown.borrow() || self.is_retired(&session_id) {
            return monitor_register_rejected(
                command_id,
                session_id,
                current_generation,
                haider_rpc::MonitorControlRejectionWire::ServiceStopped,
            );
        }
        let _mutation = self.inner.mutations.lock().await;
        if *self.inner.shutdown.borrow() || self.is_retired(&session_id) {
            return monitor_register_rejected(
                command_id,
                session_id,
                current_generation,
                haider_rpc::MonitorControlRejectionWire::ServiceStopped,
            );
        }
        match hub.latest_internal_session_seq(&session_id).await {
            Ok(0) => {
                return monitor_register_rejected(
                    command_id,
                    session_id,
                    current_generation,
                    haider_rpc::MonitorControlRejectionWire::SessionNotFound,
                );
            }
            Ok(_) => {}
            Err(error) => {
                return monitor_register_rejected(
                    command_id,
                    session_id,
                    current_generation,
                    monitor_store_haider_rejection(error),
                );
            }
        }
        if let Err(error) = self.adopt_session_locked(hub, &session_id).await {
            return monitor_register_rejected(
                command_id,
                session_id,
                current_generation,
                monitor_store_rejection(error),
            );
        }
        match self
            .replay_client_receipt(hub, &session_id, &operation_id, &request_digest)
            .await
        {
            Ok(MonitorClientReceiptReplay::Found {
                body: StoredMonitorClientReceiptBody::Register { receipt },
                accepted_seq,
            }) => {
                let body = StoredMonitorClientReceiptBody::Register {
                    receipt: receipt.clone(),
                };
                match hub
                    .claim_monitor_control_receipt(
                        &command_id,
                        "monitor.register",
                        &request_digest,
                        &request_json,
                    )
                    .await
                {
                    Ok(haider_core::MonitorControlClaim::Committed(response)) => {
                        return match decode_client_receipt(response) {
                            Ok(StoredMonitorClientReceiptBody::Register { receipt }) => receipt,
                            Ok(StoredMonitorClientReceiptBody::Remove { .. }) => {
                                monitor_register_rejected(
                                    command_id,
                                    session_id,
                                    current_generation,
                                    haider_rpc::MonitorControlRejectionWire::CommandConflict,
                                )
                            }
                            Err(error) => monitor_register_rejected(
                                command_id,
                                session_id,
                                current_generation,
                                monitor_store_rejection(error),
                            ),
                        };
                    }
                    Ok(
                        haider_core::MonitorControlClaim::Fresh
                        | haider_core::MonitorControlClaim::ResumePending,
                    ) => {}
                    Err(error) => {
                        return monitor_register_rejected(
                            command_id,
                            session_id,
                            current_generation,
                            monitor_receipt_rejection(error),
                        );
                    }
                }
                return match self
                    .finalize_client_receipt(hub, &command_id, &session_id, accepted_seq, &body)
                    .await
                {
                    Ok(()) => receipt,
                    Err(error) => monitor_register_rejected(
                        command_id,
                        session_id,
                        current_generation,
                        monitor_store_rejection(error),
                    ),
                };
            }
            Ok(MonitorClientReceiptReplay::Found { .. } | MonitorClientReceiptReplay::Conflict) => {
                return monitor_register_rejected(
                    command_id,
                    session_id,
                    current_generation,
                    haider_rpc::MonitorControlRejectionWire::CommandConflict,
                );
            }
            Ok(MonitorClientReceiptReplay::Missing) => {}
            Err(error) => {
                return monitor_register_rejected(
                    command_id,
                    session_id,
                    current_generation,
                    monitor_store_rejection(error),
                );
            }
        }
        if worker_generation != current_generation {
            return monitor_register_rejected(
                command_id,
                session_id,
                current_generation,
                haider_rpc::MonitorControlRejectionWire::StaleGeneration {
                    requested: worker_generation,
                    current: current_generation,
                },
            );
        }
        match hub
            .claim_monitor_control_receipt(
                &command_id,
                "monitor.register",
                &request_digest,
                &request_json,
            )
            .await
        {
            Ok(haider_core::MonitorControlClaim::Committed(response)) => {
                return match decode_client_receipt(response) {
                    Ok(StoredMonitorClientReceiptBody::Register { receipt }) => receipt,
                    Ok(StoredMonitorClientReceiptBody::Remove { .. }) => monitor_register_rejected(
                        command_id,
                        session_id,
                        current_generation,
                        haider_rpc::MonitorControlRejectionWire::CommandConflict,
                    ),
                    Err(error) => monitor_register_rejected(
                        command_id,
                        session_id,
                        current_generation,
                        monitor_store_rejection(error),
                    ),
                };
            }
            Ok(
                haider_core::MonitorControlClaim::Fresh
                | haider_core::MonitorControlClaim::ResumePending,
            ) => {}
            Err(error) => {
                return monitor_register_rejected(
                    command_id,
                    session_id,
                    current_generation,
                    monitor_receipt_rejection(error),
                );
            }
        }
        let MonitorRequest::Register {
            source,
            filter,
            action,
            occurrence,
            lifetime,
        } = parsed
        else {
            return monitor_register_rejected(
                command_id,
                session_id,
                current_generation,
                haider_rpc::MonitorControlRejectionWire::InvalidRequest {
                    field: None,
                    detail: "canonical monitor request was not register".into(),
                },
            );
        };

        if source.kind() != MonitorSourceKind::Sms {
            let receipt = monitor_register_rejected(
                command_id,
                session_id,
                current_generation,
                haider_rpc::MonitorControlRejectionWire::SourceUnavailable {
                    source: monitor_source_kind_wire(source.kind()),
                },
            );
            let body = StoredMonitorClientReceiptBody::Register {
                receipt: receipt.clone(),
            };
            return match self
                .persist_and_finalize_client_receipt_locked(
                    hub,
                    &receipt.command_id,
                    &receipt.session_id,
                    &operation_id,
                    request_digest,
                    body,
                )
                .await
            {
                Ok(()) => receipt,
                Err(error) => monitor_register_rejected(
                    receipt.command_id,
                    receipt.session_id,
                    current_generation,
                    monitor_store_rejection(error),
                ),
            };
        }
        let current = self.inner.registry.snapshot(&session_id);
        if current.len() >= MAX_MONITORS_PER_SESSION {
            let receipt = monitor_register_rejected(
                command_id,
                session_id,
                current_generation,
                haider_rpc::MonitorControlRejectionWire::LimitReached {
                    count: u32::try_from(current.len()).unwrap_or(u32::MAX),
                    limit: u32::try_from(MAX_MONITORS_PER_SESSION).unwrap_or(u32::MAX),
                },
            );
            let body = StoredMonitorClientReceiptBody::Register {
                receipt: receipt.clone(),
            };
            return match self
                .persist_and_finalize_client_receipt_locked(
                    hub,
                    &receipt.command_id,
                    &receipt.session_id,
                    &operation_id,
                    request_digest,
                    body,
                )
                .await
            {
                Ok(()) => receipt,
                Err(error) => monitor_register_rejected(
                    receipt.command_id,
                    receipt.session_id,
                    current_generation,
                    monitor_store_rejection(error),
                ),
            };
        }

        let created_at_ms = now_ms();
        let expires_at_ms = match lifetime {
            MonitorLifetime::Session => None,
            MonitorLifetime::Timeout { timeout_ms } => {
                Some(created_at_ms.saturating_add(timeout_ms))
            }
        };
        let registration = MonitorRegistration {
            monitor_id: format!("monitor-{}", &operation_id[..20]),
            owner_session_id: session_id.clone(),
            source,
            filter,
            action,
            occurrence,
            created_at_ms,
            start_sequence: self.inner.sources.current_sequence(),
            expires_at_ms,
            branch_id: None,
            agent_id: None,
        };
        let receipt = haider_rpc::MonitorRegisterReceiptWire {
            command_id,
            session_id: session_id.clone(),
            worker_generation: current_generation,
            policy: monitor_control_policy(),
            sources: monitor_source_availability(),
            outcome: haider_rpc::MonitorRegisterOutcomeWire::Registered {
                monitor: monitor_registration_wire(&registration),
            },
        };
        let registered = MonitorJournalEvent::MonitorRegistered {
            registration: registration.clone(),
        };
        let receipt_body = StoredMonitorClientReceiptBody::Register {
            receipt: receipt.clone(),
        };
        let stored = MonitorJournalEvent::MonitorClientReceipt {
            operation_id: operation_id.clone(),
            receipt: StoredMonitorClientReceipt {
                request_digest,
                body: receipt_body.clone(),
            },
        };
        let mut envelopes = [
            monitor_envelope(
                &session_id,
                None,
                registration.branch_id.as_ref(),
                registration.agent_id.as_ref(),
                &format!("monitor-client-registered-{}", &operation_id[..24]),
                hub.device_id(),
                current_generation,
                match registered.to_value() {
                    Ok(value) => value,
                    Err(error) => {
                        return monitor_register_rejected(
                            receipt.command_id,
                            receipt.session_id,
                            current_generation,
                            monitor_store_rejection(error),
                        );
                    }
                },
            ),
            monitor_envelope(
                &session_id,
                None,
                registration.branch_id.as_ref(),
                registration.agent_id.as_ref(),
                &format!("monitor-client-receipt-{}", &operation_id[..24]),
                hub.device_id(),
                current_generation,
                match stored.to_value() {
                    Ok(value) => value,
                    Err(error) => {
                        return monitor_register_rejected(
                            receipt.command_id,
                            receipt.session_id,
                            current_generation,
                            monitor_store_rejection(error),
                        );
                    }
                },
            ),
        ];
        if let Err(error) = hub.append(&mut envelopes).await {
            return monitor_register_rejected(
                receipt.command_id,
                receipt.session_id,
                current_generation,
                monitor_store_rejection(monitor_store_error(error)),
            );
        }
        self.inner
            .registry
            .insert(&session_id, registration.clone());
        let accepted_seq = envelopes[1].seq;
        self.schedule_timeout(hub.downgrade(), session_id, registration);
        match self
            .finalize_client_receipt(
                hub,
                &receipt.command_id,
                &receipt.session_id,
                accepted_seq,
                &receipt_body,
            )
            .await
        {
            Ok(()) => receipt,
            Err(error) => monitor_register_rejected(
                receipt.command_id,
                receipt.session_id,
                current_generation,
                monitor_store_rejection(error),
            ),
        }
    }

    pub(crate) async fn client_remove(
        &self,
        hub: &SessionHub,
        command_id: haider_rpc::CommandId,
        session_id: SessionId,
        worker_generation: u64,
        monitor_id: String,
    ) -> haider_rpc::MonitorRemoveReceiptWire {
        let current_generation = hub.worker_generation();
        let parsed = match MonitorRequest::from_tool_args(json!({
            "operation": "remove",
            "monitor_id": monitor_id,
        })) {
            Ok(parsed) => parsed,
            Err(error) => {
                return monitor_remove_rejected(
                    command_id,
                    session_id,
                    current_generation,
                    invalid_monitor_request(error),
                );
            }
        };
        if command_id.as_str().trim().is_empty() {
            return monitor_remove_rejected(
                command_id,
                session_id,
                current_generation,
                haider_rpc::MonitorControlRejectionWire::InvalidRequest {
                    field: Some("command_id".into()),
                    detail: "command_id must not be empty".into(),
                },
            );
        }
        let request_value = json!({
            "session_id": session_id,
            "worker_generation": worker_generation,
            "request": parsed,
        });
        let request_json = match serde_json::to_string(&request_value) {
            Ok(json) => json,
            Err(error) => {
                return monitor_remove_rejected(
                    command_id,
                    session_id,
                    current_generation,
                    haider_rpc::MonitorControlRejectionWire::InvalidRequest {
                        field: None,
                        detail: format!("cannot encode canonical monitor request: {error}"),
                    },
                );
            }
        };
        let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();
        let operation_id = stable_digest(&[
            session_id.as_str(),
            command_id.as_str(),
            "monitor-client-remove",
        ]);
        match hub
            .monitor_control_receipt(
                &command_id,
                "monitor.remove",
                &request_digest,
                &request_json,
            )
            .await
        {
            Ok(Some(response)) => {
                return match decode_client_receipt(response) {
                    Ok(StoredMonitorClientReceiptBody::Remove { receipt }) => receipt,
                    Ok(StoredMonitorClientReceiptBody::Register { .. }) => monitor_remove_rejected(
                        command_id,
                        session_id,
                        current_generation,
                        haider_rpc::MonitorControlRejectionWire::CommandConflict,
                    ),
                    Err(error) => monitor_remove_rejected(
                        command_id,
                        session_id,
                        current_generation,
                        monitor_store_rejection(error),
                    ),
                };
            }
            Ok(None) => {}
            Err(error) => {
                return monitor_remove_rejected(
                    command_id,
                    session_id,
                    current_generation,
                    monitor_receipt_rejection(error),
                );
            }
        }
        match hub.latest_internal_session_seq(&session_id).await {
            Ok(0) => {
                return monitor_remove_rejected(
                    command_id,
                    session_id,
                    current_generation,
                    haider_rpc::MonitorControlRejectionWire::SessionNotFound,
                );
            }
            Ok(_) => {}
            Err(error) => {
                return monitor_remove_rejected(
                    command_id,
                    session_id,
                    current_generation,
                    monitor_store_haider_rejection(error),
                );
            }
        }
        if *self.inner.shutdown.borrow() || self.is_retired(&session_id) {
            return monitor_remove_rejected(
                command_id,
                session_id,
                current_generation,
                haider_rpc::MonitorControlRejectionWire::ServiceStopped,
            );
        }
        let _mutation = self.inner.mutations.lock().await;
        if *self.inner.shutdown.borrow() || self.is_retired(&session_id) {
            return monitor_remove_rejected(
                command_id,
                session_id,
                current_generation,
                haider_rpc::MonitorControlRejectionWire::ServiceStopped,
            );
        }
        match hub.latest_internal_session_seq(&session_id).await {
            Ok(0) => {
                return monitor_remove_rejected(
                    command_id,
                    session_id,
                    current_generation,
                    haider_rpc::MonitorControlRejectionWire::SessionNotFound,
                );
            }
            Ok(_) => {}
            Err(error) => {
                return monitor_remove_rejected(
                    command_id,
                    session_id,
                    current_generation,
                    monitor_store_haider_rejection(error),
                );
            }
        }
        if let Err(error) = self.adopt_session_locked(hub, &session_id).await {
            return monitor_remove_rejected(
                command_id,
                session_id,
                current_generation,
                monitor_store_rejection(error),
            );
        }
        match self
            .replay_client_receipt(hub, &session_id, &operation_id, &request_digest)
            .await
        {
            Ok(MonitorClientReceiptReplay::Found {
                body: StoredMonitorClientReceiptBody::Remove { receipt },
                accepted_seq,
            }) => {
                let body = StoredMonitorClientReceiptBody::Remove {
                    receipt: receipt.clone(),
                };
                match hub
                    .claim_monitor_control_receipt(
                        &command_id,
                        "monitor.remove",
                        &request_digest,
                        &request_json,
                    )
                    .await
                {
                    Ok(haider_core::MonitorControlClaim::Committed(response)) => {
                        return match decode_client_receipt(response) {
                            Ok(StoredMonitorClientReceiptBody::Remove { receipt }) => receipt,
                            Ok(StoredMonitorClientReceiptBody::Register { .. }) => {
                                monitor_remove_rejected(
                                    command_id,
                                    session_id,
                                    current_generation,
                                    haider_rpc::MonitorControlRejectionWire::CommandConflict,
                                )
                            }
                            Err(error) => monitor_remove_rejected(
                                command_id,
                                session_id,
                                current_generation,
                                monitor_store_rejection(error),
                            ),
                        };
                    }
                    Ok(
                        haider_core::MonitorControlClaim::Fresh
                        | haider_core::MonitorControlClaim::ResumePending,
                    ) => {}
                    Err(error) => {
                        return monitor_remove_rejected(
                            command_id,
                            session_id,
                            current_generation,
                            monitor_receipt_rejection(error),
                        );
                    }
                }
                return match self
                    .finalize_client_receipt(hub, &command_id, &session_id, accepted_seq, &body)
                    .await
                {
                    Ok(()) => receipt,
                    Err(error) => monitor_remove_rejected(
                        command_id,
                        session_id,
                        current_generation,
                        monitor_store_rejection(error),
                    ),
                };
            }
            Ok(MonitorClientReceiptReplay::Found { .. } | MonitorClientReceiptReplay::Conflict) => {
                return monitor_remove_rejected(
                    command_id,
                    session_id,
                    current_generation,
                    haider_rpc::MonitorControlRejectionWire::CommandConflict,
                );
            }
            Ok(MonitorClientReceiptReplay::Missing) => {}
            Err(error) => {
                return monitor_remove_rejected(
                    command_id,
                    session_id,
                    current_generation,
                    monitor_store_rejection(error),
                );
            }
        }
        if worker_generation != current_generation {
            return monitor_remove_rejected(
                command_id,
                session_id,
                current_generation,
                haider_rpc::MonitorControlRejectionWire::StaleGeneration {
                    requested: worker_generation,
                    current: current_generation,
                },
            );
        }
        match hub
            .claim_monitor_control_receipt(
                &command_id,
                "monitor.remove",
                &request_digest,
                &request_json,
            )
            .await
        {
            Ok(haider_core::MonitorControlClaim::Committed(response)) => {
                return match decode_client_receipt(response) {
                    Ok(StoredMonitorClientReceiptBody::Remove { receipt }) => receipt,
                    Ok(StoredMonitorClientReceiptBody::Register { .. }) => monitor_remove_rejected(
                        command_id,
                        session_id,
                        current_generation,
                        haider_rpc::MonitorControlRejectionWire::CommandConflict,
                    ),
                    Err(error) => monitor_remove_rejected(
                        command_id,
                        session_id,
                        current_generation,
                        monitor_store_rejection(error),
                    ),
                };
            }
            Ok(
                haider_core::MonitorControlClaim::Fresh
                | haider_core::MonitorControlClaim::ResumePending,
            ) => {}
            Err(error) => {
                return monitor_remove_rejected(
                    command_id,
                    session_id,
                    current_generation,
                    monitor_receipt_rejection(error),
                );
            }
        }
        let MonitorRequest::Remove { monitor_id } = parsed else {
            return monitor_remove_rejected(
                command_id,
                session_id,
                current_generation,
                haider_rpc::MonitorControlRejectionWire::InvalidRequest {
                    field: None,
                    detail: "canonical monitor request was not remove".into(),
                },
            );
        };
        let Some(registration) = self.inner.registry.get(&session_id, &monitor_id) else {
            let receipt = monitor_remove_rejected(
                command_id,
                session_id,
                current_generation,
                haider_rpc::MonitorControlRejectionWire::NotFound { monitor_id },
            );
            let body = StoredMonitorClientReceiptBody::Remove {
                receipt: receipt.clone(),
            };
            return match self
                .persist_and_finalize_client_receipt_locked(
                    hub,
                    &receipt.command_id,
                    &receipt.session_id,
                    &operation_id,
                    request_digest,
                    body,
                )
                .await
            {
                Ok(()) => receipt,
                Err(error) => monitor_remove_rejected(
                    receipt.command_id,
                    receipt.session_id,
                    current_generation,
                    monitor_store_rejection(error),
                ),
            };
        };
        let receipt = haider_rpc::MonitorRemoveReceiptWire {
            command_id,
            session_id: session_id.clone(),
            worker_generation: current_generation,
            policy: monitor_control_policy(),
            sources: monitor_source_availability(),
            outcome: haider_rpc::MonitorRemoveOutcomeWire::Removed {
                monitor_id: monitor_id.clone(),
            },
        };
        let removed = MonitorJournalEvent::MonitorRemoved {
            monitor_id: monitor_id.clone(),
            reason: MonitorRemovalReason::Removed,
            removed_at_ms: now_ms(),
        };
        let receipt_body = StoredMonitorClientReceiptBody::Remove {
            receipt: receipt.clone(),
        };
        let stored = MonitorJournalEvent::MonitorClientReceipt {
            operation_id: operation_id.clone(),
            receipt: StoredMonitorClientReceipt {
                request_digest,
                body: receipt_body.clone(),
            },
        };
        let mut envelopes = [
            monitor_envelope(
                &session_id,
                None,
                registration.branch_id.as_ref(),
                registration.agent_id.as_ref(),
                &format!("monitor-client-removed-{}", &operation_id[..24]),
                hub.device_id(),
                current_generation,
                match removed.to_value() {
                    Ok(value) => value,
                    Err(error) => {
                        return monitor_remove_rejected(
                            receipt.command_id,
                            receipt.session_id,
                            current_generation,
                            monitor_store_rejection(error),
                        );
                    }
                },
            ),
            monitor_envelope(
                &session_id,
                None,
                registration.branch_id.as_ref(),
                registration.agent_id.as_ref(),
                &format!("monitor-client-receipt-{}", &operation_id[..24]),
                hub.device_id(),
                current_generation,
                match stored.to_value() {
                    Ok(value) => value,
                    Err(error) => {
                        return monitor_remove_rejected(
                            receipt.command_id,
                            receipt.session_id,
                            current_generation,
                            monitor_store_rejection(error),
                        );
                    }
                },
            ),
        ];
        if let Err(error) = hub.append(&mut envelopes).await {
            return monitor_remove_rejected(
                receipt.command_id,
                receipt.session_id,
                current_generation,
                monitor_store_rejection(monitor_store_error(error)),
            );
        }
        self.inner.registry.remove(&session_id, &monitor_id);
        self.clear_rate(&session_id, &monitor_id);
        self.cancel_timeout(&session_id, &monitor_id);
        self.cancel_enqueue(&session_id, &monitor_id);
        match self
            .finalize_client_receipt(
                hub,
                &receipt.command_id,
                &receipt.session_id,
                envelopes[1].seq,
                &receipt_body,
            )
            .await
        {
            Ok(()) => receipt,
            Err(error) => monitor_remove_rejected(
                receipt.command_id,
                receipt.session_id,
                current_generation,
                monitor_store_rejection(error),
            ),
        }
    }

    pub(crate) async fn execute_tool(
        &self,
        hub: &SessionHub,
        store: &HubStoreHandle,
        coordinates: MonitorToolCoordinates,
        request: MonitorRequest,
    ) -> ToolResult<BoundedResult> {
        if *self.inner.shutdown.borrow() || self.is_retired(store.session_id()) {
            return Err(ToolError::Runtime {
                message: "monitor service is stopped for this session".into(),
            });
        }
        let _mutation = self.inner.mutations.lock().await;
        if *self.inner.shutdown.borrow() || self.is_retired(store.session_id()) {
            return Err(ToolError::Runtime {
                message: "monitor service is stopped for this session".into(),
            });
        }
        self.adopt_session_locked(hub, store.session_id())
            .await
            .map_err(monitor_tool_error)?;
        let request_bytes = serde_json::to_vec(&request).map_err(|error| ToolError::Runtime {
            message: format!("cannot encode monitor tool request: {error}"),
        })?;
        let request_digest = blake3::hash(&request_bytes).to_hex().to_string();
        let operation_id = stable_digest(&[
            store.session_id().as_str(),
            coordinates.run_id.as_str(),
            &coordinates.call_id,
            "monitor-tool",
        ]);
        match request {
            MonitorRequest::Register {
                source,
                filter,
                action,
                occurrence,
                lifetime,
            } => {
                if let Some(result) = self
                    .replay_tool_receipt(hub, store.session_id(), &operation_id, &request_digest)
                    .await?
                {
                    return Ok(result);
                }
                if source.kind() != MonitorSourceKind::Sms {
                    let result = tool_result(
                        json!({
                            "status": "unsupported_source",
                            "source": source.kind(),
                            "message": "this daemon activates SMS monitors first; the source adapter is typed but not yet active",
                        }),
                        ToolResultStatus::Rejected,
                        Some("monitor source adapter is not active".into()),
                    );
                    self.persist_tool_receipt_locked(
                        store,
                        &coordinates,
                        &operation_id,
                        &request_digest,
                        &result,
                    )
                    .await?;
                    return Ok(result);
                }
                let current = self.inner.registry.snapshot(store.session_id());
                if current.len() >= MAX_MONITORS_PER_SESSION {
                    let result = tool_result(
                        json!({
                            "status": "limit_reached",
                            "count": current.len(),
                            "limit": MAX_MONITORS_PER_SESSION,
                        }),
                        ToolResultStatus::Rejected,
                        Some("session monitor limit reached".into()),
                    );
                    self.persist_tool_receipt_locked(
                        store,
                        &coordinates,
                        &operation_id,
                        &request_digest,
                        &result,
                    )
                    .await?;
                    return Ok(result);
                }
                let monitor_id = format!("monitor-{}", &operation_id[..20]);
                let created_at_ms = now_ms();
                let expires_at_ms = match lifetime {
                    MonitorLifetime::Session => None,
                    MonitorLifetime::Timeout { timeout_ms } => {
                        Some(created_at_ms.saturating_add(timeout_ms))
                    }
                };
                let registration = MonitorRegistration {
                    monitor_id: monitor_id.clone(),
                    owner_session_id: store.session_id().clone(),
                    source,
                    filter,
                    action,
                    occurrence,
                    created_at_ms,
                    start_sequence: self.inner.sources.current_sequence(),
                    expires_at_ms,
                    branch_id: coordinates.branch_id.clone(),
                    agent_id: coordinates.agent_id.clone(),
                };
                let fact = MonitorJournalEvent::MonitorRegistered {
                    registration: registration.clone(),
                };
                let result = tool_result(
                    json!({
                        "status": "registered",
                        "monitor_id": monitor_id,
                        "source": MonitorSourceKind::Sms,
                        "occurrence": occurrence,
                        "expires_at_ms": expires_at_ms,
                    }),
                    ToolResultStatus::Completed,
                    None,
                );
                let receipt = MonitorJournalEvent::MonitorToolReceipt {
                    operation_id: operation_id.clone(),
                    receipt: StoredMonitorToolReceipt {
                        request_digest: request_digest.clone(),
                        result: result.clone(),
                    },
                };
                let mut envelopes = [
                    monitor_envelope(
                        store.session_id(),
                        None,
                        coordinates.branch_id.as_ref(),
                        coordinates.agent_id.as_ref(),
                        &format!("monitor-registered-{}", &operation_id[..24]),
                        coordinates.device_id.clone(),
                        store.worker_generation(),
                        fact.to_value().map_err(monitor_tool_error)?,
                    ),
                    monitor_envelope(
                        store.session_id(),
                        None,
                        coordinates.branch_id.as_ref(),
                        coordinates.agent_id.as_ref(),
                        &format!("monitor-receipt-{}", &operation_id[..24]),
                        coordinates.device_id,
                        store.worker_generation(),
                        receipt.to_value().map_err(monitor_tool_error)?,
                    ),
                ];
                store
                    .append(&mut envelopes)
                    .await
                    .map_err(|error| monitor_tool_error(monitor_store_error(error)))?;
                self.inner
                    .registry
                    .insert(store.session_id(), registration.clone());
                self.schedule_timeout(hub.downgrade(), store.session_id().clone(), registration);
                Ok(result)
            }
            MonitorRequest::List => {
                let registrations = self.inner.registry.snapshot(store.session_id());
                let monitors = registrations
                    .iter()
                    .map(|registration| {
                        json!({
                            "monitor_id": registration.monitor_id,
                            "source": registration.source,
                            "filter": registration.filter,
                            "action": registration.action,
                            "occurrence": registration.occurrence,
                            "created_at_ms": registration.created_at_ms,
                            "expires_at_ms": registration.expires_at_ms,
                        })
                    })
                    .collect::<Vec<_>>();
                Ok(tool_result(
                    json!({"count": monitors.len(), "monitors": monitors}),
                    ToolResultStatus::Completed,
                    None,
                ))
            }
            MonitorRequest::Remove { monitor_id } => {
                if let Some(result) = self
                    .replay_tool_receipt(hub, store.session_id(), &operation_id, &request_digest)
                    .await?
                {
                    return Ok(result);
                }
                if self
                    .inner
                    .registry
                    .get(store.session_id(), &monitor_id)
                    .is_none()
                {
                    let result = tool_result(
                        json!({"status": "not_found", "monitor_id": monitor_id}),
                        ToolResultStatus::Rejected,
                        Some("monitor was not found".into()),
                    );
                    self.persist_tool_receipt_locked(
                        store,
                        &coordinates,
                        &operation_id,
                        &request_digest,
                        &result,
                    )
                    .await?;
                    return Ok(result);
                }
                let fact = MonitorJournalEvent::MonitorRemoved {
                    monitor_id: monitor_id.clone(),
                    reason: MonitorRemovalReason::Removed,
                    removed_at_ms: now_ms(),
                };
                let result = tool_result(
                    json!({"status": "removed", "monitor_id": monitor_id}),
                    ToolResultStatus::Completed,
                    None,
                );
                let receipt = MonitorJournalEvent::MonitorToolReceipt {
                    operation_id: operation_id.clone(),
                    receipt: StoredMonitorToolReceipt {
                        request_digest: request_digest.clone(),
                        result: result.clone(),
                    },
                };
                let mut envelopes = [
                    monitor_envelope(
                        store.session_id(),
                        None,
                        coordinates.branch_id.as_ref(),
                        coordinates.agent_id.as_ref(),
                        &format!("monitor-removed-{}", &operation_id[..24]),
                        coordinates.device_id.clone(),
                        store.worker_generation(),
                        fact.to_value().map_err(monitor_tool_error)?,
                    ),
                    monitor_envelope(
                        store.session_id(),
                        None,
                        coordinates.branch_id.as_ref(),
                        coordinates.agent_id.as_ref(),
                        &format!("monitor-receipt-{}", &operation_id[..24]),
                        coordinates.device_id,
                        store.worker_generation(),
                        receipt.to_value().map_err(monitor_tool_error)?,
                    ),
                ];
                store
                    .append(&mut envelopes)
                    .await
                    .map_err(|error| monitor_tool_error(monitor_store_error(error)))?;
                self.inner.registry.remove(store.session_id(), &monitor_id);
                self.clear_rate(store.session_id(), &monitor_id);
                self.cancel_timeout(store.session_id(), &monitor_id);
                self.cancel_enqueue(store.session_id(), &monitor_id);
                Ok(result)
            }
        }
    }

    async fn dispatch_batch(
        &self,
        hub: &SessionHub,
        events: Vec<MonitorEvent>,
    ) -> Result<(), MonitorError> {
        if events.is_empty() {
            return Ok(());
        }
        self.adopt_all(hub).await?;
        let source = events[0].source_kind();
        for (session, registration) in self.inner.registry.all() {
            if registration.source.kind() != source {
                continue;
            }
            let matching = events
                .iter()
                .filter(|event| monitor_matches(&registration, event))
                .cloned()
                .collect::<Vec<_>>();
            if matching.is_empty() {
                continue;
            }
            self.stage_matching_report(hub, &session, &registration, matching)
                .await;
        }
        Ok(())
    }

    fn mark_sms_classified(&self, sequence: u64) {
        if sequence > *self.inner.sms_classified.borrow() {
            self.inner.sms_classified.send_replace(sequence);
        }
    }

    fn sms_enqueue_watermark(&self) -> u64 {
        let cursor = self
            .inner
            .sms_enqueued_sequence
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let Some(cursor) = cursor else {
            return *self.inner.sms_classified.borrow();
        };
        // Linearize timeout capture with publication. `publish` holds this
        // same lock across try_send + cursor store, so a timeout can observe
        // neither a visible event with the old cursor nor a future cursor.
        let _publish = self
            .inner
            .sources
            .inner
            .subscribers
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        cursor.load(Ordering::Acquire)
    }

    async fn stage_matching_report(
        &self,
        hub: &SessionHub,
        session: &SessionId,
        registration: &MonitorRegistration,
        matching: Vec<MonitorEvent>,
    ) {
        let _mutation = self.inner.mutations.lock().await;
        if *self.inner.shutdown.borrow()
            || self.is_retired(session)
            || self
                .inner
                .registry
                .get(session, &registration.monitor_id)
                .as_ref()
                != Some(registration)
        {
            return;
        }
        let rate_limited = self.record_matches(
            session,
            &registration.monitor_id,
            u32::try_from(matching.len()).unwrap_or(u32::MAX),
        );
        let status = if rate_limited {
            MonitorReportStatus::RateLimited
        } else {
            MonitorReportStatus::Matched
        };
        let report = build_report(hub, session, registration, status, matching);
        let terminal_reason = if rate_limited {
            Some(MonitorRemovalReason::RateLimited)
        } else if registration.occurrence == MonitorOccurrence::Once {
            Some(MonitorRemovalReason::OneShotComplete)
        } else {
            None
        };
        self.schedule_enqueue_retry(
            hub.downgrade(),
            session.clone(),
            registration.clone(),
            report,
            terminal_reason,
        );
        if terminal_reason.is_none()
            && registration
                .expires_at_ms
                .is_some_and(|expires| expires <= now_ms())
        {
            // Event-time eligibility is decided before this method. If the
            // coalescing window crossed the deadline, retain the match first
            // and stage timeout as its terminal follow-up.
            let timeout = build_report(
                hub,
                session,
                registration,
                MonitorReportStatus::TimedOut,
                Vec::new(),
            );
            self.schedule_enqueue_retry(
                hub.downgrade(),
                session.clone(),
                registration.clone(),
                timeout,
                Some(MonitorRemovalReason::TimedOut),
            );
        }
    }

    fn record_matches(&self, session: &SessionId, monitor_id: &str, count: u32) -> bool {
        let mut rates = self
            .inner
            .rates
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let window = rates
            .entry((session.clone(), monitor_id.to_owned()))
            .or_insert_with(|| RateWindow {
                started: tokio::time::Instant::now(),
                matches: 0,
            });
        if window.started.elapsed() >= MONITOR_RATE_LIMIT_WINDOW {
            window.started = tokio::time::Instant::now();
            window.matches = 0;
        }
        window.matches = window.matches.saturating_add(count);
        window.matches > MONITOR_RATE_LIMIT_MATCHES
    }

    fn clear_rate(&self, session: &SessionId, monitor_id: &str) {
        self.inner
            .rates
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&(session.clone(), monitor_id.to_owned()));
    }

    async fn queue_report(
        &self,
        hub: &SessionHub,
        session: &SessionId,
        registration: &MonitorRegistration,
        report: MonitorReport,
        terminal_reason: Option<MonitorRemovalReason>,
    ) -> Result<ReportQueueOutcome, MonitorError> {
        let mutation = self.inner.mutations.lock().await;
        if *self.inner.shutdown.borrow() || self.is_retired(session) {
            return Ok(ReportQueueOutcome::Inactive);
        }
        if self
            .inner
            .registry
            .get(session, &registration.monitor_id)
            .as_ref()
            != Some(registration)
        {
            return Ok(ReportQueueOutcome::Inactive);
        }
        if self
            .inner
            .registry
            .pending(session, &report.report_id)
            .is_some()
        {
            return Ok(ReportQueueOutcome::AlreadyPending);
        }
        let mut matching = self
            .inner
            .registry
            .pending_for_monitor(session, &registration.monitor_id);
        if matching
            .iter()
            .any(|pending| pending.terminal_reason.is_some())
        {
            return Ok(ReportQueueOutcome::AlreadyPending);
        }
        let start_delivery = matching.is_empty();
        let pending = if matching.len() < 2 {
            if self.inner.registry.pending_count(session) >= MAX_PENDING_MONITOR_REPORTS_PER_SESSION
            {
                return Ok(ReportQueueOutcome::CapacityReached);
            }
            let queue_order = matching.last().map_or(Ok(0), |predecessor| {
                predecessor.queue_order.checked_add(1).ok_or_else(|| {
                    MonitorError::Store("monitor outbox queue order exhausted".into())
                })
            })?;
            PendingMonitorReport {
                report,
                terminal_reason,
                queue_order,
                queued_at_ms: now_ms(),
            }
        } else {
            // Delivery is stalled behind the oldest item. Fold every later
            // low-rate occurrence into one durable follow-up slot rather
            // than dropping it or growing an unbounded outbox.
            let follow_up = matching
                .pop()
                .ok_or_else(|| MonitorError::Store("monitor follow-up slot disappeared".into()))?;
            coalesce_pending_report(follow_up, report, terminal_reason)
        };
        let pending_revision = serde_json::to_string(&pending).map_err(|error| {
            MonitorError::Store(format!("cannot encode pending monitor revision: {error}"))
        })?;
        let identity = stable_digest(&[
            session.as_str(),
            &pending.report.report_id,
            "pending",
            &pending_revision,
        ]);
        let fact = MonitorJournalEvent::MonitorReportPending {
            pending: pending.clone(),
        };
        let mut envelopes = [monitor_envelope(
            session,
            None,
            registration.branch_id.as_ref(),
            registration.agent_id.as_ref(),
            &format!("monitor-report-pending-{}", &identity[..24]),
            hub.device_id(),
            hub.worker_generation(),
            fact.to_value()?,
        )];
        hub.append(&mut envelopes)
            .await
            .map_err(monitor_store_error)?;
        self.inner.registry.insert_pending(session, pending.clone());
        drop(mutation);
        if start_delivery {
            self.schedule_delivery(hub.downgrade(), session.clone(), pending);
        }
        Ok(ReportQueueOutcome::Queued)
    }

    fn schedule_enqueue_retry(
        &self,
        hub: WeakSessionHub,
        session: SessionId,
        registration: MonitorRegistration,
        report: MonitorReport,
        terminal_reason: Option<MonitorRemovalReason>,
    ) {
        if *self.inner.shutdown.borrow() || self.is_retired(&session) {
            return;
        }
        let key = (session.clone(), registration.monitor_id.clone());
        let item = EnqueueRetryItem {
            registration,
            report,
            terminal_reason,
            wait_for_source_sequence: if terminal_reason == Some(MonitorRemovalReason::TimedOut) {
                Some(self.sms_enqueue_watermark())
            } else {
                None
            },
        };
        let mut tasks = self
            .inner
            .enqueue_tasks
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if *self.inner.shutdown.borrow() || self.is_retired(&key.0) {
            return;
        }
        if let Some(existing) = tasks.get(&key) {
            existing
                .pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(item);
            return;
        }
        let token = self
            .inner
            .enqueue_sequence
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        let pending = Arc::new(StdMutex::new(EnqueueRetryQueue::default()));
        pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(item);
        let task_pending = Arc::clone(&pending);
        let weak_service = Arc::downgrade(&self.inner);
        let task_key = key.clone();
        let (cancel, mut cancelled) = oneshot::channel();
        let (start, started) = oneshot::channel();
        let mut shutdown = self.inner.shutdown.subscribe();
        let task = tokio::spawn(async move {
            if started.await.is_err() {
                return;
            }
            let mut backoff = MONITOR_DELIVERY_RETRY_MIN;
            loop {
                let item = task_pending
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .items
                    .front()
                    .cloned();
                let Some(item) = item else {
                    let Some(inner) = weak_service.upgrade() else {
                        break;
                    };
                    let mut tasks = inner
                        .enqueue_tasks
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    let Some(current) = tasks.get(&task_key) else {
                        break;
                    };
                    if current.token != token {
                        break;
                    }
                    let empty = current
                        .pending
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .items
                        .is_empty();
                    if empty {
                        tasks.remove(&task_key);
                        break;
                    }
                    drop(tasks);
                    continue;
                };
                let (Some(inner), Some(hub)) = (weak_service.upgrade(), hub.upgrade()) else {
                    break;
                };
                let service = MonitorService { inner };
                if let Some(watermark) = item.wait_for_source_sequence {
                    let mut classified = service.inner.sms_classified.subscribe();
                    if *classified.borrow() < watermark {
                        let ready = tokio::select! {
                            changed = classified.changed() => changed.is_ok(),
                            _ = &mut cancelled => false,
                            changed = shutdown.changed() => {
                                let _ = changed;
                                false
                            }
                        };
                        if !ready {
                            break;
                        }
                        continue;
                    }
                }
                let outcome = tokio::select! {
                    result = service.queue_report(
                        &hub,
                        &session,
                        &item.registration,
                        item.report.clone(),
                        item.terminal_reason,
                    ) => Some(result),
                    _ = &mut cancelled => None,
                    changed = shutdown.changed() => {
                        let _ = changed;
                        None
                    }
                };
                let completed = match outcome {
                    None => break,
                    Some(Ok(ReportQueueOutcome::Inactive | ReportQueueOutcome::Queued)) => true,
                    Some(Ok(ReportQueueOutcome::AlreadyPending)) => {
                        service
                            .inner
                            .registry
                            .pending(&session, &item.report.report_id)
                            .is_some()
                            || service
                                .inner
                                .registry
                                .pending_summary(&session, &item.registration.monitor_id)
                                .1
                    }
                    Some(Ok(ReportQueueOutcome::CapacityReached)) => false,
                    Some(Err(error)) => {
                        tracing::warn!(%session, monitor = %item.registration.monitor_id, %error, "monitor outbox enqueue will retry");
                        false
                    }
                };
                if completed {
                    let mut queue = task_pending.lock().unwrap_or_else(PoisonError::into_inner);
                    if queue
                        .items
                        .front()
                        .is_some_and(|front| front.report.report_id == item.report.report_id)
                    {
                        queue.items.pop_front();
                    }
                    backoff = MONITOR_DELIVERY_RETRY_MIN;
                    continue;
                }
                let keep_retrying = tokio::select! {
                    () = tokio::time::sleep(backoff) => true,
                    _ = &mut cancelled => false,
                    changed = shutdown.changed() => {
                        let _ = changed;
                        false
                    }
                };
                if !keep_retrying {
                    break;
                }
                backoff = std::cmp::min(backoff.saturating_mul(2), MONITOR_DELIVERY_RETRY_MAX);
            }
        });
        tasks.insert(
            key,
            EnqueueTask {
                token,
                pending,
                cancel,
                task,
            },
        );
        drop(tasks);
        let _ = start.send(());
    }

    fn cancel_enqueue(&self, session: &SessionId, monitor_id: &str) {
        if let Some(enqueue) = self
            .inner
            .enqueue_tasks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&(session.clone(), monitor_id.to_owned()))
        {
            let _ = enqueue.cancel.send(());
        }
    }

    fn schedule_delivery(
        &self,
        hub: WeakSessionHub,
        session: SessionId,
        pending: PendingMonitorReport,
    ) {
        if *self.inner.shutdown.borrow() || self.is_retired(&session) {
            return;
        }
        let key = (session.clone(), pending.report.report_id.clone());
        let token = self
            .inner
            .delivery_sequence
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        let weak_service = Arc::downgrade(&self.inner);
        let completion_service = weak_service.clone();
        let completion_key = key.clone();
        let (cancel, mut cancelled) = oneshot::channel();
        let (start, started) = oneshot::channel();
        let mut shutdown = self.inner.shutdown.subscribe();
        let task = tokio::spawn(async move {
            if started.await.is_err() {
                return;
            }
            let mut backoff = MONITOR_DELIVERY_RETRY_MIN;
            loop {
                let (Some(inner), Some(hub)) = (weak_service.upgrade(), hub.upgrade()) else {
                    break;
                };
                let service = MonitorService { inner };
                let result = tokio::select! {
                    result = service.deliver_pending(&hub, &session, &pending) => Some(result),
                    _ = &mut cancelled => None,
                    changed = shutdown.changed() => {
                        let _ = changed;
                        None
                    }
                };
                match result {
                    Some(Ok(())) | None => break,
                    Some(Err(error)) => {
                        tracing::warn!(%session, report = %pending.report.report_id, %error, "monitor outbox delivery will retry");
                    }
                }
                let keep_retrying = tokio::select! {
                    () = tokio::time::sleep(backoff) => true,
                    _ = &mut cancelled => false,
                    changed = shutdown.changed() => {
                        let _ = changed;
                        false
                    }
                };
                if !keep_retrying {
                    break;
                }
                backoff = std::cmp::min(backoff.saturating_mul(2), MONITOR_DELIVERY_RETRY_MAX);
            }
            if let Some(inner) = completion_service.upgrade() {
                let mut tasks = inner
                    .delivery_tasks
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                if tasks
                    .get(&completion_key)
                    .is_some_and(|current| current.token == token)
                {
                    tasks.remove(&completion_key);
                }
            }
        });
        let mut tasks = self
            .inner
            .delivery_tasks
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if *self.inner.shutdown.borrow() || self.is_retired(&key.0) || tasks.contains_key(&key) {
            drop(tasks);
            let _ = cancel.send(());
            return;
        }
        tasks.insert(
            key,
            DeliveryTask {
                token,
                cancel,
                task,
            },
        );
        drop(tasks);
        let _ = start.send(());
    }

    async fn deliver_pending(
        &self,
        hub: &SessionHub,
        session: &SessionId,
        pending: &PendingMonitorReport,
    ) -> Result<(), MonitorError> {
        if *self.inner.shutdown.borrow() || self.is_retired(session) {
            return Ok(());
        }
        if self
            .inner
            .registry
            .pending(session, &pending.report.report_id)
            .as_ref()
            != Some(pending)
        {
            return Ok(());
        }
        self.deliver_report(session, pending.report.clone()).await?;

        let mutation = self.inner.mutations.lock().await;
        if self
            .inner
            .registry
            .pending(session, &pending.report.report_id)
            .as_ref()
            != Some(pending)
        {
            return Ok(());
        }
        let delivered = MonitorJournalEvent::MonitorReportDelivered {
            report_id: pending.report.report_id.clone(),
            delivered_at_ms: now_ms(),
        };
        let delivered_identity =
            stable_digest(&[session.as_str(), &pending.report.report_id, "delivered"]);
        let mut envelopes = vec![monitor_envelope(
            session,
            None,
            pending.report.branch_id.as_ref(),
            pending.report.agent_id.as_ref(),
            &format!("monitor-report-delivered-{}", &delivered_identity[..24]),
            hub.device_id(),
            hub.worker_generation(),
            delivered.to_value()?,
        )];
        if let Some(reason) = pending.terminal_reason {
            let removed = MonitorJournalEvent::MonitorRemoved {
                monitor_id: pending.report.monitor_id.clone(),
                reason,
                removed_at_ms: now_ms(),
            };
            let removed_identity = stable_digest(&[
                session.as_str(),
                &pending.report.report_id,
                removal_reason_name(reason),
            ]);
            envelopes.push(monitor_envelope(
                session,
                None,
                pending.report.branch_id.as_ref(),
                pending.report.agent_id.as_ref(),
                &format!("monitor-terminal-{}", &removed_identity[..24]),
                hub.device_id(),
                hub.worker_generation(),
                removed.to_value()?,
            ));
        }
        hub.append(&mut envelopes)
            .await
            .map_err(monitor_store_error)?;
        self.inner
            .registry
            .remove_pending(session, &pending.report.report_id);
        if pending.terminal_reason.is_some() {
            self.inner
                .registry
                .remove(session, &pending.report.monitor_id);
            self.clear_rate(session, &pending.report.monitor_id);
            self.cancel_timeout(session, &pending.report.monitor_id);
            self.cancel_enqueue(session, &pending.report.monitor_id);
        }
        let next = if pending.terminal_reason.is_none() {
            self.inner
                .registry
                .pending_for_monitor(session, &pending.report.monitor_id)
                .into_iter()
                .next()
        } else {
            None
        };
        drop(mutation);
        if let Some(next) = next {
            self.schedule_delivery(hub.downgrade(), session.clone(), next);
        }
        Ok(())
    }

    fn schedule_timeout(
        &self,
        hub: WeakSessionHub,
        session: SessionId,
        registration: MonitorRegistration,
    ) {
        let Some(expires_at_ms) = registration.expires_at_ms else {
            return;
        };
        let delay = Duration::from_millis(expires_at_ms.saturating_sub(now_ms()));
        self.schedule_timeout_after(hub, session, registration, delay);
    }

    fn schedule_timeout_after(
        &self,
        hub: WeakSessionHub,
        session: SessionId,
        registration: MonitorRegistration,
        delay: Duration,
    ) {
        if *self.inner.shutdown.borrow() || self.is_retired(&session) {
            return;
        }
        let weak_service = Arc::downgrade(&self.inner);
        let completion_service = weak_service.clone();
        let key = (session.clone(), registration.monitor_id.clone());
        let completion_key = key.clone();
        let token = self
            .inner
            .timeout_sequence
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        let (cancel, cancelled) = oneshot::channel();
        let (start, started) = oneshot::channel();
        let task_session = session.clone();
        let task = tokio::spawn(async move {
            if started.await.is_err() {
                return;
            }
            tokio::select! {
                () = async {
                    tokio::time::sleep(delay).await;
                    if let (Some(inner), Some(hub)) = (weak_service.upgrade(), hub.upgrade()) {
                        let service = MonitorService { inner };
                        service
                            .expire_monitor(&hub, &task_session, &registration)
                            .await;
                    }
                } => {}
                _ = cancelled => {}
            }
            if let Some(inner) = completion_service.upgrade() {
                let mut tasks = inner
                    .timeout_tasks
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                if tasks
                    .get(&completion_key)
                    .is_some_and(|current| current.token == token)
                {
                    tasks.remove(&completion_key);
                }
            }
        });
        let mut tasks = self
            .inner
            .timeout_tasks
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if *self.inner.shutdown.borrow() || self.is_retired(&session) {
            drop(tasks);
            let _ = cancel.send(());
            return;
        }
        let previous = tasks.insert(
            key,
            TimeoutTask {
                token,
                cancel,
                task,
            },
        );
        drop(tasks);
        if let Some(previous) = previous {
            let _ = previous.cancel.send(());
        }
        let _ = start.send(());
    }

    fn cancel_timeout(&self, session: &SessionId, monitor_id: &str) {
        if let Some(timeout) = self
            .inner
            .timeout_tasks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&(session.clone(), monitor_id.to_owned()))
        {
            let _ = timeout.cancel.send(());
        }
    }

    async fn expire_monitor(
        &self,
        hub: &SessionHub,
        session: &SessionId,
        registration: &MonitorRegistration,
    ) {
        let _mutation = self.inner.mutations.lock().await;
        if *self.inner.shutdown.borrow() || self.is_retired(session) {
            return;
        }
        let current = self.inner.registry.get(session, &registration.monitor_id);
        if current.as_ref() != Some(registration) {
            return;
        }
        let Some(expires_at_ms) = registration.expires_at_ms else {
            return;
        };
        // Expiry is inclusive for source observations. Capture the source
        // watermark only after the first millisecond strictly beyond it, so
        // an event observed at exactly `expires_at_ms` is also fenced in.
        let remaining_ms = expires_at_ms.saturating_add(1).saturating_sub(now_ms());
        if remaining_ms > 0 {
            // Wall clock can move backward after the timer was scheduled.
            self.schedule_timeout_after(
                hub.downgrade(),
                session.clone(),
                registration.clone(),
                Duration::from_millis(remaining_ms),
            );
            return;
        }
        let report = build_report(
            hub,
            session,
            registration,
            MonitorReportStatus::TimedOut,
            Vec::new(),
        );
        self.schedule_enqueue_retry(
            hub.downgrade(),
            session.clone(),
            registration.clone(),
            report,
            Some(MonitorRemovalReason::TimedOut),
        );
    }

    async fn deliver_report(
        &self,
        session: &SessionId,
        report: MonitorReport,
    ) -> Result<MonitorDeliveryReceipt, MonitorError> {
        let sink = self
            .inner
            .sink
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let receipt = tokio::time::timeout(
            MONITOR_DELIVERY_ATTEMPT_TIMEOUT,
            sink.deliver(session, report.clone()),
        )
        .await
        .map_err(|_| MonitorError::Delivery("monitor delivery attempt timed out".into()))??;
        let transport = self
            .inner
            .transport_sink
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(transport) = transport {
            match tokio::time::timeout(
                MONITOR_TRANSPORT_MIRROR_TIMEOUT,
                transport.deliver(session, report),
            )
            .await
            {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => {
                    // Normal session output remains the durable delivery. A
                    // chat mirror can catch up without suppressing wake.
                    tracing::warn!(%session, %error, "monitor transport mirror delivery failed");
                }
                Err(_) => {
                    tracing::warn!(%session, "monitor transport mirror delivery timed out");
                }
            }
        }
        Ok(receipt)
    }
}

pub(crate) struct MonitorToolCoordinates {
    pub(crate) run_id: RunId,
    pub(crate) branch_id: Option<BranchId>,
    pub(crate) agent_id: Option<AgentId>,
    pub(crate) call_id: String,
    pub(crate) device_id: haider_protocol::ids::DeviceId,
}

impl SessionHub {
    pub(crate) async fn execute_monitor_tool(
        &self,
        store: &HubStoreHandle,
        coordinates: MonitorToolCoordinates,
        request: MonitorRequest,
    ) -> ToolResult<BoundedResult> {
        self.inner_monitor()
            .execute_tool(self, store, coordinates, request)
            .await
    }

    async fn wake_monitor_report(
        &self,
        report: MonitorReport,
    ) -> Result<MonitorDeliveryReceipt, MonitorError> {
        let text = report.prompt_text();
        let identity = stable_digest(&[
            report.session_id.as_str(),
            &report.monitor_id,
            &report.report_id,
        ]);
        let request_json = serde_json::to_string(&json!({
            "session_id": report.session_id,
            "monitor_id": report.monitor_id,
            "report_id": report.report_id,
            "text": text,
            "mode": DeliveryMode::Subturn,
        }))
        .map_err(|error| MonitorError::Delivery(format!("cannot encode monitor wake: {error}")))?;
        let delivery_text = text.clone();
        let accepted = self
            .accept_internal_turn(TurnAcceptCommand {
                command_id: format!("monitor-wake-{}", &identity[..24]),
                request_digest: crate::delegation::digest_bytes(request_json.as_bytes()),
                request_json,
                session_id: report.session_id.clone(),
                worker_generation: self.worker_generation(),
                branch_id: report.branch_id.clone(),
                run_id: RunId::new(format!("monitor-run-{}", &identity[..24])),
                agent_id: report.agent_id.clone(),
                text,
                attachments: Vec::new(),
                mode: DeliveryMode::Subturn,
                queued_event_id: EventId::new(format!("monitor-queued-{}", &identity[..24])),
                user_event_id: EventId::new(format!("monitor-user-{}", &identity[..24])),
                active_event_id: EventId::new(format!("monitor-active-{}", &identity[..24])),
                device_id: self.device_id(),
            })
            .await
            .map_err(|error| MonitorError::Delivery(error.message))?;
        let accepted_disposition = accepted.disposition;
        let disposition = match accepted_disposition {
            TurnAdmissionDisposition::Started => "started",
            TurnAdmissionDisposition::Queued => "queued",
            TurnAdmissionDisposition::SteerPending => "steer_pending",
            TurnAdmissionDisposition::SubturnPending => "subturn_pending",
        };
        let handoff = if accepted.worker_generation == self.worker_generation() {
            Some(match accepted_disposition {
                TurnAdmissionDisposition::Started | TurnAdmissionDisposition::Queued => {
                    self.submit_internal_turn(accepted).await
                }
                TurnAdmissionDisposition::SteerPending => {
                    self.submit_internal_nudge(accepted, delivery_text).await
                }
                TurnAdmissionDisposition::SubturnPending => {
                    self.submit_internal_subturn(accepted, delivery_text).await
                }
            })
        } else {
            // Unfenced receipt replay proves this wake was durable in an
            // earlier generation; recovery owns execution of that prefix.
            None
        };
        let handed_off = match handoff {
            None => false,
            Some(Ok(())) => true,
            Some(Err(error)) => {
                return Err(MonitorError::Delivery(format!(
                    "durable monitor event could not reach the worker manager: {}",
                    error.message
                )));
            }
        };
        Ok(MonitorDeliveryReceipt {
            durable: true,
            handed_off,
            disposition,
        })
    }
}

pub(crate) fn monitor_control_policy() -> haider_rpc::MonitorControlPolicyWire {
    haider_rpc::MonitorControlPolicyWire {
        list: haider_rpc::Capability::View,
        register: haider_rpc::Capability::Control,
        register_requires_control_attachment: true,
        remove: haider_rpc::Capability::Control,
        remove_requires_control_attachment: true,
        watch: haider_rpc::Capability::View,
    }
}

pub(crate) fn monitor_source_availability() -> Vec<haider_rpc::MonitorSourceAvailabilityWire> {
    use haider_rpc::{
        MonitorSourceAvailabilityStateWire as Availability, MonitorSourceAvailabilityWire as Row,
        MonitorSourceKindWire as Source, MonitorSourceUnavailableReasonWire as Reason,
    };

    vec![
        Row {
            source: Source::Sms,
            availability: Availability::Available,
        },
        Row {
            source: Source::Process,
            availability: Availability::Unavailable {
                reason: Reason::AdapterInactive,
            },
        },
        Row {
            source: Source::File,
            availability: Availability::Unavailable {
                reason: Reason::AdapterInactive,
            },
        },
        Row {
            source: Source::Poll,
            availability: Availability::Unavailable {
                reason: Reason::AdapterInactive,
            },
        },
        Row {
            source: Source::Timer,
            availability: Availability::Unavailable {
                reason: Reason::AdapterInactive,
            },
        },
    ]
}

pub(crate) fn monitor_list_rejected(
    session_id: SessionId,
    rejection: haider_rpc::MonitorControlRejectionWire,
) -> haider_rpc::MonitorListReceiptWire {
    haider_rpc::MonitorListReceiptWire {
        session_id,
        policy: monitor_control_policy(),
        sources: monitor_source_availability(),
        outcome: haider_rpc::MonitorListOutcomeWire::Rejected { rejection },
    }
}

pub(crate) fn monitor_register_rejected(
    command_id: haider_rpc::CommandId,
    session_id: SessionId,
    worker_generation: u64,
    rejection: haider_rpc::MonitorControlRejectionWire,
) -> haider_rpc::MonitorRegisterReceiptWire {
    haider_rpc::MonitorRegisterReceiptWire {
        command_id,
        session_id,
        worker_generation,
        policy: monitor_control_policy(),
        sources: monitor_source_availability(),
        outcome: haider_rpc::MonitorRegisterOutcomeWire::Rejected { rejection },
    }
}

pub(crate) fn monitor_remove_rejected(
    command_id: haider_rpc::CommandId,
    session_id: SessionId,
    worker_generation: u64,
    rejection: haider_rpc::MonitorControlRejectionWire,
) -> haider_rpc::MonitorRemoveReceiptWire {
    haider_rpc::MonitorRemoveReceiptWire {
        command_id,
        session_id,
        worker_generation,
        policy: monitor_control_policy(),
        sources: monitor_source_availability(),
        outcome: haider_rpc::MonitorRemoveOutcomeWire::Rejected { rejection },
    }
}

pub(crate) fn monitor_watch_rejected(
    session_id: SessionId,
    rejection: haider_rpc::MonitorControlRejectionWire,
) -> haider_rpc::MonitorWatchReceiptWire {
    haider_rpc::MonitorWatchReceiptWire {
        session_id,
        policy: monitor_control_policy(),
        sources: monitor_source_availability(),
        outcome: haider_rpc::MonitorWatchOutcomeWire::Rejected { rejection },
    }
}

fn monitor_store_rejection(error: MonitorError) -> haider_rpc::MonitorControlRejectionWire {
    match error {
        MonitorError::StoreUnavailable { message, retryable } => {
            haider_rpc::MonitorControlRejectionWire::StoreUnavailable {
                retryable,
                detail: message,
            }
        }
        other => haider_rpc::MonitorControlRejectionWire::StoreUnavailable {
            retryable: false,
            detail: other.to_string(),
        },
    }
}

fn monitor_store_haider_rejection(
    error: haider_protocol::error::HaiderError,
) -> haider_rpc::MonitorControlRejectionWire {
    haider_rpc::MonitorControlRejectionWire::StoreUnavailable {
        retryable: error.retryable,
        detail: error.message,
    }
}

fn monitor_receipt_rejection(
    error: haider_protocol::error::HaiderError,
) -> haider_rpc::MonitorControlRejectionWire {
    if error.code == haider_protocol::error::ErrorCode::InvalidArgument {
        haider_rpc::MonitorControlRejectionWire::CommandConflict
    } else {
        monitor_store_haider_rejection(error)
    }
}

fn decode_client_receipt(
    value: serde_json::Value,
) -> Result<StoredMonitorClientReceiptBody, MonitorError> {
    serde_json::from_value(value)
        .map_err(|error| MonitorError::Store(format!("invalid global monitor receipt: {error}")))
}

fn invalid_monitor_request(error: ToolError) -> haider_rpc::MonitorControlRejectionWire {
    let detail = match error {
        ToolError::InvalidArgument { message } => message,
        other => other.to_string(),
    };
    haider_rpc::MonitorControlRejectionWire::InvalidRequest {
        field: None,
        detail,
    }
}

fn monitor_register_request_from_wire(
    source: haider_rpc::MonitorSourceWire,
    filter: Option<haider_rpc::MonitorFilterWire>,
    action: haider_rpc::MonitorActionWire,
    occurrence: haider_rpc::MonitorOccurrenceWire,
    lifetime: haider_rpc::MonitorLifetimeWire,
) -> ToolResult<MonitorRequest> {
    MonitorRequest::from_tool_args(json!({
        "operation": "register",
        "source": source,
        "filter": filter,
        "action": action,
        "occurrence": occurrence,
        "lifetime": lifetime,
    }))
}

fn monitor_registration_wire(
    registration: &MonitorRegistration,
) -> haider_rpc::MonitorRegistrationWire {
    haider_rpc::MonitorRegistrationWire {
        monitor_id: registration.monitor_id.clone(),
        session_id: registration.owner_session_id.clone(),
        branch_id: registration.branch_id.clone(),
        agent_id: registration.agent_id.clone(),
        source: monitor_source_wire(&registration.source),
        filter: registration.filter.as_ref().map(monitor_filter_wire),
        action: monitor_action_wire(&registration.action),
        occurrence: monitor_occurrence_wire(registration.occurrence),
        created_at_ms: registration.created_at_ms,
        start_source_sequence: registration.start_sequence,
        expires_at_ms: registration.expires_at_ms,
    }
}

fn monitor_source_wire(source: &MonitorSource) -> haider_rpc::MonitorSourceWire {
    match source {
        MonitorSource::Sms => haider_rpc::MonitorSourceWire::Sms,
        MonitorSource::Process { command } => haider_rpc::MonitorSourceWire::Process {
            command: command.clone(),
        },
        MonitorSource::File { path } => haider_rpc::MonitorSourceWire::File { path: path.clone() },
        MonitorSource::Poll {
            command,
            interval_ms,
        } => haider_rpc::MonitorSourceWire::Poll {
            command: command.clone(),
            interval_ms: *interval_ms,
        },
        MonitorSource::Timer { interval_ms } => haider_rpc::MonitorSourceWire::Timer {
            interval_ms: *interval_ms,
        },
    }
}

fn monitor_source_kind_wire(source: MonitorSourceKind) -> haider_rpc::MonitorSourceKindWire {
    match source {
        MonitorSourceKind::Sms => haider_rpc::MonitorSourceKindWire::Sms,
        MonitorSourceKind::Process => haider_rpc::MonitorSourceKindWire::Process,
        MonitorSourceKind::File => haider_rpc::MonitorSourceKindWire::File,
        MonitorSourceKind::Poll => haider_rpc::MonitorSourceKindWire::Poll,
        MonitorSourceKind::Timer => haider_rpc::MonitorSourceKindWire::Timer,
    }
}

fn monitor_filter_wire(filter: &MonitorFilter) -> haider_rpc::MonitorFilterWire {
    haider_rpc::MonitorFilterWire {
        field: match filter.field {
            MonitorFilterField::Address => haider_rpc::MonitorFilterFieldWire::Address,
            MonitorFilterField::Body => haider_rpc::MonitorFilterFieldWire::Body,
            MonitorFilterField::Payload => haider_rpc::MonitorFilterFieldWire::Payload,
        },
        operator: match filter.operator {
            MonitorFilterOperator::Equals => haider_rpc::MonitorFilterOperatorWire::Equals,
            MonitorFilterOperator::Contains => haider_rpc::MonitorFilterOperatorWire::Contains,
            MonitorFilterOperator::StartsWith => haider_rpc::MonitorFilterOperatorWire::StartsWith,
            MonitorFilterOperator::EndsWith => haider_rpc::MonitorFilterOperatorWire::EndsWith,
        },
        value: filter.value.clone(),
        case_sensitive: filter.case_sensitive,
    }
}

fn monitor_action_wire(action: &MonitorAction) -> haider_rpc::MonitorActionWire {
    haider_rpc::MonitorActionWire {
        report: action.report,
        follow_up: action.follow_up.clone(),
    }
}

fn monitor_occurrence_wire(occurrence: MonitorOccurrence) -> haider_rpc::MonitorOccurrenceWire {
    match occurrence {
        MonitorOccurrence::Once => haider_rpc::MonitorOccurrenceWire::Once,
        MonitorOccurrence::Every => haider_rpc::MonitorOccurrenceWire::Every,
    }
}

/// Decode one durable pending-report fact into the dedicated client delivery
/// shape. Fork-copied facts fail the embedded owner fence and stay silent.
pub(crate) fn monitor_delivery_report(
    envelope: &RawEnvelope,
) -> Option<haider_rpc::MonitorDeliveryReportWire> {
    let MonitorJournalEvent::MonitorReportPending { pending } =
        MonitorJournalEvent::from_value(&envelope.payload)?
    else {
        return None;
    };
    let report = pending.report;
    if report.session_id != envelope.session_id {
        return None;
    }
    let delivery_identity = stable_digest(&[
        report.session_id.as_str(),
        &envelope.seq.to_string(),
        "monitor-delivery-v1",
    ]);
    Some(haider_rpc::MonitorDeliveryReportWire {
        report_id: report.report_id.clone(),
        monitor_id: report.monitor_id,
        session_id: report.session_id,
        branch_id: report.branch_id,
        agent_id: report.agent_id,
        source: monitor_source_kind_wire(report.source),
        status: match report.status {
            MonitorReportStatus::Matched => haider_rpc::MonitorReportStatusWire::Matched,
            MonitorReportStatus::RateLimited => haider_rpc::MonitorReportStatusWire::RateLimited,
            MonitorReportStatus::TimedOut => haider_rpc::MonitorReportStatusWire::TimedOut,
        },
        events: report
            .events
            .into_iter()
            .map(|event| haider_rpc::MonitorEventWire {
                sequence: event.sequence,
                observed_at_ms: event.observed_at_ms,
                payload: match event.payload {
                    MonitorEventPayload::Sms(sms) => haider_rpc::MonitorEventPayloadWire::Sms {
                        address: sms.address,
                        body: sms.body,
                        received_at_ms: sms.received_at_ms,
                    },
                    MonitorEventPayload::Process { line } => {
                        haider_rpc::MonitorEventPayloadWire::Process { line }
                    }
                    MonitorEventPayload::File { payload } => {
                        haider_rpc::MonitorEventPayloadWire::File { payload }
                    }
                    MonitorEventPayload::Poll { payload } => {
                        haider_rpc::MonitorEventPayloadWire::Poll { payload }
                    }
                    MonitorEventPayload::Timer { fired_at_ms } => {
                        haider_rpc::MonitorEventPayloadWire::Timer { fired_at_ms }
                    }
                },
            })
            .collect(),
        coalesced_count: u64::try_from(report.coalesced_count).unwrap_or(u64::MAX),
        omitted_count: u64::try_from(report.omitted_count).unwrap_or(u64::MAX),
        action: monitor_action_wire(&report.action),
        cursor: envelope.seq,
        dedupe: haider_rpc::MonitorDeliveryDedupeWire {
            delivery_key: format!("monitor-delivery-{}", &delivery_identity[..24]),
            report_key: report.report_id,
        },
    })
}

fn monitor_matches(registration: &MonitorRegistration, event: &MonitorEvent) -> bool {
    if registration.source.kind() != event.source_kind() {
        return false;
    }
    if event.observed_at_ms < registration.created_at_ms
        || (event.observed_at_ms == registration.created_at_ms
            && event.sequence <= registration.start_sequence)
        || registration
            .expires_at_ms
            .is_some_and(|expires| event.observed_at_ms > expires)
    {
        return false;
    }
    let Some(filter) = &registration.filter else {
        return true;
    };
    let Some(candidate) = event.payload.field(filter.field) else {
        return false;
    };
    let (candidate, expected) = if filter.case_sensitive {
        (candidate.to_owned(), filter.value.clone())
    } else {
        (candidate.to_lowercase(), filter.value.to_lowercase())
    };
    match filter.operator {
        MonitorFilterOperator::Equals => candidate == expected,
        MonitorFilterOperator::Contains => candidate.contains(&expected),
        MonitorFilterOperator::StartsWith => candidate.starts_with(&expected),
        MonitorFilterOperator::EndsWith => candidate.ends_with(&expected),
    }
}

fn build_report(
    hub: &SessionHub,
    session: &SessionId,
    registration: &MonitorRegistration,
    status: MonitorReportStatus,
    mut events: Vec<MonitorEvent>,
) -> MonitorReport {
    let coalesced_count = events.len();
    let omitted_count = events.len().saturating_sub(MAX_MONITOR_REPORT_EVENTS);
    let first_sequence = events.first().map_or(0, |event| event.sequence);
    let last_sequence = events.last().map_or(0, |event| event.sequence);
    let identity = stable_digest(&[
        session.as_str(),
        &registration.monitor_id,
        &hub.worker_generation().to_string(),
        &first_sequence.to_string(),
        &last_sequence.to_string(),
        &coalesced_count.to_string(),
        report_status_name(status),
    ]);
    events.truncate(MAX_MONITOR_REPORT_EVENTS);
    MonitorReport {
        report_id: format!("monitor-report-{}", &identity[..24]),
        monitor_id: registration.monitor_id.clone(),
        session_id: session.clone(),
        branch_id: registration.branch_id.clone(),
        agent_id: registration.agent_id.clone(),
        source: registration.source.kind(),
        status,
        events,
        coalesced_count,
        omitted_count,
        action: registration.action.clone(),
    }
}

fn coalesce_pending_report(
    mut pending: PendingMonitorReport,
    incoming: MonitorReport,
    terminal_reason: Option<MonitorRemovalReason>,
) -> PendingMonitorReport {
    coalesce_monitor_report(&mut pending.report, incoming, terminal_reason.is_some());
    if terminal_reason.is_some() {
        pending.terminal_reason = terminal_reason;
    }
    pending
}

fn coalesce_monitor_report(report: &mut MonitorReport, incoming: MonitorReport, terminal: bool) {
    let total = report
        .coalesced_count
        .saturating_add(incoming.coalesced_count);
    if terminal {
        report.status = incoming.status;
    }
    report.events.extend(incoming.events);
    report.events.truncate(MAX_MONITOR_REPORT_EVENTS);
    report.coalesced_count = total;
    report.omitted_count = total.saturating_sub(report.events.len());
}

fn oldest_pending_per_monitor(
    pending_reports: &BTreeMap<String, PendingMonitorReport>,
) -> Vec<PendingMonitorReport> {
    let mut pending = pending_reports.values().cloned().collect::<Vec<_>>();
    pending.sort_by(|left, right| {
        left.queue_order
            .cmp(&right.queue_order)
            .then_with(|| left.queued_at_ms.cmp(&right.queued_at_ms))
            .then_with(|| left.report.report_id.cmp(&right.report.report_id))
    });
    let mut adopted_delivery = HashSet::new();
    pending.retain(|report| adopted_delivery.insert(report.report.monitor_id.clone()));
    pending
}

// These fields mirror the durable EventEnvelope identity and routing tuple.
#[allow(clippy::too_many_arguments)]
fn monitor_envelope(
    session_id: &SessionId,
    run_id: Option<&RunId>,
    branch_id: Option<&BranchId>,
    agent_id: Option<&AgentId>,
    event_id: &str,
    device_id: haider_protocol::ids::DeviceId,
    worker_generation: u64,
    payload: serde_json::Value,
) -> RawEnvelope {
    EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(event_id),
        seq: 0,
        session_id: session_id.clone(),
        branch_id: branch_id.cloned(),
        run_id: run_id.cloned(),
        agent_id: agent_id.cloned(),
        device_id,
        authority_epoch: 0,
        worker_generation,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: false,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload,
    }
}

fn tool_result(
    preview: serde_json::Value,
    status: ToolResultStatus,
    reason: Option<String>,
) -> BoundedResult {
    BoundedResult {
        preview: preview.to_string(),
        truncated: false,
        artifact: None,
        images: Vec::new(),
        cursor: None,
        status,
        reason,
        presentation: None,
    }
}

fn monitor_tool_error(error: MonitorError) -> ToolError {
    ToolError::Runtime {
        message: error.to_string(),
    }
}

fn stable_digest(parts: &[&str]) -> String {
    let mut hasher = blake3::Hasher::new();
    for part in parts {
        hasher.update(&(part.len() as u64).to_le_bytes());
        hasher.update(part.as_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

fn bounded_chars(value: &str, maximum: usize) -> String {
    value.chars().take(maximum).collect()
}

fn removal_reason_name(reason: MonitorRemovalReason) -> &'static str {
    match reason {
        MonitorRemovalReason::Removed => "removed",
        MonitorRemovalReason::OneShotComplete => "one-shot-complete",
        MonitorRemovalReason::TimedOut => "timed-out",
        MonitorRemovalReason::RateLimited => "rate-limited",
    }
}

fn report_status_name(status: MonitorReportStatus) -> &'static str {
    match status {
        MonitorReportStatus::Matched => "matched",
        MonitorReportStatus::RateLimited => "rate-limited",
        MonitorReportStatus::TimedOut => "timed-out",
    }
}

#[cfg(test)]
#[path = "monitor_tests.rs"]
mod monitor_tests;
