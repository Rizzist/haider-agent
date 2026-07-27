//! W3b2 session-hub semantic acceptance matrix.
//!
//! These are integration tests because the hub's public worker/connection
//! seams are sufficient. No crate-internal test module is needed.

#![allow(clippy::expect_used)]

use haider_core::{HarnessActor, HarnessConfig, SqliteStoreHandle, StoreHandle, SubmitTurn};
use haider_daemon::{
    AdmissionTicket, FrameSendError, FrameSink, HubConnection, HubObservation, SendAdmission,
    SessionHub, SessionHubConfig, SessionHubObserver, SessionHubShutdownOutcome,
};
use haider_protocol::EventPayload;
use haider_protocol::envelope::{
    EventEnvelope, PromptRender, RawEnvelope, RenderTargets, SCHEMA_VERSION,
};
use haider_protocol::ids::{DeviceId, EventId, MenuId, SessionId};
use haider_protocol::menu::{Menu, MenuKind, MenuOption, MenuScope};
use haider_protocol::provider::FinishReason;
use haider_protocol::state::RunState;
use haider_provider::{FakeInputKind, FakeInputOption, FakeProvider, FakeStep};
use haider_rpc::{
    AttachMode, AttachmentId, Capability, CapabilitySet, CommandId, ErrorData, RequestBody,
    RequestId, ResponseBody, SeqRange, WireFrame,
};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio::sync::{Notify, mpsc};

const DEADLINE: Duration = Duration::from_secs(10);

fn envelope(
    session_id: &SessionId,
    event_id: impl Into<String>,
    worker_generation: u64,
) -> RawEnvelope {
    EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(event_id),
        seq: 0,
        session_id: session_id.clone(),
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: DeviceId::new("hub-test"),
        authority_epoch: 11,
        worker_generation,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: serde_json::json!({"type": "future_test_event"}),
    }
}

fn menu_opening(session_id: &SessionId, menu_id: &MenuId, worker_generation: u64) -> RawEnvelope {
    let mut envelope = envelope(session_id, "menu-opened", worker_generation);
    envelope.payload = serde_json::to_value(EventPayload::MenuOpened(Menu {
        id: menu_id.clone(),
        kind: MenuKind::Choice,
        title: "Choose".into(),
        body: Vec::new(),
        options: vec![
            MenuOption {
                key: "allow".into(),
                label: "Allow".into(),
                detail: None,
                decision: None,
            },
            MenuOption {
                key: "deny".into(),
                label: "Deny".into(),
                detail: None,
                decision: None,
            },
        ],
        blocking: true,
        scope: MenuScope::Session,
        origin: "hub-test".into(),
        ttl_ms: None,
        timeout_option: None,
    }))
    .expect("menu serializes");
    envelope
}

#[derive(Default)]
struct CollectSink {
    frames: Mutex<VecDeque<WireFrame>>,
    changed: Notify,
}

impl CollectSink {
    async fn next(&self) -> WireFrame {
        tokio::time::timeout(DEADLINE, async {
            loop {
                let changed = self.changed.notified();
                if let Some(frame) = self.frames.lock().expect("frames lock").pop_front() {
                    return frame;
                }
                changed.await;
            }
        })
        .await
        .expect("frame deadline")
    }

    fn snapshot(&self) -> Vec<WireFrame> {
        self.frames
            .lock()
            .expect("frames lock")
            .iter()
            .cloned()
            .collect()
    }
}

impl FrameSink for CollectSink {
    fn try_send(&self, frame: WireFrame) -> Result<(), FrameSendError> {
        self.frames
            .lock()
            .map_err(|_| FrameSendError)?
            .push_back(frame);
        self.changed.notify_waiters();
        Ok(())
    }

    fn purge_attachment(&self, attachment_id: &AttachmentId) -> Option<RequestId> {
        let mut purged_response = None;
        if let Ok(mut frames) = self.frames.lock() {
            frames.retain(|frame| match frame {
                WireFrame::Event {
                    attachment_id: queued,
                    ..
                }
                | WireFrame::AttachCaughtUp {
                    attachment_id: queued,
                    ..
                }
                | WireFrame::Lagged {
                    attachment_id: queued,
                    ..
                } => queued != attachment_id,
                WireFrame::Response {
                    request_id,
                    body:
                        ResponseBody::SessionAttach {
                            attachment_id: queued,
                            ..
                        },
                } if queued == attachment_id => {
                    // Mirrors the real sink: an undelivered staged attach
                    // response is removed and reported.
                    purged_response = Some(request_id.clone());
                    false
                }
                _ => true,
            });
        }
        purged_response
    }
}

struct SlowSink {
    frames: Mutex<VecDeque<WireFrame>>,
    changed: Notify,
    event_budget: usize,
    accepted_events: Mutex<usize>,
}

#[derive(Default)]
struct LostSuccessSink {
    collected: CollectSink,
    refused_success: Mutex<bool>,
}

impl FrameSink for LostSuccessSink {
    fn try_send(&self, frame: WireFrame) -> Result<(), FrameSendError> {
        if matches!(
            frame,
            WireFrame::Response {
                body: ResponseBody::MenuAnswer { .. },
                ..
            }
        ) {
            let mut refused = self.refused_success.lock().map_err(|_| FrameSendError)?;
            if !*refused {
                *refused = true;
                return Err(FrameSendError);
            }
        }
        self.collected.try_send(frame)
    }

    fn purge_attachment(&self, attachment_id: &AttachmentId) -> Option<RequestId> {
        self.collected.purge_attachment(attachment_id)
    }
}

impl SlowSink {
    fn new(event_budget: usize) -> Self {
        Self {
            frames: Mutex::new(VecDeque::new()),
            changed: Notify::new(),
            event_budget,
            accepted_events: Mutex::new(0),
        }
    }

    async fn next(&self) -> WireFrame {
        tokio::time::timeout(DEADLINE, async {
            loop {
                let changed = self.changed.notified();
                if let Some(frame) = self.frames.lock().expect("frames lock").pop_front() {
                    return frame;
                }
                changed.await;
            }
        })
        .await
        .expect("slow-sink frame deadline")
    }
}

