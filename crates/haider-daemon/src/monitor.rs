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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum MonitorEventPayload {
    Sms(SmsIncomingEvent),
    Process { line: String },
    File { payload: String },
    Poll { payload: String },
    Timer { fired_at_ms: u64 },
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
    Delivery(String),
}

impl fmt::Display for MonitorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidEvent(message) => write!(formatter, "invalid monitor event: {message}"),
            Self::SubscriptionClosed => formatter.write_str("monitor source subscription closed"),
            Self::Store(message) => write!(formatter, "monitor store failure: {message}"),
            Self::Delivery(message) => write!(formatter, "monitor delivery failure: {message}"),
        }
    }
}

impl std::error::Error for MonitorError {}

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
#[serde(tag = "type", rename_all = "snake_case")]
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
                .map_err(|error| MonitorError::Store(error.message))?;
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
                    Some(MonitorJournalEvent::MonitorToolReceipt { .. }) => {}
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
            .map_err(|error| monitor_tool_error(MonitorError::Store(error.message)))?;
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
                .map_err(|error| monitor_tool_error(MonitorError::Store(error.message)))?;
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
                    .map_err(|error| monitor_tool_error(MonitorError::Store(error.message)))?;
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
                let Some(registration) = self.inner.registry.get(store.session_id(), &monitor_id)
                else {
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
                };
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
                    .map_err(|error| monitor_tool_error(MonitorError::Store(error.message)))?;
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
            .map_err(|error| MonitorError::Store(error.message))?;
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
            .map_err(|error| MonitorError::Store(error.message))?;
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
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use haider_core::{
        BranchCreateCommand, SessionCreateCommand, SqliteStoreHandle, StoreHandle as _,
    };
    use haider_protocol::EventPayload;
    use haider_protocol::ids::DeviceId;
    use haider_protocol::state::{RunState, WaitReason};
    use std::sync::atomic::AtomicUsize;
    use tokio::sync::mpsc as tokio_mpsc;
    use tokio::sync::{oneshot as tokio_oneshot, watch as tokio_watch};
    use tokio::time::timeout;

    fn registration(filter: Option<MonitorFilter>) -> MonitorRegistration {
        MonitorRegistration {
            monitor_id: "monitor-test".into(),
            owner_session_id: SessionId::new("session-monitor-test"),
            source: MonitorSource::Sms,
            filter,
            action: MonitorAction {
                report: true,
                follow_up: None,
            },
            occurrence: MonitorOccurrence::Every,
            created_at_ms: 1,
            start_sequence: 0,
            expires_at_ms: None,
            branch_id: None,
            agent_id: None,
        }
    }

    fn sms(address: &str, body: &str) -> MonitorEvent {
        MonitorEvent {
            sequence: 1,
            observed_at_ms: 2,
            payload: MonitorEventPayload::Sms(SmsIncomingEvent {
                address: address.into(),
                body: body.into(),
                received_at_ms: 3,
            }),
        }
    }

    fn test_report(report_id: &str, body: &str, status: MonitorReportStatus) -> MonitorReport {
        MonitorReport {
            report_id: report_id.into(),
            monitor_id: "monitor-test".into(),
            session_id: SessionId::new("session-monitor-test"),
            branch_id: None,
            agent_id: None,
            source: MonitorSourceKind::Sms,
            status,
            events: vec![sms("+1", body)],
            coalesced_count: 1,
            omitted_count: 0,
            action: MonitorAction {
                report: true,
                follow_up: None,
            },
        }
    }

    #[test]
    fn filter_matching_is_typed_case_aware_and_source_scoped() {
        let body = registration(Some(MonitorFilter {
            field: MonitorFilterField::Body,
            operator: MonitorFilterOperator::Contains,
            value: "DEPLOY".into(),
            case_sensitive: false,
        }));
        assert!(monitor_matches(&body, &sms("+1", "deploy complete")));
        assert!(!monitor_matches(&body, &sms("+1", "build running")));

        let address = registration(Some(MonitorFilter {
            field: MonitorFilterField::Address,
            operator: MonitorFilterOperator::Equals,
            value: "+1555".into(),
            case_sensitive: true,
        }));
        assert!(monitor_matches(&address, &sms("+1555", "hello")));
        assert!(!monitor_matches(&address, &sms("+1666", "hello")));
    }

    #[test]
    fn event_time_fences_registration_and_inclusive_expiry() {
        let mut watch = registration(None);
        watch.created_at_ms = 10;
        watch.start_sequence = 5;
        watch.expires_at_ms = Some(20);

        let mut before_registration = sms("+1", "before");
        before_registration.sequence = 5;
        before_registration.observed_at_ms = 10;
        assert!(!monitor_matches(&watch, &before_registration));

        let mut after_registration = before_registration.clone();
        after_registration.sequence = 6;
        assert!(monitor_matches(&watch, &after_registration));

        let mut at_expiry = after_registration.clone();
        at_expiry.observed_at_ms = 20;
        assert!(monitor_matches(&watch, &at_expiry));

        let mut after_expiry = at_expiry;
        after_expiry.observed_at_ms = 21;
        assert!(!monitor_matches(&watch, &after_expiry));
    }

    #[tokio::test]
    async fn source_publish_seam_is_instance_scoped_and_bounded() {
        let first = MonitorSourceHub::new();
        let second = MonitorSourceHub::new();
        let mut subscription = first.subscribe(MonitorSourceKind::Sms);
        let receipt = publish_sms_incoming(&first, "+1555", "wake", 10).unwrap();
        assert_eq!(receipt.subscriber_count, 1);
        let event = subscription.recv().await.unwrap();
        assert_eq!(event.sequence, 1);
        assert!(matches!(
            event.payload,
            MonitorEventPayload::Sms(SmsIncomingEvent {
                ref address,
                ref body,
                received_at_ms: 10,
            }) if address == "+1555" && body == "wake"
        ));

        let other = publish_sms_incoming(&second, "+1555", "isolated", 11).unwrap();
        assert_eq!(other.subscriber_count, 0);
        assert!(publish_sms_incoming(&first, "x", &"b".repeat(MAX_SMS_BODY_BYTES + 1), 1).is_err());
    }

    #[test]
    fn journal_fold_registers_lists_and_removes() {
        let registry = MonitorRegistry::default();
        let session = SessionId::new("session-monitor-test");
        let watch = registration(None);
        registry.install(session.clone(), BTreeMap::new(), BTreeMap::new());
        registry.insert(&session, watch.clone());
        assert_eq!(registry.snapshot(&session), vec![watch]);
        assert!(registry.remove(&session, "monitor-test").is_some());
        assert!(registry.snapshot(&session).is_empty());
    }

    #[test]
    fn durable_queue_order_beats_equal_or_rollback_wall_clock_on_adoption() {
        let session = SessionId::new("session-monitor-test");
        let active = PendingMonitorReport {
            report: test_report("z-active", "first", MonitorReportStatus::Matched),
            terminal_reason: None,
            queue_order: 7,
            queued_at_ms: 100,
        };
        let follow_up = PendingMonitorReport {
            report: test_report("a-follow-up", "second", MonitorReportStatus::RateLimited),
            terminal_reason: Some(MonitorRemovalReason::RateLimited),
            queue_order: 8,
            // A rolled-back wall clock and lexically earlier id must not let
            // the terminal follow-up overtake the active report on restart.
            queued_at_ms: 1,
        };
        let mut pending = BTreeMap::new();
        pending.insert(active.report.report_id.clone(), active.clone());
        pending.insert(follow_up.report.report_id.clone(), follow_up.clone());
        assert_eq!(oldest_pending_per_monitor(&pending), vec![active.clone()]);
        let mut equal_time = follow_up.clone();
        equal_time.queued_at_ms = active.queued_at_ms;
        pending.insert(equal_time.report.report_id.clone(), equal_time);
        assert_eq!(oldest_pending_per_monitor(&pending), vec![active.clone()]);
        pending.insert(follow_up.report.report_id.clone(), follow_up.clone());

        let registry = MonitorRegistry::default();
        registry.install(session.clone(), BTreeMap::new(), pending);
        assert_eq!(
            registry.pending_for_monitor(&session, "monitor-test"),
            vec![active, follow_up]
        );
    }

    #[test]
    fn pre_durable_retry_queue_is_bounded_and_coalesces_its_follow_up() {
        let watch = registration(None);
        let mut queue = EnqueueRetryQueue::default();
        for (id, body) in [
            ("retry-first", "first"),
            ("retry-second", "second"),
            ("retry-third", "third"),
        ] {
            queue.push(EnqueueRetryItem {
                registration: watch.clone(),
                report: test_report(id, body, MonitorReportStatus::Matched),
                terminal_reason: None,
                wait_for_source_sequence: None,
            });
        }
        assert_eq!(queue.items.len(), 2);
        assert_eq!(queue.items[0].report.report_id, "retry-first");
        assert_eq!(queue.items[1].report.report_id, "retry-second");
        assert_eq!(queue.items[1].report.coalesced_count, 2);
        assert!(queue.items[1].report.events.iter().any(|event| {
            matches!(&event.payload, MonitorEventPayload::Sms(sms) if sms.body == "third")
        }));

        queue.push(EnqueueRetryItem {
            registration: watch,
            report: test_report("retry-terminal", "stop", MonitorReportStatus::RateLimited),
            terminal_reason: Some(MonitorRemovalReason::RateLimited),
            wait_for_source_sequence: None,
        });
        assert_eq!(queue.items.len(), 2);
        assert_eq!(
            queue.items[1].terminal_reason,
            Some(MonitorRemovalReason::RateLimited)
        );
        assert_eq!(
            queue.items[1].report.status,
            MonitorReportStatus::RateLimited
        );
        assert_eq!(queue.items[1].report.coalesced_count, 3);
    }

    #[test]
    fn report_event_and_prompt_bounds_are_explicit() {
        let mut events = Vec::new();
        for sequence in 0..(MAX_MONITOR_REPORT_EVENTS + 3) {
            let mut event = sms("+1", &"x".repeat(MAX_REPORT_BODY_CHARS + 10));
            event.sequence = sequence as u64;
            events.push(event);
        }
        let watch = registration(None);
        let session = SessionId::new("session-report");
        let coalesced_count = events.len();
        let omitted_count = events.len().saturating_sub(MAX_MONITOR_REPORT_EVENTS);
        events.truncate(MAX_MONITOR_REPORT_EVENTS);
        let report = MonitorReport {
            report_id: "report".into(),
            monitor_id: watch.monitor_id,
            session_id: session,
            branch_id: None,
            agent_id: None,
            source: MonitorSourceKind::Sms,
            status: MonitorReportStatus::Matched,
            events,
            coalesced_count,
            omitted_count,
            action: watch.action,
        };
        assert_eq!(report.events.len(), MAX_MONITOR_REPORT_EVENTS);
        assert_eq!(report.omitted_count, 3);
        assert!(report.prompt_text().contains("monitor_event"));
    }

    struct MonitorWorld {
        store: SqliteStoreHandle,
        hub: SessionHub,
        session: SessionId,
        run: RunId,
        lease: HubStoreHandle,
        _root: tempfile::TempDir,
    }

    impl MonitorWorld {
        async fn new(label: &str) -> Self {
            let root = tempfile::tempdir().expect("temporary monitor profile");
            let store = SqliteStoreHandle::open(root.path())
                .await
                .expect("monitor store");
            let hub = SessionHub::new(
                store.clone(),
                crate::session_hub::SessionHubConfig::default(),
            )
            .expect("monitor hub");
            // Production activates after installing WorkerManager. These
            // subsystem tests intentionally exercise the sink seam directly.
            hub.inner_monitor().activate(hub.downgrade());
            let session = SessionId::new(format!("monitor-session-{label}"));
            let device = DeviceId::new(format!("monitor-device-{label}"));
            let cwd = std::fs::canonicalize(std::env::current_dir().expect("cwd"))
                .expect("canonical cwd")
                .to_string_lossy()
                .into_owned();
            hub.create_internal_session(SessionCreateCommand {
                command_id: format!("monitor-create-{label}"),
                request_digest: format!("monitor-create-{label}-digest"),
                request_json: format!(r#"{{"session":"{label}"}}"#),
                session_id: session.clone(),
                cwd,
                provider: "fake".into(),
                model: "fake-model".into(),
                max_tokens: 4096,
                permission_overrides: None,
                effort: None,
                fast: false,
                cache_policy: Default::default(),
                system_prompt_version: crate::worker::SystemPromptBuilder::VERSION.into(),
                event_id: EventId::new(format!("monitor-created-{label}")),
                device_id: device.clone(),
            })
            .await
            .expect("create monitor session");
            let run = RunId::new(format!("monitor-tool-run-{label}"));
            hub.accept_internal_turn(TurnAcceptCommand {
                command_id: format!("monitor-tool-submit-{label}"),
                request_digest: format!("monitor-tool-submit-{label}-digest"),
                request_json: format!(r#"{{"turn":"{label}"}}"#),
                session_id: session.clone(),
                worker_generation: store.worker_generation(),
                run_id: run.clone(),
                agent_id: None,
                branch_id: None,
                text: "register a monitor".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
                queued_event_id: EventId::new(format!("monitor-tool-queued-{label}")),
                user_event_id: EventId::new(format!("monitor-tool-user-{label}")),
                active_event_id: EventId::new(format!("monitor-tool-active-{label}")),
                device_id: device,
            })
            .await
            .expect("accept monitor tool turn");
            let lease = hub
                .acquire_worker_lease(session.clone())
                .await
                .expect("monitor worker lease");
            Self {
                store,
                hub,
                session,
                run,
                lease,
                _root: root,
            }
        }

        fn coordinates(&self, call: &str) -> MonitorToolCoordinates {
            MonitorToolCoordinates {
                run_id: self.run.clone(),
                branch_id: None,
                agent_id: None,
                call_id: call.to_owned(),
                device_id: DeviceId::new(format!("monitor-call-{call}")),
            }
        }

        async fn execute(&self, call: &str, request: MonitorRequest) -> BoundedResult {
            self.hub
                .execute_monitor_tool(&self.lease, self.coordinates(call), request)
                .await
                .expect("execute monitor tool")
        }

        async fn register(
            &self,
            call: &str,
            filter: Option<MonitorFilter>,
            occurrence: MonitorOccurrence,
        ) -> String {
            self.register_with_lifetime(call, filter, occurrence, MonitorLifetime::Session)
                .await
        }

        async fn register_with_lifetime(
            &self,
            call: &str,
            filter: Option<MonitorFilter>,
            occurrence: MonitorOccurrence,
            lifetime: MonitorLifetime,
        ) -> String {
            let result = self
                .execute(
                    call,
                    MonitorRequest::Register {
                        source: MonitorSource::Sms,
                        filter,
                        action: MonitorAction {
                            report: true,
                            follow_up: Some("react to this SMS".into()),
                        },
                        occurrence,
                        lifetime,
                    },
                )
                .await;
            assert_eq!(result.status, ToolResultStatus::Completed);
            serde_json::from_str::<serde_json::Value>(&result.preview)
                .expect("registration preview")
                .get("monitor_id")
                .and_then(serde_json::Value::as_str)
                .expect("monitor id")
                .to_owned()
        }

        async fn wait_for_count(&self, expected: usize) -> BoundedResult {
            timeout(Duration::from_secs(3), async {
                loop {
                    let result = self.execute("list-poll", MonitorRequest::List).await;
                    let count = serde_json::from_str::<serde_json::Value>(&result.preview)
                        .expect("monitor list preview")
                        .get("count")
                        .and_then(serde_json::Value::as_u64)
                        .expect("monitor list count") as usize;
                    if count == expected {
                        break result;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("monitor count did not converge")
        }

        fn install_canonical_test_sink(&self, sink: Arc<dyn MonitorDeliverySink>) {
            *self
                .hub
                .inner_monitor()
                .inner
                .sink
                .write()
                .unwrap_or_else(PoisonError::into_inner) = sink;
        }
    }

    struct CapturingSink {
        reports: tokio_mpsc::UnboundedSender<MonitorReport>,
    }

    struct FailOnceSink {
        attempts: Arc<AtomicUsize>,
        reports: tokio_mpsc::UnboundedSender<MonitorReport>,
    }

    struct GatedSink {
        reports: tokio_mpsc::UnboundedSender<MonitorReport>,
        started: StdMutex<Option<tokio_oneshot::Sender<()>>>,
        release: tokio_watch::Receiver<bool>,
    }

    #[async_trait]
    impl MonitorDeliverySink for FailOnceSink {
        async fn deliver(
            &self,
            _session: &SessionId,
            report: MonitorReport,
        ) -> Result<MonitorDeliveryReceipt, MonitorError> {
            if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err(MonitorError::Delivery("injected first failure".into()));
            }
            self.reports
                .send(report)
                .map_err(|_| MonitorError::Delivery("test report receiver was dropped".into()))?;
            Ok(MonitorDeliveryReceipt {
                durable: true,
                handed_off: true,
                disposition: "captured",
            })
        }
    }

    #[async_trait]
    impl MonitorDeliverySink for CapturingSink {
        async fn deliver(
            &self,
            _session: &SessionId,
            report: MonitorReport,
        ) -> Result<MonitorDeliveryReceipt, MonitorError> {
            self.reports
                .send(report)
                .map_err(|_| MonitorError::Delivery("test report receiver was dropped".into()))?;
            Ok(MonitorDeliveryReceipt {
                durable: true,
                handed_off: true,
                disposition: "captured",
            })
        }
    }

    #[async_trait]
    impl MonitorDeliverySink for GatedSink {
        async fn deliver(
            &self,
            _session: &SessionId,
            report: MonitorReport,
        ) -> Result<MonitorDeliveryReceipt, MonitorError> {
            self.reports
                .send(report)
                .map_err(|_| MonitorError::Delivery("test report receiver was dropped".into()))?;
            if let Some(started) = self
                .started
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
            {
                let _ = started.send(());
            }
            let mut release = self.release.clone();
            while !*release.borrow() {
                release
                    .changed()
                    .await
                    .map_err(|_| MonitorError::Delivery("test delivery gate was dropped".into()))?;
            }
            Ok(MonitorDeliveryReceipt {
                durable: true,
                handed_off: true,
                disposition: "captured",
            })
        }
    }

    #[tokio::test]
    async fn register_list_remove_and_durable_readoption() {
        let world = MonitorWorld::new("crud").await;
        let monitor_id = world
            .register("register", None, MonitorOccurrence::Every)
            .await;
        let replayed_id = world
            .register("register", None, MonitorOccurrence::Every)
            .await;
        assert_eq!(replayed_id, monitor_id);
        assert!(world.wait_for_count(1).await.preview.contains(&monitor_id));
        let altered = world
            .hub
            .execute_monitor_tool(
                &world.lease,
                world.coordinates("register"),
                MonitorRequest::Register {
                    source: MonitorSource::Sms,
                    filter: None,
                    action: MonitorAction {
                        report: true,
                        follow_up: Some("different replay".into()),
                    },
                    occurrence: MonitorOccurrence::Once,
                    lifetime: MonitorLifetime::Session,
                },
            )
            .await
            .expect_err("changed replay must conflict");
        assert!(altered.to_string().contains("different arguments"));
        let listed = world.execute("list", MonitorRequest::List).await;
        assert!(listed.preview.contains(&monitor_id));

        // Drop only the projection; the next list must fold the durable facts.
        world
            .hub
            .inner_monitor()
            .inner
            .registry
            .forget_session(&world.session);
        let readopted = world.execute("readopt", MonitorRequest::List).await;
        assert!(readopted.preview.contains(&monitor_id));

        let removed = world
            .execute(
                "remove",
                MonitorRequest::Remove {
                    monitor_id: monitor_id.clone(),
                },
            )
            .await;
        assert!(removed.preview.contains("removed"));
        let replayed_remove = world
            .execute(
                "remove",
                MonitorRequest::Remove {
                    monitor_id: monitor_id.clone(),
                },
            )
            .await;
        assert_eq!(replayed_remove, removed);
        let empty = world.execute("empty", MonitorRequest::List).await;
        assert!(empty.preview.contains(r#""count":0"#));
    }

    #[tokio::test]
    async fn durable_registry_is_rebuilt_after_store_reopen() {
        let world = MonitorWorld::new("reopen").await;
        let monitor_id = world
            .register("register", None, MonitorOccurrence::Every)
            .await;
        let MonitorWorld {
            store,
            hub,
            session,
            run: _,
            lease,
            _root: root,
        } = world;
        hub.inner_monitor()
            .shutdown()
            .await
            .expect("shutdown first monitor service");
        drop(lease);
        drop(hub);
        store.close().await.expect("close first monitor store");

        let reopened_store = SqliteStoreHandle::open(root.path())
            .await
            .expect("reopen monitor store");
        let reopened_hub = SessionHub::new(
            reopened_store.clone(),
            crate::session_hub::SessionHubConfig::default(),
        )
        .expect("reopened monitor hub");
        reopened_hub
            .inner_monitor()
            .activate(reopened_hub.downgrade());
        reopened_hub
            .inner_monitor()
            .adopt_session(&reopened_hub, &session)
            .await
            .expect("adopt reopened monitor registry");
        assert_eq!(
            reopened_hub
                .inner_monitor()
                .inner
                .registry
                .snapshot(&session)
                .first()
                .map(|registration| registration.monitor_id.as_str()),
            Some(monitor_id.as_str())
        );
        reopened_hub
            .inner_monitor()
            .shutdown()
            .await
            .expect("shutdown reopened monitor service");
        drop(reopened_hub);
        reopened_store
            .close()
            .await
            .expect("close reopened monitor store");
    }

    #[tokio::test]
    async fn failed_durable_delete_rollback_restores_registry_timeout_and_pending_delivery() {
        let world = MonitorWorld::new("delete-rollback").await;
        let monitor_id = world
            .register_with_lifetime(
                "register",
                None,
                MonitorOccurrence::Every,
                MonitorLifetime::Timeout { timeout_ms: 60_000 },
            )
            .await;
        let (blocked_reports, mut blocked_report) = tokio_mpsc::unbounded_channel();
        let (started, delivery_started) = tokio_oneshot::channel();
        let (_release, release_gate) = tokio_watch::channel(false);
        world.install_canonical_test_sink(Arc::new(GatedSink {
            reports: blocked_reports,
            started: StdMutex::new(Some(started)),
            release: release_gate,
        }));
        publish_sms_incoming(
            &world.hub.monitor_source_hub(),
            "+1",
            "pending across failed delete",
            1,
        )
        .expect("publish pending delete event");
        timeout(Duration::from_secs(3), delivery_started)
            .await
            .expect("pending delivery did not start")
            .expect("pending delivery start sender dropped");
        let original = blocked_report.recv().await.expect("blocked pending report");

        // This is the exact monitor transaction around SessionHub's durable
        // delete call: forget first, then restore because that call failed.
        // The SQLite store remains intact, as it does on a failed delete.
        world
            .hub
            .inner_monitor()
            .forget_session(&world.hub, &world.session)
            .await
            .expect("forget monitors before simulated delete failure");
        assert!(
            world
                .hub
                .inner_monitor()
                .inner
                .registry
                .get(&world.session, &monitor_id)
                .is_none()
        );

        let (restored_reports, mut restored_report) = tokio_mpsc::unbounded_channel();
        world.install_canonical_test_sink(Arc::new(CapturingSink {
            reports: restored_reports,
        }));
        world
            .hub
            .inner_monitor()
            .restore_session(&world.hub, &world.session)
            .await
            .expect("restore monitors after simulated delete failure");
        assert!(
            world
                .hub
                .inner_monitor()
                .inner
                .registry
                .get(&world.session, &monitor_id)
                .is_some()
        );
        assert!(
            world
                .hub
                .inner_monitor()
                .inner
                .timeout_tasks
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .contains_key(&(world.session.clone(), monitor_id.clone()))
        );
        let restored = timeout(Duration::from_secs(3), restored_report.recv())
            .await
            .expect("restored pending delivery timeout")
            .expect("restored pending delivery");
        assert_eq!(restored.report_id, original.report_id);
        assert!(restored.events.iter().any(|event| {
            matches!(&event.payload, MonitorEventPayload::Sms(sms) if sms.body == "pending across failed delete")
        }));
    }

    #[tokio::test]
    async fn published_sms_wakes_a_normal_durable_turn_through_default_sink() {
        let world = MonitorWorld::new("wake").await;
        let monitor_id = world
            .register("register", None, MonitorOccurrence::Every)
            .await;
        let receipt = publish_sms_incoming(
            &world.hub.monitor_source_hub(),
            "+15551212",
            "deploy completed",
            99,
        )
        .expect("publish SMS monitor event");
        assert_eq!(receipt.subscriber_count, 1);

        timeout(Duration::from_secs(3), async {
            loop {
                let events = world
                    .store
                    .read(&world.session, 0, 512)
                    .await
                    .expect("read monitor wake journal");
                if events.into_iter().any(|event| {
                    serde_json::from_value::<EventPayload>(event.payload).is_ok_and(|payload| {
                        matches!(
                            payload,
                            EventPayload::UserMessage { text, .. }
                                if text.contains("monitor_event") && text.contains(&monitor_id)
                        )
                    })
                }) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("monitor wake was not durably accepted");
    }

    #[tokio::test]
    async fn named_branch_waiting_run_is_woken_as_a_subturn() {
        let world = MonitorWorld::new("named-waiting").await;
        let mut done = [monitor_envelope(
            &world.session,
            Some(&world.run),
            None,
            None,
            "monitor-named-source-done",
            DeviceId::new("monitor-named-device"),
            world.store.worker_generation(),
            serde_json::to_value(EventPayload::RunState(RunState::Done))
                .expect("encode source done"),
        )];
        world
            .lease
            .append(&mut done)
            .await
            .expect("finish source run");
        let source_events = world
            .store
            .read(&world.session, 0, 128)
            .await
            .expect("read source history");
        let (fork_node_id, fork_seq) = source_events
            .iter()
            .find_map(|event| {
                let EventPayload::NodeCommitted(node) =
                    serde_json::from_value::<EventPayload>(event.payload.clone()).ok()?
                else {
                    return None;
                };
                (event.run_id.as_ref() == Some(&world.run)).then_some((node.node, event.seq))
            })
            .expect("source fork node");
        let branch_id = BranchId::new("monitor-named-branch");
        let branch_request = r#"{"fork":"monitor-source"}"#.to_owned();
        world
            .store
            .create_branch(BranchCreateCommand {
                command_id: "monitor-create-named-branch".into(),
                request_digest: blake3::hash(branch_request.as_bytes()).to_hex().to_string(),
                request_json: branch_request,
                session_id: world.session.clone(),
                worker_generation: world.store.worker_generation(),
                branch_id: branch_id.clone(),
                source_branch_id: None,
                fork_node_id,
                fork_seq,
                name: Some("Monitor branch".into()),
                event_id: EventId::new("monitor-named-branch-created"),
                device_id: DeviceId::new("monitor-named-device"),
            })
            .await
            .expect("create monitor branch");
        let branch_run = RunId::new("monitor-named-branch-run");
        world
            .hub
            .accept_internal_turn(TurnAcceptCommand {
                command_id: "monitor-accept-named-run".into(),
                request_digest: "monitor-accept-named-run-digest".into(),
                request_json: r#"{"turn":"named"}"#.into(),
                session_id: world.session.clone(),
                worker_generation: world.store.worker_generation(),
                run_id: branch_run.clone(),
                agent_id: None,
                branch_id: Some(branch_id.clone()),
                text: "work on named branch".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
                queued_event_id: EventId::new("monitor-named-queued"),
                user_event_id: EventId::new("monitor-named-user"),
                active_event_id: EventId::new("monitor-named-active"),
                device_id: DeviceId::new("monitor-named-device"),
            })
            .await
            .expect("accept named run");
        let branch_lease = world
            .hub
            .acquire_worker_lease(world.session.clone())
            .await
            .expect("named branch lease");
        let mut waiting = [monitor_envelope(
            &world.session,
            Some(&branch_run),
            Some(&branch_id),
            None,
            "monitor-named-waiting",
            DeviceId::new("monitor-named-device"),
            world.store.worker_generation(),
            serde_json::to_value(EventPayload::RunState(RunState::Waiting {
                reason: WaitReason::Dependency,
            }))
            .expect("encode waiting state"),
        )];
        branch_lease
            .append(&mut waiting)
            .await
            .expect("park named run");
        let report = MonitorReport {
            report_id: "monitor-report-named-waiting".into(),
            monitor_id: "monitor-named".into(),
            session_id: world.session.clone(),
            branch_id: Some(branch_id.clone()),
            agent_id: None,
            source: MonitorSourceKind::Sms,
            status: MonitorReportStatus::Matched,
            events: vec![sms("+1", "wake named")],
            coalesced_count: 1,
            omitted_count: 0,
            action: MonitorAction {
                report: true,
                follow_up: None,
            },
        };
        let error = world
            .hub
            .wake_monitor_report(report)
            .await
            .expect_err("missing manager must retain the subturn for retry");
        assert!(
            error
                .to_string()
                .contains("could not reach the worker manager")
        );
        let events = world
            .store
            .read(&world.session, 0, 256)
            .await
            .expect("read named wake");
        assert!(events.into_iter().any(|event| {
            event.run_id.as_ref() == Some(&branch_run)
                && serde_json::from_value::<EventPayload>(event.payload).is_ok_and(|payload| {
                    matches!(
                        payload,
                        EventPayload::UserMessage { text, mode, .. }
                            if mode == DeliveryMode::Subturn && text.contains("monitor_event")
                    )
                })
        }));
    }

    #[tokio::test]
    async fn matching_bursts_coalesce_and_a_firehose_auto_stops() {
        let world = MonitorWorld::new("rate").await;
        world
            .register("register", None, MonitorOccurrence::Every)
            .await;
        let (reports, mut received) = tokio_mpsc::unbounded_channel();
        world.install_canonical_test_sink(Arc::new(CapturingSink { reports }));
        let sources = world.hub.monitor_source_hub();
        for index in 0..3 {
            publish_sms_incoming(&sources, "+1", &format!("burst-{index}"), index)
                .expect("publish coalesced SMS");
        }
        let first = timeout(Duration::from_secs(3), received.recv())
            .await
            .expect("coalesced report timeout")
            .expect("coalesced report");
        assert_eq!(first.status, MonitorReportStatus::Matched);
        assert_eq!(first.coalesced_count, 3);

        for index in 0..62 {
            publish_sms_incoming(&sources, "+1", &format!("firehose-{index}"), 100 + index)
                .expect("publish firehose SMS");
        }
        let stopped = timeout(Duration::from_secs(3), received.recv())
            .await
            .expect("rate-limit report timeout")
            .expect("rate-limit report");
        assert_eq!(stopped.status, MonitorReportStatus::RateLimited);
        assert_eq!(stopped.coalesced_count, 62);
        let empty = world.wait_for_count(0).await;
        assert!(empty.preview.contains(r#""count":0"#));
    }

    #[tokio::test]
    async fn failed_delivery_retries_the_same_durable_report() {
        let world = MonitorWorld::new("delivery-retry").await;
        let monitor_id = world
            .register("register", None, MonitorOccurrence::Every)
            .await;
        let attempts = Arc::new(AtomicUsize::new(0));
        let (reports, mut received) = tokio_mpsc::unbounded_channel();
        world.install_canonical_test_sink(Arc::new(FailOnceSink {
            attempts: Arc::clone(&attempts),
            reports,
        }));
        publish_sms_incoming(&world.hub.monitor_source_hub(), "+1", "retry me", 1)
            .expect("publish retry event");
        let report = timeout(Duration::from_secs(4), received.recv())
            .await
            .expect("retried report timeout")
            .expect("retried report");
        assert_eq!(report.monitor_id, monitor_id);
        assert_eq!(report.coalesced_count, 1);
        assert!(attempts.load(Ordering::SeqCst) >= 2);
        timeout(Duration::from_secs(3), async {
            loop {
                if world
                    .hub
                    .inner_monitor()
                    .inner
                    .registry
                    .pending_summary(&world.session, &monitor_id)
                    .0
                    == 0
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("delivered outbox report was not acknowledged");
        world.wait_for_count(1).await;
    }

    #[tokio::test]
    async fn stalled_delivery_retains_a_bounded_follow_up_occurrence() {
        let world = MonitorWorld::new("delivery-follow-up").await;
        let monitor_id = world
            .register("register", None, MonitorOccurrence::Every)
            .await;
        let (reports, mut received) = tokio_mpsc::unbounded_channel();
        let (started, delivery_started) = tokio_oneshot::channel();
        let (release, release_gate) = tokio_watch::channel(false);
        world.install_canonical_test_sink(Arc::new(GatedSink {
            reports,
            started: StdMutex::new(Some(started)),
            release: release_gate,
        }));
        let sources = world.hub.monitor_source_hub();
        publish_sms_incoming(&sources, "+1", "first occurrence", 1)
            .expect("publish first occurrence");
        timeout(Duration::from_secs(3), delivery_started)
            .await
            .expect("first delivery did not start")
            .expect("first delivery start sender dropped");

        // This event lands after the source coalescing window while the first
        // durable delivery remains blocked. It must occupy the bounded
        // follow-up slot rather than disappear as AlreadyPending.
        publish_sms_incoming(&sources, "+1", "second occurrence", 2)
            .expect("publish second occurrence");
        timeout(Duration::from_secs(3), async {
            loop {
                if world
                    .hub
                    .inner_monitor()
                    .inner
                    .registry
                    .pending_summary(&world.session, &monitor_id)
                    .0
                    == 2
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("follow-up occurrence did not enter the durable outbox");

        release.send_replace(true);
        let first = timeout(Duration::from_secs(3), received.recv())
            .await
            .expect("first report timeout")
            .expect("first report");
        let second = timeout(Duration::from_secs(3), received.recv())
            .await
            .expect("follow-up report timeout")
            .expect("follow-up report");
        assert!(first.events.iter().any(|event| {
            matches!(&event.payload, MonitorEventPayload::Sms(sms) if sms.body == "first occurrence")
        }));
        assert!(second.events.iter().any(|event| {
            matches!(&event.payload, MonitorEventPayload::Sms(sms) if sms.body == "second occurrence")
        }));
        timeout(Duration::from_secs(3), async {
            loop {
                if world
                    .hub
                    .inner_monitor()
                    .inner
                    .registry
                    .pending_summary(&world.session, &monitor_id)
                    .0
                    == 0
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("follow-up report was not acknowledged");
    }

    #[tokio::test]
    async fn pre_expiry_event_is_classified_before_timeout_via_source_watermark() {
        let world = MonitorWorld::new("expiry-watermark").await;
        let monitor_id = world
            .register("register", None, MonitorOccurrence::Every)
            .await;
        let sources = world.hub.monitor_source_hub();
        let mut registration = world
            .hub
            .inner_monitor()
            .inner
            .registry
            .get(&world.session, &monitor_id)
            .expect("registered monitor");
        registration.created_at_ms = now_ms();
        registration.start_sequence = sources.current_sequence();
        registration.expires_at_ms = Some(now_ms().saturating_add(100));
        world
            .hub
            .inner_monitor()
            .inner
            .registry
            .insert(&world.session, registration.clone());
        world.hub.inner_monitor().schedule_timeout(
            world.hub.downgrade(),
            world.session.clone(),
            registration,
        );
        let (reports, mut received) = tokio_mpsc::unbounded_channel();
        world.install_canonical_test_sink(Arc::new(CapturingSink { reports }));
        publish_sms_incoming(&sources, "+1", "just before expiry", 1)
            .expect("publish pre-expiry event");

        // Source classification intentionally waits 250ms, past the 100ms
        // deadline. The timeout worker's explicit source watermark must keep
        // the earlier event in front of the terminal report.
        let matched = timeout(Duration::from_secs(3), received.recv())
            .await
            .expect("matched report timeout")
            .expect("matched report");
        let timed_out = timeout(Duration::from_secs(3), received.recv())
            .await
            .expect("timeout report timeout")
            .expect("timeout report");
        assert_eq!(matched.status, MonitorReportStatus::Matched);
        assert!(matched.events.iter().any(|event| {
            matches!(&event.payload, MonitorEventPayload::Sms(sms) if sms.body == "just before expiry")
        }));
        assert_eq!(timed_out.status, MonitorReportStatus::TimedOut);
    }

    #[tokio::test]
    async fn once_stops_after_delivery_while_every_remains_active() {
        let world = MonitorWorld::new("occurrence").await;
        let once = world.register("once", None, MonitorOccurrence::Once).await;
        let every = world
            .register("every", None, MonitorOccurrence::Every)
            .await;
        let (reports, mut received) = tokio_mpsc::unbounded_channel();
        world.install_canonical_test_sink(Arc::new(CapturingSink { reports }));
        publish_sms_incoming(&world.hub.monitor_source_hub(), "+1", "event", 1)
            .expect("publish occurrence event");
        let first = timeout(Duration::from_secs(3), received.recv())
            .await
            .expect("first occurrence report timeout")
            .expect("first occurrence report");
        let second = timeout(Duration::from_secs(3), received.recv())
            .await
            .expect("second occurrence report timeout")
            .expect("second occurrence report");
        assert!(
            [first.monitor_id, second.monitor_id]
                .iter()
                .any(|monitor| monitor == &once)
        );
        let remaining = world.wait_for_count(1).await;
        assert!(remaining.preview.contains(&every));
        assert!(!remaining.preview.contains(&once));
    }

    #[tokio::test]
    async fn timeout_reports_and_stops_while_session_lifetime_persists() {
        let timed = MonitorWorld::new("timeout").await;
        let (reports, mut received) = tokio_mpsc::unbounded_channel();
        timed.install_canonical_test_sink(Arc::new(CapturingSink { reports }));
        timed
            .register_with_lifetime(
                "timed",
                None,
                MonitorOccurrence::Every,
                MonitorLifetime::Timeout { timeout_ms: 100 },
            )
            .await;
        let report = timeout(Duration::from_secs(3), received.recv())
            .await
            .expect("timeout report wait")
            .expect("timeout report");
        assert_eq!(report.status, MonitorReportStatus::TimedOut);
        assert_eq!(report.coalesced_count, 0);
        timed.wait_for_count(0).await;

        let persistent = MonitorWorld::new("session-lifetime").await;
        let monitor_id = persistent
            .register("persistent", None, MonitorOccurrence::Every)
            .await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            persistent
                .wait_for_count(1)
                .await
                .preview
                .contains(&monitor_id)
        );
    }

    #[tokio::test]
    async fn bounded_registry_and_filter_matching() {
        let world = MonitorWorld::new("bounds-filter").await;
        let filter = MonitorFilter {
            field: MonitorFilterField::Body,
            operator: MonitorFilterOperator::Contains,
            value: "ship".into(),
            case_sensitive: false,
        };
        world
            .register("filtered", Some(filter), MonitorOccurrence::Every)
            .await;
        let (reports, mut received) = tokio_mpsc::unbounded_channel();
        world.install_canonical_test_sink(Arc::new(CapturingSink { reports }));
        let sources = world.hub.monitor_source_hub();
        publish_sms_incoming(&sources, "+1", "ignore", 1).expect("publish mismatch");
        tokio::time::sleep(MONITOR_COALESCE_WINDOW + Duration::from_millis(50)).await;
        assert!(received.try_recv().is_err());
        publish_sms_incoming(&sources, "+1", "SHIP it", 2).expect("publish match");
        let matched = timeout(Duration::from_secs(3), received.recv())
            .await
            .expect("filter match timeout")
            .expect("filter match report");
        assert_eq!(matched.coalesced_count, 1);

        for index in 1..MAX_MONITORS_PER_SESSION {
            world
                .register(&format!("fill-{index}"), None, MonitorOccurrence::Every)
                .await;
        }
        let overflow = world
            .execute(
                "overflow",
                MonitorRequest::Register {
                    source: MonitorSource::Sms,
                    filter: None,
                    action: MonitorAction {
                        report: true,
                        follow_up: None,
                    },
                    occurrence: MonitorOccurrence::Every,
                    lifetime: MonitorLifetime::Session,
                },
            )
            .await;
        assert_eq!(overflow.status, ToolResultStatus::Rejected);
        assert!(overflow.preview.contains("limit_reached"));
    }
}