impl FrameSink for SlowSink {
    fn try_send(&self, frame: WireFrame) -> Result<(), FrameSendError> {
        if matches!(frame, WireFrame::Event { .. }) {
            let mut accepted = self.accepted_events.lock().map_err(|_| FrameSendError)?;
            if *accepted >= self.event_budget {
                return Err(FrameSendError);
            }
            *accepted = accepted.saturating_add(1);
        }
        self.frames
            .lock()
            .map_err(|_| FrameSendError)?
            .push_back(frame);
        self.changed.notify_waiters();
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
enum GateTarget {
    BeforeEvent(u64),
    Persisted(u64),
    ReceiverRegistered,
    ReplayEvent(u64),
    BeforeCaughtUp,
    BufferedEvent(u64),
    FinalSuffixHeadCaptured(u64),
}

impl GateTarget {
    fn matches(self, observation: &HubObservation) -> bool {
        match (self, observation) {
            (Self::ReceiverRegistered, HubObservation::ReceiverRegistered { .. })
            | (Self::BeforeCaughtUp, HubObservation::BeforeCaughtUp { .. }) => true,
            (Self::BeforeEvent(expected), HubObservation::BeforeEvent { seq, .. })
            | (Self::ReplayEvent(expected), HubObservation::ReplayEvent { seq, .. })
            | (Self::BufferedEvent(expected), HubObservation::BufferedEvent { seq, .. })
            | (
                Self::FinalSuffixHeadCaptured(expected),
                HubObservation::FinalSuffixHeadCaptured { head: seq, .. },
            )
            | (
                Self::Persisted(expected),
                HubObservation::Persisted {
                    through_seq: seq, ..
                },
            ) => expected == *seq,
            _ => false,
        }
    }
}

struct GateObserver {
    targets: Mutex<VecDeque<GateTarget>>,
    reached: mpsc::UnboundedSender<HubObservation>,
    observed: mpsc::UnboundedSender<HubObservation>,
    releases: Mutex<std::sync::mpsc::Receiver<()>>,
}

impl SessionHubObserver for GateObserver {
    fn observe(&self, observation: HubObservation) {
        let _ = self.observed.send(observation.clone());
        let should_gate = self
            .targets
            .lock()
            .expect("targets lock")
            .front()
            .is_some_and(|target| target.matches(&observation));
        if !should_gate {
            return;
        }
        self.targets.lock().expect("targets lock").pop_front();
        let _ = self.reached.send(observation);
        self.releases
            .lock()
            .expect("release lock")
            .recv()
            .expect("test releases gated boundary");
    }
}

struct ObserverControl {
    reached: mpsc::UnboundedReceiver<HubObservation>,
    observed: mpsc::UnboundedReceiver<HubObservation>,
    release: std::sync::mpsc::Sender<()>,
}

impl ObserverControl {
    async fn reached(&mut self) -> HubObservation {
        tokio::time::timeout(DEADLINE, self.reached.recv())
            .await
            .expect("boundary deadline")
            .expect("observer remains open")
    }

    fn release(&self) {
        self.release.send(()).expect("release observer");
    }

    fn discard_observed(&mut self) {
        while self.observed.try_recv().is_ok() {}
    }

    async fn observed_append(&mut self, session_id: &SessionId) {
        tokio::time::timeout(DEADLINE, async {
            loop {
                if matches!(
                    self.observed.recv().await,
                    Some(HubObservation::AppendEnqueued {
                        session_id: observed,
                    }) if observed == *session_id
                ) {
                    return;
                }
            }
        })
        .await
        .expect("append enqueue observation deadline");
    }

    async fn observed_shutdown_guarded(&mut self) {
        tokio::time::timeout(DEADLINE, async {
            loop {
                if matches!(
                    self.observed.recv().await,
                    Some(HubObservation::ShutdownGuarded)
                ) {
                    return;
                }
            }
        })
        .await
        .expect("shutdown guard observation deadline");
    }
}

fn gated_observer(targets: Vec<GateTarget>) -> (Arc<GateObserver>, ObserverControl) {
    let (reached, reached_rx) = mpsc::unbounded_channel();
    let (observed, observed_rx) = mpsc::unbounded_channel();
    let (release, releases) = std::sync::mpsc::channel();
    (
        Arc::new(GateObserver {
            targets: Mutex::new(targets.into()),
            reached,
            observed,
            releases: Mutex::new(releases),
        }),
        ObserverControl {
            reached: reached_rx,
            observed: observed_rx,
            release,
        },
    )
}

async fn open_hub(
    observer: Option<Arc<dyn SessionHubObserver>>,
    catch_up_capacity: usize,
) -> (tempfile::TempDir, SqliteStoreHandle, SessionHub) {
    open_hub_with_capacities(observer, 64, catch_up_capacity).await
}

async fn open_hub_with_capacities(
    observer: Option<Arc<dyn SessionHubObserver>>,
    actor_command_capacity: usize,
    catch_up_capacity: usize,
) -> (tempfile::TempDir, SqliteStoreHandle, SessionHub) {
    let config = SessionHubConfig {
        actor_command_capacity,
        catch_up_capacity,
        ..SessionHubConfig::default()
    };
    open_hub_with_config(observer, config).await
}

async fn open_hub_with_config(
    observer: Option<Arc<dyn SessionHubObserver>>,
    config: SessionHubConfig,
) -> (tempfile::TempDir, SqliteStoreHandle, SessionHub) {
    let root = tempfile::tempdir().expect("temp store");
    let store = SqliteStoreHandle::open(root.path())
        .await
        .expect("store opens");
    let hub = match observer {
        Some(observer) => {
            SessionHub::with_observer(store.clone(), config, observer).expect("hub opens")
        }
        None => SessionHub::new(store.clone(), config).expect("hub opens"),
    };
    (root, store, hub)
}

async fn append_one(
    hub: &SessionHub,
    session_id: &SessionId,
    worker_generation: u64,
    event_id: &str,
) -> u64 {
    let mut event = vec![envelope(session_id, event_id, worker_generation)];
    hub.append(&mut event).await.expect("append commits");
    event[0].seq
}

fn capabilities() -> CapabilitySet {
    CapabilitySet::from([Capability::View, Capability::Control])
}

fn attachment_from(frame: WireFrame) -> (AttachmentId, u64) {
    let WireFrame::Response {
        body:
            ResponseBody::SessionAttach {
                attachment_id,
                attach_state,
            },
        ..
    } = frame
    else {
        panic!("expected attach response");
    };
    (attachment_id, attach_state.replay_through_seq)
}

/// Forces an append at every §5.5 boundary: before registration, in the
/// impossible registration→H gap, during replay, at H, immediately after H,
/// before caught-up, and during buffered drain.
///
/// MUTATION CHECK: split receiver registration and `head` capture into two
/// actor commands (or publish before store append). Expected failure: the
/// attach H becomes 3 or the delivered sequence is non-contiguous/out of
/// replay→caught-up→buffered order.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replay_live_barrier_is_contiguous_at_every_forced_boundary() {
    let (observer, mut control) = gated_observer(vec![
        GateTarget::ReceiverRegistered,
        GateTarget::ReplayEvent(1),
        GateTarget::BeforeCaughtUp,
        GateTarget::BufferedEvent(3),
    ]);
    let (_root, store, hub) = open_hub(Some(observer), 16).await;
    let session_id = SessionId::new("all-boundaries");
    let generation = store.worker_generation();
    let mut initial = vec![
        envelope(&session_id, "before-registration", generation),
        envelope(&session_id, "exactly-at-h", generation),
    ];
    hub.append(&mut initial).await.expect("initial history");

    let sink = Arc::new(CollectSink::default());
    let connection = Arc::new(
        hub.open_connection(capabilities(), sink.clone())
            .expect("connection opens"),
    );
    let attach = tokio::spawn({
        let connection = Arc::clone(&connection);
        let session_id = session_id.clone();
        async move {
            connection
                .request(
                    RequestId::new("attach"),
                    RequestBody::SessionAttach {
                        session_id,
                        after_seq: 0,
                        mode: AttachMode::View,
                    },
                )
                .await
        }
    });

    assert!(matches!(
        control.reached().await,
        HubObservation::ReceiverRegistered { .. }
    ));
    control.discard_observed();
    let immediately_after_h = tokio::spawn({
        let hub = hub.clone();
        let session_id = session_id.clone();
        async move { append_one(&hub, &session_id, generation, "immediately-after-h").await }
    });
    // Real-state synchronization: the append command is demonstrably queued
    // behind the actor turn whose receiver insertion has happened but whose
    // adjacent H capture is deliberately gated.
    control.observed_append(&session_id).await;
    control.release();

    assert!(matches!(
        control.reached().await,
        HubObservation::ReplayEvent { seq: 1, .. }
    ));
    assert_eq!(immediately_after_h.await.expect("append joins"), 3);
    assert_eq!(
        append_one(&hub, &session_id, generation, "during-replay").await,
        4
    );
    control.release();

    assert!(matches!(
        control.reached().await,
        HubObservation::BeforeCaughtUp { through_seq: 2, .. }
    ));
    assert_eq!(
        append_one(&hub, &session_id, generation, "before-caught-up").await,
        5
    );
    control.release();

    assert!(matches!(
        control.reached().await,
        HubObservation::BufferedEvent { seq: 3, .. }
    ));
    assert_eq!(
        append_one(&hub, &session_id, generation, "during-buffered-drain").await,
        6
    );
    control.release();
    attach
        .await
        .expect("attach task joins")
        .expect("attach succeeds");

    let (attachment_id, captured_h) = attachment_from(sink.next().await);
    assert_eq!(captured_h, 2, "the registration→H gap admitted no append");
    let mut event_seqs = Vec::new();
    let mut caught_up_index = None;
    while event_seqs.len() < 6 || caught_up_index.is_none() {
        match sink.next().await {
            WireFrame::Event {
                attachment_id: found,
                envelope,
                ..
            } => {
                assert_eq!(found, attachment_id);
                event_seqs.push(envelope.seq);
            }
            WireFrame::AttachCaughtUp {
                attachment_id: found,
                high_water_seq,
            } => {
                assert_eq!(found, attachment_id);
                assert_eq!(high_water_seq, 2);
                caught_up_index = Some(event_seqs.len());
            }
            frame => panic!("unexpected frame: {frame:?}"),
        }
    }
    assert_eq!(event_seqs, [1, 2, 3, 4, 5, 6]);
    assert_eq!(caught_up_index, Some(2));

    connection.close().await.expect("connection closes");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// MUTATION CHECK: treat a full actor catch-up receiver as terminal, or omit
/// the actor-ordered re-registration and store replay. Expected failure: seq 4
/// never arrives after the one-slot receiver overflows.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_internal_catch_up_receiver_reregisters_and_resumes_from_store() {
    let (observer, mut control) = gated_observer(vec![GateTarget::ReplayEvent(1)]);
    let (_root, store, hub) = open_hub(Some(observer), 1).await;
    let session_id = SessionId::new("catch-up-overflow");
    let generation = store.worker_generation();
    append_one(&hub, &session_id, generation, "initial-1").await;
    append_one(&hub, &session_id, generation, "initial-2").await;

    let sink = Arc::new(CollectSink::default());
    let connection = hub
        .open_connection(capabilities(), sink.clone())
        .expect("connection");
    connection
        .request(
            RequestId::new("attach"),
            RequestBody::SessionAttach {
                session_id: session_id.clone(),
                after_seq: 0,
                mode: AttachMode::View,
            },
        )
        .await
        .expect("attach routes");
    assert!(matches!(
        control.reached().await,
        HubObservation::ReplayEvent { seq: 1, .. }
    ));

    append_one(&hub, &session_id, generation, "fills-one-slot-buffer").await;
    append_one(&hub, &session_id, generation, "overflows-one-slot-buffer").await;
    control.release();

    let mut delivered = Vec::new();
    loop {
        match sink.next().await {
            WireFrame::Event { envelope, .. } => delivered.push(envelope.seq),
            WireFrame::AttachCaughtUp {
                high_water_seq: 4, ..
            } => break,
            WireFrame::Response { .. } | WireFrame::AttachCaughtUp { .. } => {}
            frame => panic!("unexpected catch-up frame: {frame:?}"),
        }
    }
    assert_eq!(delivered, [1, 2, 3, 4]);

    connection.close().await.expect("connection closes");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// MUTATION CHECK: replace the `cursor_ahead` branch with an empty replay.
/// Expected failure: the response loses its typed requested/head recovery
/// coordinates and this shape assertion fails.
#[tokio::test]
async fn cursor_ahead_is_correlated_and_carries_recovery_coordinates() {
    let (_root, store, hub) = open_hub(None, 8).await;
    let session_id = SessionId::new("cursor-ahead");
    append_one(&hub, &session_id, store.worker_generation(), "one").await;
    let sink = Arc::new(CollectSink::default());
    let connection = hub
        .open_connection(capabilities(), sink.clone())
        .expect("connection");
    connection
        .request(
            RequestId::new("ahead"),
            RequestBody::SessionAttach {
                session_id,
                after_seq: 9,
                mode: AttachMode::View,
            },
        )
        .await
        .expect("request routes");
    assert!(matches!(
        sink.next().await,
        WireFrame::Response {
            request_id,
            body: ResponseBody::Error {
                ref code,
                data: Some(ErrorData::CursorAhead { requested: 9, head: 1 }),
                ..
            },
        } if request_id.as_str() == "ahead" && code == "cursor_ahead"
    ));
    connection.close().await.expect("connection closes");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// MUTATION CHECK: remove the ownership-locked delivery check or purge before
/// removing/cancelling the attachment. Expected failure: the gated event is
/// queued after purge and survives the detach acknowledgement.
/// Verified by revert on 2026-07-27.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn detach_mid_replay_purges_and_leaks_no_later_event() {
    let (observer, mut control) = gated_observer(vec![GateTarget::BeforeEvent(1)]);
    let (_root, store, hub) = open_hub(Some(observer), 8).await;
    let session_id = SessionId::new("detach-replay");
    let generation = store.worker_generation();
    for seq in 1..=8 {
        append_one(&hub, &session_id, generation, &format!("event-{seq}")).await;
    }
    let sink = Arc::new(CollectSink::default());
    let connection = Arc::new(
        hub.open_connection(capabilities(), sink.clone())
            .expect("connection"),
    );
    connection
        .request(
            RequestId::new("attach"),
            RequestBody::SessionAttach {
                session_id,
                after_seq: 0,
                mode: AttachMode::View,
            },
        )
        .await
        .expect("attach routes");
    assert!(matches!(
        control.reached().await,
        HubObservation::BeforeEvent { seq: 1, .. }
    ));
    let (attachment_id, _) = attachment_from(sink.next().await);
    connection
        .request(
            RequestId::new("detach"),
            RequestBody::SessionDetach {
                attachment_id: attachment_id.clone(),
            },
        )
        .await
        .expect("detach routes");
    control.release();
    hub.shutdown().await.expect("hub joins replay");

    let frames = sink.snapshot();
    frames
        .iter()
        .position(|frame| {
            matches!(
                frame,
                WireFrame::Response {
                    body: ResponseBody::SessionDetach { attachment_id: found },
                    ..
                } if found == &attachment_id
            )
        })
        .expect("detach response exists");
    assert!(frames.iter().all(|frame| {
        !matches!(
            frame,
            WireFrame::Event {
                attachment_id: found,
                ..
            } if found == &attachment_id
        )
    }));
    connection.close().await.expect("connection closes");
    store.close().await.expect("store closes");
}

/// MUTATION CHECK: decode list cursors as array offsets or let list/read
/// subscribe. Expected failure: insertion ordering/page continuation changes
/// or an unsolicited event appears after these non-subscribing responses.
#[tokio::test]
async fn list_uses_opaque_stable_order_cursor_and_read_does_not_subscribe() {
    let (_root, store, hub) = open_hub(None, 8).await;
    let generation = store.worker_generation();
    for name in ["session-c", "session-a", "session-b"] {
        append_one(&hub, &SessionId::new(name), generation, name).await;
    }
    let sink = Arc::new(CollectSink::default());
    let connection = hub
        .open_connection(capabilities(), sink.clone())
        .expect("connection");
    connection
        .request(
            RequestId::new("list-1"),
            RequestBody::SessionList {
                cursor: None,
                limit: 2,
            },
        )
        .await
        .expect("list routes");
    let WireFrame::Response {
        body:
            ResponseBody::SessionList {
                sessions,
                next_cursor: Some(cursor),
            },
        ..
    } = sink.next().await
    else {
        panic!("expected first list page");
    };
    assert_eq!(
        sessions
            .iter()
            .map(|summary| summary.session_id.as_str())
            .collect::<Vec<_>>(),
        ["session-a", "session-b"]
    );
    assert!(cursor.starts_with("hs1."));
    connection
        .request(
            RequestId::new("list-2"),
            RequestBody::SessionList {
                cursor: Some(cursor),
                limit: 2,
            },
        )
        .await
        .expect("list routes");
    assert!(matches!(
        sink.next().await,
        WireFrame::Response {
            body: ResponseBody::SessionList { ref sessions, next_cursor: None },
            ..
        } if sessions.len() == 1 && sessions[0].session_id.as_str() == "session-c"
    ));
    connection
        .request(
            RequestId::new("read"),
            RequestBody::SessionRead {
                session_id: SessionId::new("session-a"),
                range: SeqRange {
                    start_seq: 1,
                    end_seq: 1,
                },
            },
        )
        .await
        .expect("read routes");
    assert!(matches!(
        sink.next().await,
        WireFrame::Response {
            body: ResponseBody::SessionRead { ref result },
            ..
        } if result.envelopes.len() == 1 && result.head_seq == 1
    ));
    append_one(
        &hub,
        &SessionId::new("session-a"),
        generation,
        "non-subscribing",
    )
    .await;
    assert!(
        sink.snapshot().is_empty(),
        "list/read created no subscription"
    );

    connection.close().await.expect("connection closes");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// MUTATION CHECK: await socket capacity from the session actor or retain an
/// unbounded client history instead of detaching on outbox refusal. Expected
/// failure: appends stop completing, no `Lagged` arrives, or the resumed
/// stream is not the exact suffix after the client's applied cursor.
#[tokio::test]
async fn slow_client_is_lagged_and_store_resume_is_contiguous() {
    let (_root, store, hub) = open_hub(None, 4).await;
    let session_id = SessionId::new("slow-client");
    let generation = store.worker_generation();
    for seq in 1..=7 {
        append_one(&hub, &session_id, generation, &format!("event-{seq}")).await;
    }

    let slow = Arc::new(SlowSink::new(1));
    let connection = hub
        .open_connection(capabilities(), slow.clone())
        .expect("slow connection");
    connection
        .request(
            RequestId::new("slow-attach"),
            RequestBody::SessionAttach {
                session_id: session_id.clone(),
                after_seq: 0,
                mode: AttachMode::View,
            },
        )
        .await
        .expect("attach routes");
    let (attachment_id, _) = attachment_from(slow.next().await);
    let first = match slow.next().await {
        WireFrame::Event { envelope, .. } => envelope.seq,
        frame => panic!("expected first applied event, got {frame:?}"),
    };
    assert_eq!(first, 1);
    assert!(matches!(
        slow.next().await,
        WireFrame::Lagged {
            attachment_id: found,
            last_queued_seq: 1,
        } if found == attachment_id
    ));

    let resumed = Arc::new(CollectSink::default());
    let resumed_connection = hub
        .open_connection(capabilities(), resumed.clone())
        .expect("resume connection");
    resumed_connection
        .request(
            RequestId::new("resume"),
            RequestBody::SessionAttach {
                session_id,
                after_seq: first,
                mode: AttachMode::View,
            },
        )
        .await
        .expect("resume routes");
    let _ = attachment_from(resumed.next().await);
    let mut resumed_seqs = Vec::new();
    loop {
        match resumed.next().await {
            WireFrame::Event { envelope, .. } => resumed_seqs.push(envelope.seq),
            WireFrame::AttachCaughtUp {
                high_water_seq: 7, ..
            } => break,
            frame => panic!("unexpected resume frame: {frame:?}"),
        }
    }
    assert_eq!(resumed_seqs, [2, 3, 4, 5, 6, 7]);

    connection.close().await.expect("slow connection closes");
    resumed_connection
        .close()
        .await
        .expect("resume connection closes");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// MUTATION CHECK: key replay progress by a shared notification offset rather
/// than each attachment's `after_seq`. Expected failure: at least one cursor
/// receives the wrong suffix or a non-contiguous sequence.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_concurrent_cursors_each_receive_their_exact_suffix() {
    let (_root, store, hub) = open_hub(None, 32).await;
    let session_id = SessionId::new("many-cursors");
    let generation = store.worker_generation();
    for seq in 1..=24 {
        append_one(&hub, &session_id, generation, &format!("event-{seq}")).await;
    }

    let tasks = [0_u64, 1, 3, 7, 12, 18, 23]
        .into_iter()
        .map(|after_seq| {
            let hub = hub.clone();
            let session_id = session_id.clone();
            tokio::spawn(async move {
                let sink = Arc::new(CollectSink::default());
                let connection = hub
                    .open_connection(capabilities(), sink.clone())
                    .expect("connection");
                connection
                    .request(
                        RequestId::new(format!("cursor-{after_seq}")),
                        RequestBody::SessionAttach {
                            session_id,
                            after_seq,
                            mode: AttachMode::View,
                        },
                    )
                    .await
                    .expect("attach routes");
                let _ = attachment_from(sink.next().await);
                let mut received = Vec::new();
                loop {
                    match sink.next().await {
                        WireFrame::Event { envelope, .. } => received.push(envelope.seq),
                        WireFrame::AttachCaughtUp {
                            high_water_seq: 24, ..
                        } => break,
                        frame => panic!("unexpected cursor frame: {frame:?}"),
                    }
                }
                connection.close().await.expect("connection closes");
                (after_seq, received)
            })
        })
        .collect::<Vec<_>>();
    for task in tasks {
        let (after_seq, received) = task.await.expect("cursor task joins");
        assert_eq!(received, ((after_seq + 1)..=24).collect::<Vec<_>>());
    }

    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// MUTATION CHECK: require only the connection's negotiated `control`
/// capability and skip the control-attachment ownership check. Expected
/// failure: the unattached controller gets any error other than the correlated
/// `capability_denied` pinned here.
#[tokio::test]
async fn control_requires_a_control_attachment_to_the_target_session() {
    let (_root, store, hub) = open_hub(None, 8).await;
    let sink = Arc::new(CollectSink::default());
    let connection = hub
        .open_connection(capabilities(), sink.clone())
        .expect("connection");
    connection
        .menu_answer(
            Some(RequestId::new("answer")),
            haider_rpc::CommandId::new("command"),
            SessionId::new("not-attached"),
            haider_protocol::ids::MenuId::new("menu"),
            1,
            store.worker_generation(),
            "allow".into(),
            0,
            None,
        )
        .await
        .expect("denial is delivered");
    assert!(matches!(
        sink.next().await,
        WireFrame::Response {
            request_id,
            body: ResponseBody::Error { ref code, .. },
        } if request_id.as_str() == "answer" && code == "capability_denied"
    ));
    connection.close().await.expect("connection closes");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// MUTATION CHECK: remove/no-op `SessionHub::begin_draining`'s atomic store.
/// Expected failure: this already-open connection admits the list request
/// instead of returning `draining`.
#[tokio::test]
async fn begin_draining_synchronously_rejects_new_connection_work() {
    let (_root, store, hub) = open_hub(None, 8).await;
    let session_id = SessionId::new("drain-admission");
    append_one(&hub, &session_id, store.worker_generation(), "initial").await;
    let sink = Arc::new(CollectSink::default());
    let connection = hub
        .open_connection(capabilities(), sink.clone())
        .expect("connection opens before drain");

    hub.begin_draining();
    connection
        .request(
            RequestId::new("during-drain"),
            RequestBody::SessionList {
                cursor: None,
                limit: 10,
            },
        )
        .await
        .expect("typed drain rejection enqueues");
    assert!(matches!(
        sink.next().await,
        WireFrame::Response {
            request_id,
            body: ResponseBody::Error { ref code, .. },
        } if request_id.as_str() == "during-drain" && code == "draining"
    ));
    assert!(hub.open_connection(capabilities(), sink).is_err());

    connection.close().await.expect("connection closes");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// MUTATION CHECK: move menu arbitration outside the session actor or append
/// without the durable `menu_resolutions` CAS. Expected failure: more than one
/// success response/event appears, or loser errors disagree on
/// `resolution_seq`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn n_way_menu_answer_race_has_one_streamed_winner_and_correlated_losers() {
    let (_root, store, hub) = open_hub(None, 16).await;
    let session_id = SessionId::new("menu-race-session");
    let menu_id = MenuId::new("menu-race");
    let generation = store.worker_generation();
    let mut opening = vec![menu_opening(&session_id, &menu_id, generation)];
    hub.append(&mut opening)
        .await
        .expect("menu opening commits");
    let request_seq = opening[0].seq;

    let mut clients = Vec::new();
    for index in 0..8 {
        let sink = Arc::new(CollectSink::default());
        let connection = Arc::new(
            hub.open_connection(capabilities(), sink.clone())
                .expect("connection"),
        );
        connection
            .request(
                RequestId::new(format!("attach-{index}")),
                RequestBody::SessionAttach {
                    session_id: session_id.clone(),
                    after_seq: 0,
                    mode: AttachMode::Control,
                },
            )
            .await
            .expect("attach routes");
        let _ = attachment_from(sink.next().await);
        assert!(matches!(
            sink.next().await,
            WireFrame::Event { ref envelope, .. } if envelope.seq == request_seq
        ));
        assert!(matches!(
            sink.next().await,
            WireFrame::AttachCaughtUp { high_water_seq, .. } if high_water_seq == request_seq
        ));
        clients.push((connection, sink));
    }

    let racers = clients
        .iter()
        .enumerate()
        .map(|(index, (connection, _))| {
            let connection = Arc::clone(connection);
            let session_id = session_id.clone();
            let menu_id = menu_id.clone();
            tokio::spawn(async move {
                connection
                    .menu_answer(
                        Some(RequestId::new(format!("answer-{index}"))),
                        CommandId::new(format!("command-{index}")),
                        session_id,
                        menu_id,
                        request_seq,
                        generation,
                        "allow".into(),
                        0,
                        None,
                    )
                    .await
            })
        })
        .collect::<Vec<_>>();
    for racer in racers {
        racer
            .await
            .expect("racer joins")
            .expect("answer response enqueues");
    }

    let mut successes = 0;
    let mut loser_coordinates = Vec::new();
    for (_, sink) in &clients {
        let mut saw_resolution_event = false;
        let mut response = None;
        while !saw_resolution_event || response.is_none() {
            match sink.next().await {
                WireFrame::Event { envelope, .. } => {
                    if envelope.seq == request_seq + 1 {
                        assert!(
                            serde_json::from_value::<EventPayload>(envelope.payload).is_ok_and(
                                |payload| {
                                    matches!(
                                        payload,
                                        EventPayload::MenuAnswered(ref answer)
                                            if answer.menu == menu_id
                                    )
                                }
                            )
                        );
                        saw_resolution_event = true;
                    }
                }
                frame @ WireFrame::Response { .. } => response = Some(frame),
                frame => panic!("unexpected menu-race frame: {frame:?}"),
            }
        }
        assert!(
            saw_resolution_event,
            "outcome must arrive through every attachment's event stream"
        );
        match response.expect("answer response arrived") {
            WireFrame::Response {
                body: ResponseBody::MenuAnswer { resolution_seq },
                ..
            } => {
                successes += 1;
                assert_eq!(resolution_seq, request_seq + 1);
            }
            WireFrame::Response {
                body:
                    ResponseBody::Error {
                        ref code,
                        data: Some(ErrorData::AlreadyResolved { resolution_seq }),
                        ..
                    },
                ..
            } => {
                assert_eq!(code, "already_resolved");
                loser_coordinates.push(resolution_seq);
            }
            frame => panic!("unexpected answer response: {frame:?}"),
        }
    }
    assert_eq!(successes, 1);
    assert_eq!(loser_coordinates, vec![request_seq + 1; 7]);

    for (connection, _) in clients {
        connection.close().await.expect("connection closes");
    }
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// MUTATION CHECK: omit `register_harness`, notify only socket attachments,
/// or let the harness append `MenuAnswered` again. Expected failure: the turn
/// times out or durable history contains more than one resolution.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn committed_menu_event_wakes_registered_harness_exactly_once() {
    let (_root, store, hub) = open_hub(None, 16).await;
    let session_id = SessionId::new("live-harness-menu");
    let generation = store.worker_generation();
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitRequestInput {
            call_id: "hub-question".into(),
            kind: FakeInputKind::Choice,
            title: "Continue?".into(),
            body: Vec::new(),
            options: vec![FakeInputOption {
                key: "yes".into(),
                label: "Yes".into(),
                detail: None,
            }],
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "hub-question".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let harness_store: Arc<dyn StoreHandle> = Arc::new(hub.clone());
    let harness = HarnessActor::spawn(
        HarnessConfig::for_session(
            session_id.clone(),
            DeviceId::new("hub-harness"),
            4,
            generation,
        )
        .with_started_at_ms(1_700_000_000_000),
        provider,
        harness_store,
    );
    hub.register_harness(session_id.clone(), harness.clone())
        .await
        .expect("harness registers");
    let turn = harness
        .submit_turn(SubmitTurn::new("ask through the hub"))
        .await
        .expect("turn starts");
    let mut state = harness.state_receiver();
    let parked = state
        .wait_for(|state| matches!(state, Some(RunState::InputRequired { .. })))
        .await
        .expect("harness parks")
        .clone();
    let Some(RunState::InputRequired { menu }) = parked else {
        panic!("wait predicate guarantees InputRequired");
    };
    let history = store
        .read(&session_id, 0, 128)
        .await
        .expect("history reads");
    let request_seq = history
        .iter()
        .find_map(|envelope| {
            serde_json::from_value::<EventPayload>(envelope.payload.clone())
                .ok()
                .and_then(|payload| match payload {
                    EventPayload::MenuOpened(opened) if opened.id == menu => Some(envelope.seq),
                    _ => None,
                })
        })
        .expect("menu opening sequence");
    let head = history.last().map_or(0, |envelope| envelope.seq);

    let sink = Arc::new(CollectSink::default());
    let connection = hub
        .open_connection(capabilities(), sink.clone())
        .expect("connection");
    connection
        .request(
            RequestId::new("attach"),
            RequestBody::SessionAttach {
                session_id: session_id.clone(),
                after_seq: head,
                mode: AttachMode::Control,
            },
        )
        .await
        .expect("control attaches");
    let _ = attachment_from(sink.next().await);
    assert!(matches!(
        sink.next().await,
        WireFrame::AttachCaughtUp {
            high_water_seq, ..
        } if high_water_seq == head
    ));
    connection
        .menu_answer(
            Some(RequestId::new("answer")),
            CommandId::new("harness-command"),
            session_id.clone(),
            menu,
            request_seq,
            generation,
            "yes".into(),
            0,
            None,
        )
        .await
        .expect("answer routes");

    let outcome = tokio::time::timeout(DEADLINE, turn.wait())
        .await
        .expect("committed event wakes harness")
        .expect("turn completes");
    assert_eq!(outcome.state, RunState::Done);
    let history = store
        .read(&session_id, 0, 256)
        .await
        .expect("final history reads");
    assert_eq!(
        history
            .iter()
            .filter(|envelope| {
                serde_json::from_value::<EventPayload>(envelope.payload.clone())
                    .is_ok_and(|payload| matches!(payload, EventPayload::MenuAnswered(_)))
            })
            .count(),
        1
    );
    assert!(history.iter().any(|envelope| {
        serde_json::from_value::<EventPayload>(envelope.payload.clone())
            .is_ok_and(|payload| matches!(payload, EventPayload::ToolResult { .. }))
    }));

    connection.close().await.expect("connection closes");
    drop(harness);
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// MUTATION CHECK: make the socket response authoritative or omit the
/// command-id idempotency lookup. Expected failure: losing the success reply
/// loses the resolution, or retry appends sequence 3 instead of returning 2.
#[tokio::test]
async fn lost_menu_success_response_is_recovered_from_stream_and_idempotent_retry() {
    let (_root, store, hub) = open_hub(None, 8).await;
    let session_id = SessionId::new("lost-response-session");
    let menu_id = MenuId::new("lost-response-menu");
    let generation = store.worker_generation();
    let mut opening = vec![menu_opening(&session_id, &menu_id, generation)];
    hub.append(&mut opening).await.expect("menu opens");

    let lost = Arc::new(LostSuccessSink::default());
    let first = hub
        .open_connection(capabilities(), lost.clone())
        .expect("first connection");
    first
        .request(
            RequestId::new("attach-first"),
            RequestBody::SessionAttach {
                session_id: session_id.clone(),
                after_seq: 0,
                mode: AttachMode::Control,
            },
        )
        .await
        .expect("attach routes");
    let _ = attachment_from(lost.collected.next().await);
    let _ = lost.collected.next().await;
    let _ = lost.collected.next().await;
    let command_id = CommandId::new("lost-response-command");
    let error = first
        .menu_answer(
            Some(RequestId::new("lost-success")),
            command_id.clone(),
            session_id.clone(),
            menu_id.clone(),
            opening[0].seq,
            generation,
            "allow".into(),
            0,
            None,
        )
        .await
        .expect_err("success reply is deliberately lost");
    assert!(error.to_string().contains("outbox"));
    assert!(matches!(
        lost.collected.next().await,
        WireFrame::Event { envelope, .. } if envelope.seq == 2
    ));
    first.close().await.expect("first connection closes");

    let retry_sink = Arc::new(CollectSink::default());
    let retry = hub
        .open_connection(capabilities(), retry_sink.clone())
        .expect("retry connection");
    retry
        .request(
            RequestId::new("attach-retry"),
            RequestBody::SessionAttach {
                session_id: session_id.clone(),
                after_seq: 2,
                mode: AttachMode::Control,
            },
        )
        .await
        .expect("retry attaches");
    let _ = attachment_from(retry_sink.next().await);
    assert!(matches!(
        retry_sink.next().await,
        WireFrame::AttachCaughtUp {
            high_water_seq: 2,
            ..
        }
    ));
    retry
        .menu_answer(
            Some(RequestId::new("retry-success")),
            command_id,
            session_id,
            menu_id,
            opening[0].seq,
            generation,
            "allow".into(),
            0,
            None,
        )
        .await
        .expect("idempotent retry routes");
    assert!(matches!(
        retry_sink.next().await,
        WireFrame::Response {
            body: ResponseBody::MenuAnswer { resolution_seq: 2 },
            ..
        }
    ));
    assert!(
        retry_sink.snapshot().is_empty(),
        "idempotent retry publishes no duplicate resolution"
    );

    retry.close().await.expect("retry closes");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// MUTATION CHECK: take replay/actor handles only after an awaited detach/stop
/// send, or omit the cooperative actor stop fence. Expected failure:
/// `ShutdownGuarded` is unreachable with a full queue, or the queued append
/// commits after the shutdown future is cancelled at its deadline.
/// Verified by revert on 2026-07-27.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelled_shutdown_future_still_aborts_every_owned_hub_task() {
    let (observer, mut control) = gated_observer(vec![GateTarget::Persisted(2)]);
    let (_root, store, hub) = open_hub_with_capacities(Some(observer), 1, 8).await;
    let session_id = SessionId::new("cancelled-shutdown");
    let generation = store.worker_generation();
    append_one(&hub, &session_id, generation, "initial").await;

    let sink = Arc::new(CollectSink::default());
    let connection = hub
        .open_connection(capabilities(), sink.clone())
        .expect("connection");
    connection
        .request(
            RequestId::new("attach"),
            RequestBody::SessionAttach {
                session_id: session_id.clone(),
                after_seq: 1,
                mode: AttachMode::View,
            },
        )
        .await
        .expect("attach routes");
    let _ = attachment_from(sink.next().await);
    assert!(matches!(
        sink.next().await,
        WireFrame::AttachCaughtUp {
            high_water_seq: 1,
            ..
        }
    ));

    let committing = tokio::spawn({
        let hub = hub.clone();
        let session_id = session_id.clone();
        async move { append_one(&hub, &session_id, generation, "committing").await }
    });
    assert!(matches!(
        control.reached().await,
        HubObservation::Persisted { through_seq: 2, .. }
    ));

    control.discard_observed();
    let queued = tokio::spawn({
        let hub = hub.clone();
        let session_id = session_id.clone();
        async move {
            let mut event = vec![envelope(&session_id, "must-not-commit", generation)];
            hub.append(&mut event).await
        }
    });
    control.observed_append(&session_id).await;

    let shutdown = tokio::spawn({
        let hub = hub.clone();
        async move { hub.shutdown().await }
    });
    control.observed_shutdown_guarded().await;
    shutdown.abort();
    assert!(
        shutdown
            .await
            .expect_err("deadline cancels shutdown")
            .is_cancelled()
    );
    control.release();

    assert_eq!(committing.await.expect("committing append joins"), 2);
    assert!(
        queued.await.expect("queued append task joins").is_err(),
        "abort-on-drop closes the actor before it can consume queued work"
    );
    assert_eq!(store.latest_seq(&session_id).await.expect("head reads"), 2);

    connection.close().await.expect("connection closes");
    store.close().await.expect("store closes");
}

/// MUTATION CHECK: stop joining replay tasks in `SessionHub::shutdown`
/// (detach them instead). Expected failure: shutdown reports complete while
/// the gated replay is still alive, so `shutdown.is_finished()` becomes true
/// before release. (Under the §6.6 grace the joined replay COMPLETES its
/// delivery rather than being cancelled; shutdown still owns its lifetime.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drain_during_replay_owns_replay_completion_before_store_close() {
    let (observer, mut control) = gated_observer(vec![GateTarget::ReplayEvent(1)]);
    let (_root, store, hub) = open_hub(Some(observer), 8).await;
    let session_id = SessionId::new("drain-replay");
    let generation = store.worker_generation();
    for seq in 1..=4 {
        append_one(&hub, &session_id, generation, &format!("event-{seq}")).await;
    }
    let sink = Arc::new(CollectSink::default());
    let connection = Arc::new(
        hub.open_connection(capabilities(), sink)
            .expect("connection"),
    );
    let attach = tokio::spawn({
        let connection = Arc::clone(&connection);
        async move {
            connection
                .request(
                    RequestId::new("attach"),
                    RequestBody::SessionAttach {
                        session_id,
                        after_seq: 0,
                        mode: AttachMode::View,
                    },
                )
                .await
        }
    });
    assert!(matches!(
        control.reached().await,
        HubObservation::ReplayEvent { seq: 1, .. }
    ));
    attach
        .await
        .expect("attach joins")
        .expect("attach response was admitted");

    let shutdown = tokio::spawn({
        let hub = hub.clone();
        async move { hub.shutdown().await }
    });
    tokio::task::yield_now().await;
    assert!(
        !shutdown.is_finished(),
        "drain must own replay completion, not detach the task"
    );
    control.release();
    shutdown
        .await
        .expect("shutdown joins")
        .expect("hub shuts down");
    connection.close().await.expect("connection closes");
    store
        .close()
        .await
        .expect("no replay clone holds the store past drain");
}

/// MUTATION CHECK: replace durable prompt replay with an in-memory pending
/// menu cache. Expected failure: an attachment created after `MenuOpened`
/// misses the prompt (especially after rebuilding the hub from the store).
#[tokio::test]
async fn attachment_after_menu_opened_learns_pending_menu_from_replay() {
    let (root, store, hub) = open_hub(None, 8).await;
    let session_id = SessionId::new("pending-menu-session");
    let menu_id = MenuId::new("pending-menu");
    let mut opening = vec![menu_opening(
        &session_id,
        &menu_id,
        store.worker_generation(),
    )];
    hub.append(&mut opening).await.expect("menu opens");
    hub.shutdown().await.expect("first hub stops");
    store.close().await.expect("first store closes");

    let store = SqliteStoreHandle::open(root.path())
        .await
        .expect("store reopens in a new worker generation");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default())
        .expect("hub rebuilds from durable state");

    let sink = Arc::new(CollectSink::default());
    let connection = hub
        .open_connection(capabilities(), sink.clone())
        .expect("connection");
    connection
        .request(
            RequestId::new("attach"),
            RequestBody::SessionAttach {
                session_id,
                after_seq: 0,
                mode: AttachMode::Control,
            },
        )
        .await
        .expect("attach routes");
    let _ = attachment_from(sink.next().await);
    assert!(matches!(
        sink.next().await,
        WireFrame::Event { envelope, .. }
            if serde_json::from_value::<EventPayload>(envelope.payload.clone()).is_ok_and(
                |payload| matches!(payload, EventPayload::MenuOpened(menu) if menu.id == menu_id)
            )
    ));
    assert!(matches!(
        sink.next().await,
        WireFrame::AttachCaughtUp {
            high_water_seq: 1,
            ..
        }
    ));

    connection.close().await.expect("connection closes");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// MUTATION CHECK: compare MenuAnswer generation only with the opening event
/// and omit the active-worker fence. Expected failure: the stale answer
/// commits and the client sees a success/event instead of `stale_generation`.
#[tokio::test]
async fn stale_worker_generation_menu_answer_is_rejected_and_fenced() {
    let (_root, store, hub) = open_hub(None, 8).await;
    let session_id = SessionId::new("stale-menu-session");
    let menu_id = MenuId::new("stale-menu");
    let generation = store.worker_generation();
    let stale_generation = generation.saturating_sub(1);
    let mut opening = vec![menu_opening(&session_id, &menu_id, stale_generation)];
    hub.append(&mut opening).await.expect("menu opens");
    let sink = Arc::new(CollectSink::default());
    let connection = hub
        .open_connection(capabilities(), sink.clone())
        .expect("connection");
    connection
        .request(
            RequestId::new("attach"),
            RequestBody::SessionAttach {
                session_id: session_id.clone(),
                after_seq: 0,
                mode: AttachMode::Control,
            },
        )
        .await
        .expect("attach routes");
    let _ = attachment_from(sink.next().await);
    let _ = sink.next().await;
    let _ = sink.next().await;
    connection
        .menu_answer(
            Some(RequestId::new("stale")),
            CommandId::new("stale-command"),
            session_id,
            menu_id,
            opening[0].seq,
            stale_generation,
            "allow".into(),
            0,
            None,
        )
        .await
        .expect("rejection enqueues");
    assert!(matches!(
        sink.next().await,
        WireFrame::Response {
            request_id,
            body: ResponseBody::Error { ref code, .. },
        } if request_id.as_str() == "stale" && code == "stale_generation"
    ));
    assert!(
        sink.snapshot().is_empty(),
        "stale answer published no event"
    );

    connection.close().await.expect("connection closes");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// Attaches from sequence zero and drains this attachment's replay through
/// its `AttachCaughtUp`, returning the attachment id.
async fn attach_caught_up(
    connection: &HubConnection,
    sink: &Arc<CollectSink>,
    session_id: &SessionId,
    request_id: &str,
) -> AttachmentId {
    connection
        .request(
            RequestId::new(request_id),
            RequestBody::SessionAttach {
                session_id: session_id.clone(),
                after_seq: 0,
                mode: AttachMode::View,
            },
        )
        .await
        .expect("attach routes");
    let (attachment_id, _) = attachment_from(sink.next().await);
    loop {
        match sink.next().await {
            WireFrame::Event { .. } => {}
            WireFrame::AttachCaughtUp {
                attachment_id: found,
                ..
            } if found == attachment_id => break,
            frame => panic!("unexpected attach frame: {frame:?}"),
        }
    }
    attachment_id
}

/// MUTATION CHECK: skip the slot reservation before actor/channel allocation
/// or the release inside `take_attachment`. Expected failure: the third
/// attach is admitted, or the post-detach retry stays rejected.
/// Verified by revert on 2026-07-27.
#[tokio::test]
async fn per_connection_attachment_cap_rejects_overloaded_and_readmits_after_detach() {
    let config = SessionHubConfig {
        max_attachments_per_connection: 2,
        max_attachments: 8,
        ..SessionHubConfig::default()
    };
    let (_root, store, hub) = open_hub_with_config(None, config).await;
    let session_id = SessionId::new("admission-per-connection");
    append_one(&hub, &session_id, store.worker_generation(), "seed").await;
    let sink = Arc::new(CollectSink::default());
    let connection = hub
        .open_connection(capabilities(), sink.clone())
        .expect("connection");

    let first = attach_caught_up(&connection, &sink, &session_id, "attach-1").await;
    let _second = attach_caught_up(&connection, &sink, &session_id, "attach-2").await;
    connection
        .request(
            RequestId::new("attach-3"),
            RequestBody::SessionAttach {
                session_id: session_id.clone(),
                after_seq: 0,
                mode: AttachMode::View,
            },
        )
        .await
        .expect("typed rejection enqueues");
    assert!(matches!(
        sink.next().await,
        WireFrame::Response {
            request_id,
            body: ResponseBody::Error {
                ref code,
                retryable: true,
                ..
            },
        } if request_id.as_str() == "attach-3" && code == "overloaded"
    ));

    connection
        .request(
            RequestId::new("detach-1"),
            RequestBody::SessionDetach {
                attachment_id: first,
            },
        )
        .await
        .expect("detach routes");
    assert!(matches!(
        sink.next().await,
        WireFrame::Response {
            body: ResponseBody::SessionDetach { .. },
            ..
        }
    ));
    let _readmitted = attach_caught_up(&connection, &sink, &session_id, "attach-4").await;

    connection.close().await.expect("connection closes");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// MUTATION CHECK: enforce only the per-connection ledger. Expected failure:
/// the second connection's attach is admitted while the hub sits at its
/// global attachment cap.
#[tokio::test]
async fn global_attachment_cap_binds_independently_of_per_connection_headroom() {
    let config = SessionHubConfig {
        max_attachments_per_connection: 4,
        max_attachments: 2,
        ..SessionHubConfig::default()
    };
    let (_root, store, hub) = open_hub_with_config(None, config).await;
    let session_id = SessionId::new("admission-global");
    append_one(&hub, &session_id, store.worker_generation(), "seed").await;
    let first_sink = Arc::new(CollectSink::default());
    let first = hub
        .open_connection(capabilities(), first_sink.clone())
        .expect("first connection");
    let second_sink = Arc::new(CollectSink::default());
    let second = hub
        .open_connection(capabilities(), second_sink.clone())
        .expect("second connection");

    let held = attach_caught_up(&first, &first_sink, &session_id, "first-1").await;
    let _also_held = attach_caught_up(&first, &first_sink, &session_id, "first-2").await;
    second
        .request(
            RequestId::new("second-1"),
            RequestBody::SessionAttach {
                session_id: session_id.clone(),
                after_seq: 0,
                mode: AttachMode::View,
            },
        )
        .await
        .expect("typed rejection enqueues");
    assert!(matches!(
        second_sink.next().await,
        WireFrame::Response {
            request_id,
            body: ResponseBody::Error {
                ref code,
                retryable: true,
                ..
            },
        } if request_id.as_str() == "second-1" && code == "overloaded"
    ));

    first
        .request(
            RequestId::new("first-detach"),
            RequestBody::SessionDetach {
                attachment_id: held,
            },
        )
        .await
        .expect("detach routes");
    assert!(matches!(
        first_sink.next().await,
        WireFrame::Response {
            body: ResponseBody::SessionDetach { .. },
            ..
        }
    ));
    let _readmitted = attach_caught_up(&second, &second_sink, &session_id, "second-2").await;

    first.close().await.expect("first closes");
    second.close().await.expect("second closes");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// MUTATION CHECK: remove the byte charge in `publish` (keep only the
/// 64-slot frame count). Expected failure: both large envelopes are buffered
/// without lag, no store-resume happens, and the raised-head AttachCaughtUp
/// asserted below never arrives.
/// Verified by revert on 2026-07-27.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn catch_up_byte_budget_trips_long_before_the_frame_count_and_resumes_from_store() {
    let (observer, mut control) = gated_observer(vec![GateTarget::ReplayEvent(1)]);
    let config = SessionHubConfig {
        catch_up_byte_budget: 4_096,
        ..SessionHubConfig::default()
    };
    let (_root, store, hub) = open_hub_with_config(Some(observer), config).await;
    let session_id = SessionId::new("catch-up-bytes");
    let generation = store.worker_generation();
    append_one(&hub, &session_id, generation, "initial-1").await;
    append_one(&hub, &session_id, generation, "initial-2").await;

    let sink = Arc::new(CollectSink::default());
    let connection = hub
        .open_connection(capabilities(), sink.clone())
        .expect("connection");
    connection
        .request(
            RequestId::new("attach"),
            RequestBody::SessionAttach {
                session_id: session_id.clone(),
                after_seq: 0,
                mode: AttachMode::View,
            },
        )
        .await
        .expect("attach routes");
    assert!(matches!(
        control.reached().await,
        HubObservation::ReplayEvent { seq: 1, .. }
    ));

    for name in ["giant-1", "giant-2"] {
        let mut event = vec![envelope(&session_id, name, generation)];
        event[0].payload = serde_json::json!({
            "type": "future_test_event",
            "blob": "x".repeat(3_000),
        });
        hub.append(&mut event).await.expect("giant append commits");
    }
    control.release();

    let mut delivered = Vec::new();
    loop {
        match sink.next().await {
            WireFrame::Event { envelope, .. } => delivered.push(envelope.seq),
            WireFrame::AttachCaughtUp {
                high_water_seq: 4, ..
            } => break,
            WireFrame::Response { .. } | WireFrame::AttachCaughtUp { .. } => {}
            frame => panic!("unexpected byte-budget frame: {frame:?}"),
        }
    }
    assert_eq!(delivered, [1, 2, 3, 4]);

    connection.close().await.expect("connection closes");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// A sink that admits the initial replay and caught-up marker, then answers
/// `Busy` to every further event with a drain signal that never advances —
/// the genuinely stuck client shape.
struct StalledAfterCaughtUpSink {
    collected: CollectSink,
    saw_caught_up: AtomicBool,
    /// Admission tickets that deliberately never fire — the stuck shape.
    parked: Mutex<Vec<AdmissionTicket>>,
}

impl FrameSink for StalledAfterCaughtUpSink {
    fn try_send(&self, frame: WireFrame) -> Result<(), FrameSendError> {
        if matches!(frame, WireFrame::AttachCaughtUp { .. }) {
            self.saw_caught_up.store(true, Ordering::Release);
        }
        self.collected.try_send(frame)
    }

    fn purge_attachment(&self, attachment_id: &AttachmentId) -> Option<RequestId> {
        self.collected.purge_attachment(attachment_id)
    }

    fn offer(&self, _attachment_id: &AttachmentId, frame: &WireFrame) -> SendAdmission {
        if self.saw_caught_up.load(Ordering::Acquire) && matches!(frame, WireFrame::Event { .. }) {
            return SendAdmission::Busy;
        }
        match self.try_send(frame.clone()) {
            Ok(()) => SendAdmission::Sent,
            Err(FrameSendError) => SendAdmission::Refused,
        }
    }

    fn drain_ticket(&self) -> Option<AdmissionTicket> {
        let ticket = Arc::new(Notify::new());
        self.parked
            .lock()
            .expect("parked lock")
            .push(Arc::clone(&ticket));
        Some(ticket)
    }
}

/// MUTATION CHECK: remove the lagged-while-busy exit from `deliver_frame`
/// (wait only for drain progress and cancellation). Expected failure: the
/// stalled attachment never laggs or detaches, no `Lagged` frame ever
/// reaches the sink, and the deadline expires.
/// Verified by revert on 2026-07-27.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn commit_pressure_behind_a_stalled_outbox_laggs_and_detaches() {
    let config = SessionHubConfig {
        catch_up_byte_budget: 1_800,
        ..SessionHubConfig::default()
    };
    let (_root, store, hub) = open_hub_with_config(None, config).await;
    let session_id = SessionId::new("stalled-outbox");
    let generation = store.worker_generation();
    append_one(&hub, &session_id, generation, "initial").await;
    let sink = Arc::new(StalledAfterCaughtUpSink {
        collected: CollectSink::default(),
        saw_caught_up: AtomicBool::new(false),
        parked: Mutex::new(Vec::new()),
    });
    let connection = hub
        .open_connection(capabilities(), sink.clone())
        .expect("connection");
    connection
        .request(
            RequestId::new("attach"),
            RequestBody::SessionAttach {
                session_id: session_id.clone(),
                after_seq: 0,
                mode: AttachMode::View,
            },
        )
        .await
        .expect("attach routes");
    let (attachment_id, _) = attachment_from(sink.collected.next().await);
    assert!(matches!(
        sink.collected.next().await,
        WireFrame::Event { envelope, .. } if envelope.seq == 1
    ));
    assert!(matches!(
        sink.collected.next().await,
        WireFrame::AttachCaughtUp {
            high_water_seq: 1,
            ..
        }
    ));

    // Each append either fits the momentarily-empty catch-up channel or trips
    // the byte ledger while the replay task is blocked awaiting outbox
    // capacity that never comes; the trip converges within a few commits.
    for index in 0..8 {
        let mut event = vec![envelope(
            &session_id,
            format!("pressure-{index}"),
            generation,
        )];
        event[0].payload = serde_json::json!({
            "type": "future_test_event",
            "blob": "x".repeat(400),
        });
        hub.append(&mut event)
            .await
            .expect("pressure append commits");
    }
    assert!(matches!(
        sink.collected.next().await,
        WireFrame::Lagged {
            attachment_id: ref found,
            ..
        } if found == &attachment_id
    ));

    connection
        .request(
            RequestId::new("detach-after"),
            RequestBody::SessionDetach {
                attachment_id: attachment_id.clone(),
            },
        )
        .await
        .expect("detach routes");
    assert!(matches!(
        sink.collected.next().await,
        WireFrame::Response {
            request_id,
            body: ResponseBody::Error { ref code, .. },
        } if request_id.as_str() == "detach-after" && code == "not_found"
    ));

    connection.close().await.expect("connection closes");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// MUTATION CHECK: restore the pre-W3b2.3 drain order (sweep owners and
/// abort replay tasks BEFORE joining actors). Expected failure: the append
/// that was already inside its store await commits but publishes to orphaned
/// senders, the committed envelope never reaches the attached sink, and this
/// test times out waiting for it.
/// Verified by revert on 2026-07-27.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn graceful_drain_broadcasts_an_in_flight_commit_before_teardown() {
    let (observer, mut control) = gated_observer(vec![GateTarget::Persisted(2)]);
    let (_root, store, hub) = open_hub(Some(observer), 8).await;
    let session_id = SessionId::new("drain-broadcast");
    let generation = store.worker_generation();
    append_one(&hub, &session_id, generation, "initial").await;
    let sink = Arc::new(CollectSink::default());
    let connection = hub
        .open_connection(capabilities(), sink.clone())
        .expect("connection");
    connection
        .request(
            RequestId::new("attach"),
            RequestBody::SessionAttach {
                session_id: session_id.clone(),
                after_seq: 1,
                mode: AttachMode::View,
            },
        )
        .await
        .expect("attach routes");
    let _ = attachment_from(sink.next().await);
    assert!(matches!(
        sink.next().await,
        WireFrame::AttachCaughtUp {
            high_water_seq: 1,
            ..
        }
    ));

    // The append is INSIDE its actor arm: the store call returned (Persisted
    // gate), publication has not happened yet.
    let committing = tokio::spawn({
        let hub = hub.clone();
        let session_id = session_id.clone();
        async move { append_one(&hub, &session_id, generation, "final-checkpoint").await }
    });
    assert!(matches!(
        control.reached().await,
        HubObservation::Persisted { through_seq: 2, .. }
    ));
    let shutdown = tokio::spawn({
        let hub = hub.clone();
        async move { hub.shutdown().await }
    });
    control.observed_shutdown_guarded().await;
    control.release();

    assert_eq!(committing.await.expect("append joins"), 2);
    assert_eq!(
        shutdown
            .await
            .expect("shutdown joins")
            .expect("graceful drain completes"),
        SessionHubShutdownOutcome::Graceful
    );
    // §6.6: the final committed envelope was broadcast during the grace.
    loop {
        match sink.next().await {
            WireFrame::Event { envelope, .. } if envelope.seq == 2 => break,
            WireFrame::Event { .. } | WireFrame::AttachCaughtUp { .. } => {}
            frame => panic!("unexpected drain-broadcast frame: {frame:?}"),
        }
    }

    connection.close().await.expect("connection closes");
    store.close().await.expect("store closes");
}

/// MUTATION CHECK: replace the RAII admission guard with release-on-return
/// only. Expected failure: an attach cancelled mid-registration leaks its
/// slot and the follow-up attach on the same one-slot connection is rejected
/// `overloaded`.
/// Verified by revert on 2026-07-27.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelled_registration_refunds_its_admission_slot() {
    let (observer, mut control) = gated_observer(vec![GateTarget::ReceiverRegistered]);
    let config = SessionHubConfig {
        max_attachments_per_connection: 1,
        max_attachments: 4,
        ..SessionHubConfig::default()
    };
    let (_root, store, hub) = open_hub_with_config(Some(observer), config).await;
    let session_id = SessionId::new("cancelled-registration");
    append_one(&hub, &session_id, store.worker_generation(), "seed").await;
    let sink = Arc::new(CollectSink::default());
    let connection = Arc::new(
        hub.open_connection(capabilities(), sink.clone())
            .expect("connection"),
    );

    let attach = tokio::spawn({
        let connection = Arc::clone(&connection);
        let session_id = session_id.clone();
        async move {
            connection
                .request(
                    RequestId::new("cancelled-attach"),
                    RequestBody::SessionAttach {
                        session_id,
                        after_seq: 0,
                        mode: AttachMode::View,
                    },
                )
                .await
        }
    });
    // The registration holds its reserved slot and is parked at the actor
    // round trip; abort the requester exactly there.
    assert!(matches!(
        control.reached().await,
        HubObservation::ReceiverRegistered { .. }
    ));
    attach.abort();
    assert!(
        attach
            .await
            .expect_err("attach task was cancelled")
            .is_cancelled()
    );
    control.release();

    // The RAII guard refunded the slot, so the one-slot connection admits a
    // fresh attachment.
    let _readmitted = attach_caught_up(&connection, &sink, &session_id, "attach-after").await;

    connection.close().await.expect("connection closes");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// A sink whose staged attach response is never delivered: events answer
/// `Busy` on parked tickets and the purge reports the pending request id —
/// the shape `lag_and_detach`'s unknown-id rule must handle.
#[derive(Default)]
struct UndeliveredResponseSink {
    collected: CollectSink,
    pending: Mutex<Option<RequestId>>,
    parked: Mutex<Vec<AdmissionTicket>>,
    confirmed_parked: AtomicBool,
    parked_changed: Notify,
}

impl UndeliveredResponseSink {
    async fn wait_until_parked(&self) {
        tokio::time::timeout(DEADLINE, async {
            loop {
                let changed = self.parked_changed.notified();
                if self.confirmed_parked.load(Ordering::Acquire) {
                    return;
                }
                changed.await;
            }
        })
        .await
        .expect("replay parking deadline");
    }
}

impl FrameSink for UndeliveredResponseSink {
    fn try_send(&self, frame: WireFrame) -> Result<(), FrameSendError> {
        self.collected.try_send(frame)
    }

    fn try_send_for(
        &self,
        _attachment_id: &AttachmentId,
        frame: WireFrame,
    ) -> Result<(), FrameSendError> {
        let WireFrame::Response { request_id, .. } = &frame else {
            return self.try_send(frame);
        };
        // Staged but never popped: the client never learns the id.
        *self.pending.lock().expect("pending lock") = Some(request_id.clone());
        Ok(())
    }

    fn offer(&self, _attachment_id: &AttachmentId, frame: &WireFrame) -> SendAdmission {
        if matches!(frame, WireFrame::Event { .. }) {
            return SendAdmission::Busy;
        }
        match self.try_send(frame.clone()) {
            Ok(()) => SendAdmission::Sent,
            Err(FrameSendError) => SendAdmission::Refused,
        }
    }

    fn offer_ticketed(
        &self,
        attachment_id: &AttachmentId,
        frame: &WireFrame,
        _ticket: &AdmissionTicket,
    ) -> SendAdmission {
        let admission = self.offer(attachment_id, frame);
        if matches!(admission, SendAdmission::Busy) {
            self.confirmed_parked.store(true, Ordering::Release);
            self.parked_changed.notify_waiters();
        }
        admission
    }

    fn purge_attachment(&self, attachment_id: &AttachmentId) -> Option<RequestId> {
        let _ = self.collected.purge_attachment(attachment_id);
        self.pending.lock().expect("pending lock").take()
    }

    fn drain_ticket(&self) -> Option<AdmissionTicket> {
        let ticket = Arc::new(Notify::new());
        self.parked
            .lock()
            .expect("parked lock")
            .push(Arc::clone(&ticket));
        Some(ticket)
    }
}

/// MUTATION CHECK: ignore the purge report in `lag_and_detach` and always
/// send `Lagged`. Expected failure: the client receives a `Lagged` frame for
/// an attachment id it was never told about, and the correlated error
/// asserted below never arrives (unknown-id rule).
/// Verified by revert on 2026-07-27.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn undelivered_attach_response_is_answered_with_a_correlated_error_not_lagged() {
    let config = SessionHubConfig {
        catch_up_byte_budget: 1_800,
        ..SessionHubConfig::default()
    };
    let (_root, store, hub) = open_hub_with_config(None, config).await;
    let session_id = SessionId::new("undelivered-response");
    let generation = store.worker_generation();
    append_one(&hub, &session_id, generation, "seed").await;
    let sink = Arc::new(UndeliveredResponseSink::default());
    let connection = hub
        .open_connection(capabilities(), sink.clone())
        .expect("connection");
    connection
        .request(
            RequestId::new("attach-unheard"),
            RequestBody::SessionAttach {
                session_id: session_id.clone(),
                after_seq: 0,
                mode: AttachMode::View,
            },
        )
        .await
        .expect("attach routes");
    // Deterministic barrier: the replay has answered Busy, enqueued its
    // admission ticket, and confirmed it still cannot deliver. Only now may
    // pressure trip the catch-up lag. No append can win ahead of parking.
    sink.wait_until_parked().await;

    // One oversized commit trips the hard catch-up byte bound while the
    // replay is parked on its admission ticket: lag-under-stall detaches.
    let mut event = vec![envelope(&session_id, "pressure", generation)];
    event[0].payload = serde_json::json!({
        "type": "future_test_event",
        "blob": "x".repeat(700),
    });
    hub.append(&mut event).await.expect("pressure commits");

    match sink.collected.next().await {
        WireFrame::Response {
            request_id,
            body:
                ResponseBody::Error {
                    code,
                    retryable: true,
                    ..
                },
        } => {
            assert_eq!(request_id.as_str(), "attach-unheard");
            assert_eq!(code, "overloaded");
        }
        frame => panic!("expected the correlated attach error, got {frame:?}"),
    }
    assert!(
        sink.collected
            .snapshot()
            .iter()
            .all(|frame| !matches!(frame, WireFrame::Lagged { .. } | WireFrame::Event { .. })),
        "no frame may reference an attachment id the client never learned"
    );

    connection.close().await.expect("connection closes");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// MUTATION CHECK: remove the pending-lag `final_suffix_resume` from the
/// live loop's closed-channel exit. Expected failure: the envelope that
/// overflowed the catch-up buffer just before drain is never delivered and
/// this test times out waiting for seq 3.
/// Verified by revert on 2026-07-27.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn graceful_drain_store_resumes_a_pending_lag_suffix() {
    let (observer, mut control) = gated_observer(vec![GateTarget::BeforeEvent(2)]);
    let config = SessionHubConfig {
        catch_up_byte_budget: 1_800,
        ..SessionHubConfig::default()
    };
    let (_root, store, hub) = open_hub_with_config(Some(observer), config).await;
    let session_id = SessionId::new("drain-pending-lag");
    let generation = store.worker_generation();
    append_one(&hub, &session_id, generation, "seed").await;
    let sink = Arc::new(CollectSink::default());
    let connection = hub
        .open_connection(capabilities(), sink.clone())
        .expect("connection");
    connection
        .request(
            RequestId::new("attach"),
            RequestBody::SessionAttach {
                session_id: session_id.clone(),
                after_seq: 0,
                mode: AttachMode::View,
            },
        )
        .await
        .expect("attach routes");
    let _ = attachment_from(sink.next().await);
    assert!(matches!(
        sink.next().await,
        WireFrame::Event { envelope, .. } if envelope.seq == 1
    ));
    assert!(matches!(
        sink.next().await,
        WireFrame::AttachCaughtUp {
            high_water_seq: 1,
            ..
        }
    ));

    // Small live commit: the replay task picks it up and parks at the
    // BeforeEvent(2) gate, leaving the catch-up channel EMPTY.
    append_one(&hub, &session_id, generation, "small-live").await;
    assert!(matches!(
        control.reached().await,
        HubObservation::BeforeEvent { seq: 2, .. }
    ));
    // Oversized commit while the replay is gated: trips the hard byte bound,
    // sets REAL lag, and is never buffered.
    let mut oversized = vec![envelope(&session_id, "oversized", generation)];
    oversized[0].payload = serde_json::json!({
        "type": "future_test_event",
        "blob": "x".repeat(700),
    });
    hub.append(&mut oversized).await.expect("oversized commits");

    // Drain begins with the lag pending and stops the actor.
    let shutdown = tokio::spawn({
        let hub = hub.clone();
        async move { hub.shutdown().await }
    });
    control.observed_shutdown_guarded().await;
    control.release();
    assert_eq!(
        shutdown
            .await
            .expect("shutdown joins")
            .expect("graceful drain completes"),
        SessionHubShutdownOutcome::Graceful
    );

    // §6.6: the committed suffix (seq 3) was store-resumed during the grace
    // and announced with a final caught-up at the durable head.
    let mut saw_suffix = false;
    let mut saw_final_head = false;
    while !saw_suffix || !saw_final_head {
        match sink.next().await {
            WireFrame::Event { envelope, .. } if envelope.seq == 3 => saw_suffix = true,
            WireFrame::AttachCaughtUp {
                high_water_seq: 3, ..
            } => saw_final_head = true,
            WireFrame::Event { .. } | WireFrame::AttachCaughtUp { .. } => {}
            frame => panic!("unexpected drain-suffix frame: {frame:?}"),
        }
    }

    connection.close().await.expect("connection closes");
    store.close().await.expect("store closes");
}

/// MUTATION CHECK: downgrade a `final_suffix_resume` read failure to
/// `ReplayCompletion::Complete`. Expected failure: shutdown reports
/// `Graceful` after silently dropping the committed suffix.
/// Verified by revert on 2026-07-27.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn final_suffix_store_read_failure_forces_the_shutdown_outcome() {
    let (observer, mut control) = gated_observer(vec![
        GateTarget::BeforeEvent(2),
        GateTarget::FinalSuffixHeadCaptured(3),
    ]);
    let config = SessionHubConfig {
        catch_up_byte_budget: 1_800,
        ..SessionHubConfig::default()
    };
    let (_root, store, hub) = open_hub_with_config(Some(observer), config).await;
    let session_id = SessionId::new("forced-final-suffix");
    let generation = store.worker_generation();
    append_one(&hub, &session_id, generation, "seed").await;
    let sink = Arc::new(CollectSink::default());
    let connection = hub
        .open_connection(capabilities(), sink.clone())
        .expect("connection");
    connection
        .request(
            RequestId::new("attach"),
            RequestBody::SessionAttach {
                session_id: session_id.clone(),
                after_seq: 0,
                mode: AttachMode::View,
            },
        )
        .await
        .expect("attach routes");
    let _ = attachment_from(sink.next().await);
    assert!(matches!(
        sink.next().await,
        WireFrame::Event { envelope, .. } if envelope.seq == 1
    ));
    assert!(matches!(
        sink.next().await,
        WireFrame::AttachCaughtUp {
            high_water_seq: 1,
            ..
        }
    ));

    append_one(&hub, &session_id, generation, "small-live").await;
    assert!(matches!(
        control.reached().await,
        HubObservation::BeforeEvent { seq: 2, .. }
    ));
    let mut oversized = vec![envelope(&session_id, "oversized", generation)];
    oversized[0].payload = serde_json::json!({
        "type": "future_test_event",
        "blob": "x".repeat(2_000),
    });
    hub.append(&mut oversized).await.expect("oversized commits");

    let shutdown = tokio::spawn({
        let hub = hub.clone();
        async move { hub.shutdown().await }
    });
    control.observed_shutdown_guarded().await;
    control.release();
    assert!(matches!(
        control.reached().await,
        HubObservation::FinalSuffixHeadCaptured { head: 3, .. }
    ));

    // `latest_seq` has fixed the final head. Closing the shared store here
    // makes the immediately-following final `read_page` fail deterministically.
    store
        .clone()
        .close()
        .await
        .expect("store closes at fault seam");
    control.release();
    assert_eq!(
        shutdown
            .await
            .expect("shutdown joins")
            .expect("shutdown reports an outcome"),
        SessionHubShutdownOutcome::Forced,
        "a final-suffix read failure can never be downgraded to graceful"
    );
    connection.close().await.expect("connection closes");
}

/// MUTATION CHECK: restore the empty-channel oversized-envelope admission in
/// `publish`. Expected failure: the giant envelope is buffered and delivered
/// live, so the discriminating SECOND `AttachCaughtUp` at the raised head
/// never arrives and this test times out.
/// Verified by revert on 2026-07-27.
#[tokio::test]
async fn oversized_envelope_takes_the_store_resume_path_exactly_once() {
    let config = SessionHubConfig {
        catch_up_byte_budget: 1_800,
        ..SessionHubConfig::default()
    };
    let (_root, store, hub) = open_hub_with_config(None, config).await;
    let session_id = SessionId::new("oversized-envelope");
    let generation = store.worker_generation();
    append_one(&hub, &session_id, generation, "seed").await;
    let sink = Arc::new(CollectSink::default());
    let connection = hub
        .open_connection(capabilities(), sink.clone())
        .expect("connection");
    connection
        .request(
            RequestId::new("attach"),
            RequestBody::SessionAttach {
                session_id: session_id.clone(),
                after_seq: 0,
                mode: AttachMode::View,
            },
        )
        .await
        .expect("attach routes");
    let _ = attachment_from(sink.next().await);
    assert!(matches!(
        sink.next().await,
        WireFrame::Event { envelope, .. } if envelope.seq == 1
    ));
    assert!(matches!(
        sink.next().await,
        WireFrame::AttachCaughtUp {
            high_water_seq: 1,
            ..
        }
    ));

    let mut giant = vec![envelope(&session_id, "giant", generation)];
    giant[0].payload = serde_json::json!({
        "type": "future_test_event",
        "blob": "x".repeat(3_000),
    });
    hub.append(&mut giant).await.expect("giant commits");

    // The hard byte bound never buffers it: the attachment laggs internally,
    // re-registers, and the giant arrives from the STORE — visible as a
    // repeated caught-up at the raised head.
    let mut delivered = Vec::new();
    loop {
        match sink.next().await {
            WireFrame::Event { envelope, .. } => delivered.push(envelope.seq),
            WireFrame::AttachCaughtUp {
                high_water_seq: 2, ..
            } => break,
            frame => panic!("unexpected oversized-envelope frame: {frame:?}"),
        }
    }
    assert_eq!(delivered, [2], "the giant arrived exactly once, from store");

    connection.close().await.expect("connection closes");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// A faithful bounded outbox for the combined adversarial case: atomic
/// frame+byte admission, FIFO tickets fired as the reader consumes, replies
/// always deliverable — the [`FrameSink`] contract of the real connection
/// outbox, driven by the test as the reading client. (Live commit pressure
/// is unreachable over the wire in v0.1 — there is no append RPC and the
/// menu CAS is generation-fenced across restarts — so the combined case
/// exercises the hub seam; the per-dimension e2e tests cover the real
/// outbox.)
struct BoundedReaderSink {
    state: Mutex<BoundedReaderState>,
    changed: Notify,
    frame_cap: usize,
    byte_cap: usize,
}

struct BoundedReaderState {
    queue: VecDeque<(WireFrame, usize)>,
    queued_bytes: usize,
    tickets: VecDeque<Weak<Notify>>,
}

impl BoundedReaderState {
    fn prune_dead_tickets(&mut self) {
        while self
            .tickets
            .front()
            .is_some_and(|ticket| ticket.strong_count() == 0)
        {
            self.tickets.pop_front();
        }
    }

    fn ticket_is_head(&mut self, ticket: &AdmissionTicket) -> bool {
        self.prune_dead_tickets();
        self.tickets
            .front()
            .is_some_and(|head| Weak::ptr_eq(head, &Arc::downgrade(ticket)))
    }

    fn fire_head_ticket(&mut self) {
        self.prune_dead_tickets();
        if let Some(ticket) = self.tickets.front().and_then(Weak::upgrade) {
            ticket.notify_one();
        }
    }

    fn remove_ticket(&mut self, ticket: &AdmissionTicket) -> bool {
        self.prune_dead_tickets();
        let was_head = self.ticket_is_head(ticket);
        let token = Arc::downgrade(ticket);
        self.tickets
            .retain(|candidate| !Weak::ptr_eq(candidate, &token));
        self.prune_dead_tickets();
        was_head
    }
}

impl BoundedReaderSink {
    fn new(frame_cap: usize, byte_cap: usize) -> Self {
        Self {
            state: Mutex::new(BoundedReaderState {
                queue: VecDeque::new(),
                queued_bytes: 0,
                tickets: VecDeque::new(),
            }),
            changed: Notify::new(),
            frame_cap,
            byte_cap,
        }
    }

    fn weight(frame: &WireFrame) -> usize {
        match frame {
            WireFrame::Event { envelope, .. } => serde_json::to_string(&envelope.payload)
                .map(|payload| payload.len())
                .unwrap_or(256)
                .saturating_add(256),
            _ => 128,
        }
    }

    fn offer_with_ticket(
        &self,
        frame: &WireFrame,
        ticket: Option<&AdmissionTicket>,
    ) -> SendAdmission {
        let weight = Self::weight(frame);
        let Ok(mut state) = self.state.lock() else {
            return SendAdmission::Refused;
        };
        state.prune_dead_tickets();
        let caller_may_admit =
            state.tickets.is_empty() || ticket.is_some_and(|ticket| state.ticket_is_head(ticket));
        if !caller_may_admit
            || state.queue.len() >= self.frame_cap
            || state.queued_bytes.saturating_add(weight) > self.byte_cap
        {
            return SendAdmission::Busy;
        }
        if let Some(ticket) = ticket
            && state.ticket_is_head(ticket)
        {
            state.tickets.pop_front();
        }
        state.queue.push_back((frame.clone(), weight));
        state.queued_bytes = state.queued_bytes.saturating_add(weight);
        if state.queue.len() < self.frame_cap && state.queued_bytes < self.byte_cap {
            state.fire_head_ticket();
        }
        drop(state);
        self.changed.notify_waiters();
        SendAdmission::Sent
    }

    /// Begins one writer turn: the frame slot is free and the FIFO head is
    /// fired, while the popped frame's bytes remain charged in flight.
    fn pop_for_write(&self) -> Option<(WireFrame, usize)> {
        let mut state = self.state.lock().expect("reader state");
        let popped = state.queue.pop_front();
        if popped.is_some() {
            state.fire_head_ticket();
        }
        popped
    }

    /// Completes the writer turn and returns the in-flight byte charge.
    fn finish_write(&self, weight: usize) {
        let mut state = self.state.lock().expect("reader state");
        state.queued_bytes = state.queued_bytes.saturating_sub(weight);
        state.fire_head_ticket();
    }

    /// Faithful writer timing: pop frees the frame slot and wakes the FIFO
    /// head while the frame's bytes remain charged in flight. Only after a
    /// scheduler turn standing in for write settlement are those bytes
    /// credited and the same reserved head woken again.
    async fn next(&self) -> WireFrame {
        tokio::time::timeout(DEADLINE, async {
            loop {
                let notified = self.changed.notified();
                let popped = self.pop_for_write();
                if let Some((frame, weight)) = popped {
                    tokio::task::yield_now().await;
                    self.finish_write(weight);
                    return frame;
                }
                notified.await;
            }
        })
        .await
        .expect("bounded reader deadline")
    }
}

impl FrameSink for BoundedReaderSink {
    fn try_send(&self, frame: WireFrame) -> Result<(), FrameSendError> {
        // Reply class: always deliverable (the reader consumes promptly).
        let weight = Self::weight(&frame);
        let mut state = self.state.lock().map_err(|_| FrameSendError)?;
        state.queue.push_back((frame, weight));
        state.queued_bytes = state.queued_bytes.saturating_add(weight);
        drop(state);
        self.changed.notify_waiters();
        Ok(())
    }

    fn offer(&self, _attachment_id: &AttachmentId, frame: &WireFrame) -> SendAdmission {
        self.offer_with_ticket(frame, None)
    }

    fn offer_ticketed(
        &self,
        _attachment_id: &AttachmentId,
        frame: &WireFrame,
        ticket: &AdmissionTicket,
    ) -> SendAdmission {
        self.offer_with_ticket(frame, Some(ticket))
    }

    fn drain_ticket(&self) -> Option<AdmissionTicket> {
        let ticket = Arc::new(Notify::new());
        match self.state.lock() {
            Ok(mut state) => state.tickets.push_back(Arc::downgrade(&ticket)),
            Err(_) => ticket.notify_one(),
        }
        Some(ticket)
    }

    fn cancel_ticket(&self, ticket: &AdmissionTicket) {
        if let Ok(mut state) = self.state.lock()
            && state.remove_ticket(ticket)
        {
            state.fire_head_ticket();
        }
    }
}

/// The sink-level version of the reviewer's exact schedule: the head token
/// fires when the writer pops, but a fresh offer runs before the head reoffer.
///
/// MUTATION CHECK: remove `BoundedReaderSink::offer_with_ticket`'s
/// `(tickets empty || presented token is head)` gate. Expected failure: the
/// fresh frame is admitted instead of parking in the fired-head window.
/// Verified by revert on 2026-07-27.
#[tokio::test]
async fn bounded_reader_sink_fired_head_blocks_a_fresh_barger() {
    let sink = BoundedReaderSink::new(1, 1_024);
    let camped_id = AttachmentId::new("camped");
    let head_id = AttachmentId::new("head");
    let fresh_id = AttachmentId::new("fresh");
    let camped = WireFrame::AttachCaughtUp {
        attachment_id: camped_id.clone(),
        high_water_seq: 1,
    };
    let head_frame = WireFrame::AttachCaughtUp {
        attachment_id: head_id.clone(),
        high_water_seq: 2,
    };
    let fresh_frame = WireFrame::AttachCaughtUp {
        attachment_id: fresh_id.clone(),
        high_water_seq: 3,
    };

    assert!(matches!(
        sink.offer(&camped_id, &camped),
        SendAdmission::Sent
    ));
    assert!(matches!(
        sink.offer(&head_id, &head_frame),
        SendAdmission::Busy
    ));
    let head = sink.drain_ticket().expect("bounded sink issues tickets");
    assert!(matches!(
        sink.offer_ticketed(&head_id, &head_frame, &head),
        SendAdmission::Busy
    ));

    let (popped, camped_weight) = sink.pop_for_write().expect("camped frame pops");
    assert_eq!(popped, camped);
    head.notified().await;
    assert!(
        matches!(sink.offer(&fresh_id, &fresh_frame), SendAdmission::Busy),
        "fresh offer must park behind the fired head"
    );
    assert!(matches!(
        sink.offer_ticketed(&head_id, &head_frame, &head),
        SendAdmission::Sent
    ));
    sink.finish_write(camped_weight);

    let (served_head, head_weight) = sink.pop_for_write().expect("head is served");
    assert_eq!(served_head, head_frame);
    assert!(matches!(
        sink.offer(&fresh_id, &fresh_frame),
        SendAdmission::Sent
    ));
    sink.finish_write(head_weight);
    assert_eq!(
        sink.pop_for_write().expect("fresh is served after head").0,
        fresh_frame
    );
}

/// MUTATION CHECK: treat a `Busy` admission as a hard refusal inside
/// `deliver_frame` (the rejected snapshot-era behavior). Expected failure:
/// under combined byte + frame + live-commit pressure across five lanes, at
/// least one reading lane is purged through `Lagged` and the zero-Lagged
/// assertion trips (or the cold lane never completes).
/// Verified by revert on 2026-07-27.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn combined_pressure_five_lanes_large_envelopes_and_live_commits_lag_no_reader() {
    let config = SessionHubConfig {
        // The whole eight-commit hot-lane burst fits in the catch-up ledger's
        // true-weight units. Any Lagged below therefore discriminates outbox
        // admission/fairness, not a legitimate internal-buffer overflow.
        catch_up_byte_budget: 96 * 1024,
        ..SessionHubConfig::default()
    };
    let (_root, store, hub) = open_hub_with_config(None, config).await;
    let generation = store.worker_generation();
    let sessions = (0..5)
        .map(|index| SessionId::new(format!("combined-{index}")))
        .collect::<Vec<_>>();
    for session_id in &sessions {
        for seq in 1..=12 {
            let mut event = vec![envelope(
                session_id,
                format!("{session_id}-big-{seq}"),
                generation,
            )];
            event[0].payload = serde_json::json!({
                "type": "future_test_event",
                "blob": "x".repeat(8 * 1024),
            });
            hub.append(&mut event).await.expect("seed commits");
        }
    }
    let cold_session = SessionId::new("combined-cold");
    append_one(&hub, &cold_session, generation, "cold-seed").await;

    // Tight bounds: ~6 large frames or ~48 KiB in flight across 6 lanes.
    let sink = Arc::new(BoundedReaderSink::new(6, 48 * 1024));
    let connection = Arc::new(
        hub.open_connection(capabilities(), sink.clone())
            .expect("connection"),
    );
    for (index, session_id) in sessions.iter().enumerate() {
        connection
            .request(
                RequestId::new(format!("attach-{index}")),
                RequestBody::SessionAttach {
                    session_id: session_id.clone(),
                    after_seq: 0,
                    mode: AttachMode::View,
                },
            )
            .await
            .expect("attach routes");
    }
    // Live commit pressure on two hot sessions while every replay runs.
    let pressure = tokio::spawn({
        let hub = hub.clone();
        let hot = sessions[0].clone();
        let also_hot = sessions[1].clone();
        async move {
            for round in 0..8 {
                for session_id in [&hot, &also_hot] {
                    let mut event = vec![envelope(
                        session_id,
                        format!("{session_id}-live-{round}"),
                        generation,
                    )];
                    event[0].payload = serde_json::json!({
                        "type": "future_test_event",
                        "blob": "y".repeat(8 * 1024),
                    });
                    hub.append(&mut event).await.expect("live commit");
                }
            }
        }
    });
    // Cold late attachment joins mid-storm.
    connection
        .request(
            RequestId::new("attach-cold"),
            RequestBody::SessionAttach {
                session_id: cold_session.clone(),
                after_seq: 0,
                mode: AttachMode::View,
            },
        )
        .await
        .expect("cold attach routes");

    // The reading client: dedup by seq per session (at-least-once, R11);
    // finish once every lane is complete and the cold lane caught up.
    let mut applied: std::collections::HashMap<SessionId, std::collections::BTreeSet<u64>> =
        std::collections::HashMap::new();
    let mut cold_caught_up = false;
    loop {
        match sink.next().await {
            WireFrame::Response { .. } => {}
            WireFrame::Event {
                session_id,
                envelope,
                ..
            } => {
                applied.entry(session_id).or_default().insert(envelope.seq);
            }
            WireFrame::AttachCaughtUp { attachment_id, .. } => {
                // Identify the cold lane by its completed single event.
                let _ = attachment_id;
                if applied
                    .get(&cold_session)
                    .is_some_and(|seqs| seqs.contains(&1))
                {
                    cold_caught_up = true;
                }
            }
            WireFrame::Lagged { .. } => {
                panic!("a continuously reading lane must never be lagged")
            }
            frame => panic!("unexpected combined-pressure frame: {frame:?}"),
        }
        let hot_done = (0..2).all(|index| {
            applied
                .get(&sessions[index])
                .is_some_and(|seqs| seqs.len() == 20)
        });
        let warm_done = (2..5).all(|index| {
            applied
                .get(&sessions[index])
                .is_some_and(|seqs| seqs.len() == 12)
        });
        if hot_done && warm_done && cold_caught_up {
            break;
        }
    }
    pressure.await.expect("pressure task joins");
    for (session_id, seqs) in &applied {
        let expected = if session_id == &cold_session {
            1
        } else if session_id == &sessions[0] || session_id == &sessions[1] {
            20
        } else {
            12
        };
        assert_eq!(
            seqs.iter().copied().collect::<Vec<_>>(),
            (1..=expected).collect::<Vec<_>>(),
            "lane {session_id} is contiguous and complete"
        );
    }

    connection.close().await.expect("connection closes");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}
