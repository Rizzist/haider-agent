#![allow(clippy::expect_used)]

//! W3b2 session-hub semantic acceptance matrix.
//!
//! These are integration tests because the hub's public worker/connection
//! seams are sufficient. No crate-internal test module is needed.

use base64::Engine as _;
use haider_core::{
    DelegationRecord, DelegationState, HarnessActor, HarnessConfig, SessionCreateCommand,
    SqliteStoreHandle, StoreHandle, SubmitCommittedTurn,
};
use haider_daemon::ConnectionTransport;
use haider_daemon::{
    AdmissionTicket, FrameSendError, FrameSink, HubConnection, HubObservation,
    IMAGE_ATTACHMENT_MIME_ALLOWLIST, SendAdmission, SessionHub, SessionHubConfig,
    SessionHubObserver, SessionHubShutdownOutcome,
};
use haider_protocol::DeliveryMode;
use haider_protocol::EventPayload;
use haider_protocol::agent::{
    AgentManifest, AgentRole, ChildReport, Grant, Placement, ReportVerification,
};
use haider_protocol::branch::{BranchCreated, BranchDescriptor};
use haider_protocol::context::{ContextFootprint, ContextFootprintTruth};
use haider_protocol::envelope::{
    EventEnvelope, PromptRender, RawEnvelope, RenderTargets, SCHEMA_VERSION,
};
use haider_protocol::error::{ErrorAction, ErrorCode, ErrorPresentation, ErrorScope};
use haider_protocol::history::{CompactionResume, NodeKind, TreeNode};
use haider_protocol::ids::{
    AgentId, ArtifactRef, BranchId, CredentialAlias, DeviceId, EffectId, EventId, ItemId, LeaseId,
    MenuId, NodeId, RunId, SessionId,
};
use haider_protocol::item::{ItemDelta, ItemEvent, ToolStatus, TurnItem};
use haider_protocol::menu::{
    DecisionKind, EffectRecoveryAction, Menu, MenuKind, MenuOption, MenuScope,
};
use haider_protocol::provider::{
    CacheStatAvailability, FinishReason, NormalizedUsage, Usage, UsageRequestKind, UsageScope,
    UsageSource,
};
use haider_protocol::session::ModelSelected;
use haider_protocol::state::{RunState, WaitReason};
use haider_protocol::tool::{AttachmentBlock, PdfDeliveryMode};
use haider_protocol::verify::VerifyVerdict;
use haider_provider::{FakeInputKind, FakeInputOption, FakeProvider, FakeStep, Message};
use haider_rpc::{
    ARTIFACT_PUT_MAX_BYTES, AttachMode, AttachmentId, Capability, CapabilitySet, CommandId,
    ERROR_CODE_ARTIFACT_TOO_LARGE, ERROR_CODE_ATTACHMENT_MIME_UNSUPPORTED,
    ERROR_CODE_ATTACHMENT_NOT_FOUND, ERROR_CODE_ATTACHMENT_TOO_LARGE,
    ERROR_CODE_ATTACHMENTS_TOO_LARGE, ERROR_CODE_BUSY, ERROR_CODE_CAPABILITY_DENIED,
    ERROR_CODE_PDF_MALFORMED, ERROR_CODE_PDF_TOO_LARGE, ERROR_CODE_PDF_TOO_MANY_PAGES,
    ERROR_CODE_TOO_MANY_ATTACHMENTS, ErrorData, FleetAgentStateWire, MenuInput,
    ObserveRunStateWire, RequestBody, RequestId, ResponseBody, SeqRange, SessionSummary,
    SurfaceInputPublishWire, WireFrame,
};
use std::collections::VecDeque;
use std::fs::OpenOptions;
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

fn footprint_envelope(
    session_id: &SessionId,
    event_id: &str,
    worker_generation: u64,
    used_tokens: u64,
) -> RawEnvelope {
    let mut envelope = envelope(session_id, event_id, worker_generation);
    let footprint = ContextFootprint {
        input_tokens: used_tokens,
        output_tokens: 0,
        cached_input_tokens: 0,
        used_tokens,
        context_window: Some(200_000),
        reserved_output_tokens: 30_000,
        soft_threshold_tokens: Some(170_000),
        estimated_turns_to_threshold: None,
        truth: ContextFootprintTruth::Estimated,
    };
    envelope.payload = serde_json::to_value(EventPayload::Item(ItemEvent::Completed {
        item_id: ItemId::new(format!("item-{event_id}")),
        item: footprint.extension_item().expect("footprint serializes"),
    }))
    .expect("footprint event serializes");
    envelope
}

fn cache_rate_usage_envelope(
    session_id: &SessionId,
    event_id: &str,
    worker_generation: u64,
    run: &str,
    logical_input: u64,
    cache_read_input: u64,
) -> RawEnvelope {
    let mut envelope = envelope(session_id, event_id, worker_generation);
    let run_id = RunId::new(run);
    envelope.run_id = Some(run_id.clone());
    envelope.payload = serde_json::to_value(EventPayload::Usage(Usage {
        input: logical_input,
        output: 0,
        reasoning: 0,
        cached: cache_read_input,
        source: UsageSource::ProviderReported,
        account: None,
        accounts: Vec::new(),
        normalized: Some(NormalizedUsage {
            logical_input,
            uncached_input: logical_input.saturating_sub(cache_read_input),
            cache_read_input,
            billed_output: 0,
            cache_status: CacheStatAvailability::Present,
            cache_telemetry_input: logical_input,
            ..NormalizedUsage::default()
        }),
        scope: Some(UsageScope {
            provider: "openai".into(),
            model: "gpt-5.2".into(),
            account_scope: None,
            auth_scope: "oauth".into(),
            api_family: None,
            effort: None,
            speed: None,
            cache_epoch: "summary-cache-rate".into(),
            stable_prefix_tokens: 0,
            cache_boundaries: None,
            request_kind: UsageRequestKind::MainTurn,
            run: Some(run_id),
            agent: None,
            prefix_digests: None,
        }),
        cache_cost: None,
        request: None,
    }))
    .expect("cache-rate usage serializes");
    envelope
}

#[derive(Default)]
struct CollectSink {
    frames: Mutex<VecDeque<WireFrame>>,
    changed: Notify,
    opening_binding_consumed: AtomicBool,
}

impl CollectSink {
    async fn take_next(&self) -> WireFrame {
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

    async fn next_raw(&self) -> WireFrame {
        let frame = self.take_next().await;
        if matches!(frame, WireFrame::ResidentSessionBinding { .. }) {
            let _ = self.opening_binding_consumed.compare_exchange(
                false,
                true,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }
        frame
    }

    async fn next(&self) -> WireFrame {
        loop {
            let frame = self.take_next().await;
            if matches!(frame, WireFrame::ResidentSessionBinding { .. })
                && self
                    .opening_binding_consumed
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
            {
                continue;
            }
            return frame;
        }
    }

    fn snapshot(&self) -> Vec<WireFrame> {
        let skip_opening_binding = !self.opening_binding_consumed.load(Ordering::Acquire);
        self.frames
            .lock()
            .expect("frames lock")
            .iter()
            .enumerate()
            .filter(|(index, frame)| {
                !(skip_opening_binding
                    && *index == 0
                    && matches!(frame, WireFrame::ResidentSessionBinding { .. }))
            })
            .map(|(_, frame)| frame)
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
                | WireFrame::SessionDescendantStream {
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
                        }
                        | ResponseBody::SessionDescendantsAttach {
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

/// Holds an attach response off-wire, then permanently refuses the first
/// descendant event. This deterministically exercises the unknown-id repair
/// path: purge must replace the staged success with a correlated error.
#[derive(Default)]
struct RefuseDescendantStartSink {
    inner: CollectSink,
    staged: Mutex<Option<(AttachmentId, RequestId)>>,
}

impl RefuseDescendantStartSink {
    async fn next(&self) -> WireFrame {
        self.inner.next().await
    }
}

impl FrameSink for RefuseDescendantStartSink {
    fn try_send(&self, frame: WireFrame) -> Result<(), FrameSendError> {
        if matches!(frame, WireFrame::SessionDescendantStream { .. }) {
            return Err(FrameSendError);
        }
        self.inner.try_send(frame)
    }

    fn try_send_for(
        &self,
        attachment_id: &AttachmentId,
        frame: WireFrame,
    ) -> Result<(), FrameSendError> {
        let WireFrame::Response { request_id, .. } = frame else {
            return Err(FrameSendError);
        };
        *self.staged.lock().map_err(|_| FrameSendError)? =
            Some((attachment_id.clone(), request_id));
        Ok(())
    }

    fn purge_attachment(&self, attachment_id: &AttachmentId) -> Option<RequestId> {
        let mut staged = self.staged.lock().ok()?;
        let matches = staged
            .as_ref()
            .is_some_and(|(queued, _)| queued == attachment_id);
        if matches {
            staged.take().map(|(_, request_id)| request_id)
        } else {
            None
        }
    }
}

struct SlowSink {
    frames: Mutex<VecDeque<WireFrame>>,
    changed: Notify,
    event_budget: usize,
    accepted_events: Mutex<usize>,
    opening_binding_consumed: AtomicBool,
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
            opening_binding_consumed: AtomicBool::new(false),
        }
    }

    async fn next(&self) -> WireFrame {
        loop {
            let frame = tokio::time::timeout(DEADLINE, async {
                loop {
                    let changed = self.changed.notified();
                    if let Some(frame) = self.frames.lock().expect("frames lock").pop_front() {
                        return frame;
                    }
                    changed.await;
                }
            })
            .await
            .expect("slow-sink frame deadline");
            if matches!(frame, WireFrame::ResidentSessionBinding { .. })
                && self
                    .opening_binding_consumed
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
            {
                continue;
            }
            return frame;
        }
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

    async fn observed_shutdown_actors_stopped(&mut self) {
        tokio::time::timeout(DEADLINE, async {
            loop {
                if matches!(
                    self.observed.recv().await,
                    Some(HubObservation::ShutdownActorsStopped)
                ) {
                    return;
                }
            }
        })
        .await
        .expect("shutdown actor-stop observation deadline");
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

fn pipe_event(
    session_id: &SessionId,
    event_id: &str,
    worker_generation: u64,
    payload: EventPayload,
) -> RawEnvelope {
    let mut event = envelope(session_id, event_id, worker_generation);
    event.payload = serde_json::to_value(payload).expect("pipe payload serializes");
    event
}

fn user_pipe_event(
    session_id: &SessionId,
    event_id: &str,
    worker_generation: u64,
    text: &str,
) -> RawEnvelope {
    pipe_event(
        session_id,
        event_id,
        worker_generation,
        EventPayload::NodeCommitted(TreeNode {
            node: NodeId::new(format!("node-{event_id}")),
            parent: None,
            kind: NodeKind::UserTurn {
                text: text.into(),
                attachments: Vec::new(),
            },
        }),
    )
}

fn compaction_pipe_event(
    session_id: &SessionId,
    event_id: &str,
    worker_generation: u64,
    run_id: &str,
) -> RawEnvelope {
    let mut event = pipe_event(
        session_id,
        event_id,
        worker_generation,
        EventPayload::NodeCommitted(TreeNode {
            node: NodeId::new(format!("node-{event_id}")),
            parent: None,
            kind: NodeKind::Compaction {
                covers_from: NodeId::new(format!("from-{event_id}")),
                covers_to: NodeId::new(format!("to-{event_id}")),
                summary_artifact: ArtifactRef::new(format!("artifact-{event_id}")),
                tokens_before: 100,
                tokens_after: 10,
                resume_cause: CompactionResume::AutoMidTurn,
            },
        }),
    );
    event.run_id = Some(RunId::new(run_id));
    event
}

fn run_state_pipe_event(
    session_id: &SessionId,
    event_id: &str,
    worker_generation: u64,
    run_id: &str,
    state: RunState,
) -> RawEnvelope {
    let mut event = pipe_event(
        session_id,
        event_id,
        worker_generation,
        EventPayload::RunState(state),
    );
    event.run_id = Some(RunId::new(run_id));
    event
}

fn successor_path(base: &std::path::Path, sidecar: &str) -> std::path::PathBuf {
    let tail: serde_json::Value = serde_json::from_str(
        sidecar
            .lines()
            .last()
            .expect("sealed sidecar has a terminator"),
    )
    .expect("terminator is JSON");
    base.parent().expect("sidecar has parent").join(
        tail["successor"]
            .as_str()
            .expect("terminator names successor"),
    )
}

fn sidecar_chain_paths(base: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut paths = Vec::new();
    let mut current = base.to_owned();
    loop {
        assert!(!paths.contains(&current), "sidecar fixture chain loops");
        paths.push(current.clone());
        let segment = std::fs::read_to_string(&current).expect("chain segment reads");
        let tail: serde_json::Value = serde_json::from_str(
            segment
                .lines()
                .last()
                .expect("chain segment has a final line"),
        )
        .expect("chain tail is JSON");
        let Some(successor) = tail
            .get("segment_end")
            .is_some()
            .then(|| tail.get("successor").and_then(serde_json::Value::as_str))
            .flatten()
        else {
            return paths;
        };
        current = base.parent().expect("sidecar has parent").join(successor);
    }
}

fn expected_sidecar_body(events: &[RawEnvelope]) -> String {
    let mut ordered: Vec<&RawEnvelope> = events.iter().collect();
    ordered.sort_by_key(|event| event.seq);
    ordered
        .into_iter()
        .filter_map(haider_protocol::pipe::sidecar_row_line)
        .map(|line| format!("{line}\n"))
        .collect()
}

fn expected_sidecar(session_id: &SessionId, generation: u64, events: &[RawEnvelope]) -> String {
    expected_sidecar_batches(session_id, generation, &[events])
}

fn expected_sidecar_batches(
    session_id: &SessionId,
    generation: u64,
    batches: &[&[RawEnvelope]],
) -> String {
    let mut expected = format!(
        "{{\"pipe\":\"haider.session.jsonl\",\"version\":6,\"session_id\":\"{session_id}\",\"generation\":{generation}}}\n"
    );
    for batch in batches {
        expected.push_str(&expected_sidecar_body(batch));
        let head = batch.iter().map(|event| event.seq).max().unwrap_or(0);
        expected.push_str(&format!(
            "{{\"coverage\":{head},\"generation\":{generation}}}\n"
        ));
    }
    expected
}

async fn stored_sidecar(
    store: &SqliteStoreHandle,
    session_id: &SessionId,
    generation: u64,
) -> String {
    let events = store
        .read(session_id, 0, usize::MAX)
        .await
        .expect("journal reads");
    expected_sidecar(session_id, generation, &events)
}

fn sidecar_path(root: &tempfile::TempDir, session_id: &SessionId) -> std::path::PathBuf {
    root.path().join("pipe").join(format!("{session_id}.pipe"))
}

/// #6 (935): sidecar maintenance runs off the publish path on a per-session
/// writer task, so a read right after `append` can race the write. Poll the
/// file until it is non-empty and STABLE across a tick — the atomic
/// temp+rename rebuild converges to exactly the steady state
/// `hub.shutdown()` guarantees, with no intermediate stable point.
async fn stable_sidecar(path: &std::path::Path) -> String {
    let mut prev: Option<String> = None;
    for _ in 0..300 {
        if let Ok(cur) = std::fs::read_to_string(path) {
            if !cur.is_empty() && prev.as_deref() == Some(cur.as_str()) {
                return cur;
            }
            prev = Some(cur);
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    prev.unwrap_or_default()
}

async fn append_delta(
    hub: &SessionHub,
    session_id: &SessionId,
    worker_generation: u64,
    event_id: &str,
) -> u64 {
    let delta = delta_pipe_event(session_id, event_id, worker_generation);
    let mut events = [delta];
    hub.append(&mut events).await.expect("delta appends");
    events[0].seq
}

fn delta_pipe_event(session_id: &SessionId, event_id: &str, worker_generation: u64) -> RawEnvelope {
    let mut delta = envelope(session_id, event_id, worker_generation);
    delta.payload = serde_json::to_value(EventPayload::Item(ItemEvent::Delta {
        item_id: ItemId::new("sealed-replay-item"),
        delta: ItemDelta::Text {
            text: event_id.to_owned(),
        },
    }))
    .expect("delta serializes");
    delta
}

fn capabilities() -> CapabilitySet {
    CapabilitySet::from([Capability::View, Capability::Control])
}

async fn upload_bytes(
    connection: &HubConnection,
    sink: &CollectSink,
    request_id: &str,
    bytes: &[u8],
) -> ResponseBody {
    connection
        .request(
            RequestId::new(request_id),
            RequestBody::ArtifactPut {
                data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
            },
        )
        .await
        .expect("artifact.put routes");
    let WireFrame::Response { body, .. } = sink.next().await else {
        panic!("expected artifact.put response");
    };
    body
}

async fn attach_control_session(
    hub: &SessionHub,
    store: &SqliteStoreHandle,
    connection: &HubConnection,
    sink: &CollectSink,
    session_id: &SessionId,
) {
    append_one(
        hub,
        session_id,
        store.worker_generation(),
        "attachment-validation-seed",
    )
    .await;
    connection
        .request(
            RequestId::new("attachment-validation-attach"),
            RequestBody::SessionAttach {
                session_id: session_id.clone(),
                after_seq: 0,
                mode: AttachMode::Control,
                sealed_replay: false,
            },
        )
        .await
        .expect("control attach routes");
    assert!(matches!(
        sink.next().await,
        WireFrame::Response {
            body: ResponseBody::SessionAttach { .. },
            ..
        }
    ));
    assert!(matches!(sink.next().await, WireFrame::Event { .. }));
    assert!(matches!(
        sink.next().await,
        WireFrame::AttachCaughtUp {
            high_water_seq: 1,
            ..
        }
    ));
}

async fn create_and_attach_typed_session(
    store: &SqliteStoreHandle,
    connection: &HubConnection,
    sink: &CollectSink,
    session_id: &SessionId,
    provider: &str,
) {
    create_typed_session(store, session_id, provider).await;
    connection
        .request(
            RequestId::new(format!("attach-{session_id}")),
            RequestBody::SessionAttach {
                session_id: session_id.clone(),
                after_seq: 0,
                mode: AttachMode::Control,
                sealed_replay: false,
            },
        )
        .await
        .expect("control attach routes");
    assert!(matches!(
        sink.next().await,
        WireFrame::Response {
            body: ResponseBody::SessionAttach { .. },
            ..
        }
    ));
    assert!(matches!(sink.next().await, WireFrame::Event { .. }));
    assert!(matches!(
        sink.next().await,
        WireFrame::AttachCaughtUp {
            high_water_seq: 1,
            ..
        }
    ));
}

async fn create_typed_session(store: &SqliteStoreHandle, session_id: &SessionId, provider: &str) {
    store
        .create_session(SessionCreateCommand {
            command_id: format!("create-{session_id}"),
            request_digest: format!("create-digest-{session_id}"),
            request_json: format!(
                r#"{{"cwd":"/tmp","max_tokens":4096,"model":"test-model","provider":"{provider}"}}"#
            ),
            session_id: session_id.clone(),
            cwd: "/tmp".into(),
            provider: provider.into(),
            model: "test-model".into(),
            max_tokens: 4096,
            permission_overrides: None,
            effort: None,
            fast: false,
            cache_policy: Default::default(),
            system_prompt_version: "test-system-v1".into(),
            event_id: EventId::new(format!("created-{session_id}")),
            device_id: DeviceId::new("hub-test"),
        })
        .await
        .expect("typed session creates");
}

/// MUTATION CHECK: ignore `in_session` in the `command.list` handler.
/// Runtime failure: `/compact` is either advertised at the launcher or hidden
/// from an attached session, forcing clients to mirror session-only names.
#[tokio::test]
async fn command_list_serves_the_requested_current_context() {
    let (_root, store, hub) = open_hub(None, 8).await;
    let sink = Arc::new(CollectSink::default());
    let connection = hub
        .open_connection(
            capabilities(),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("connection");
    assert!(matches!(
        sink.next_raw().await,
        WireFrame::ResidentSessionBinding { .. }
    ));

    for (request, in_session) in [("launcher", false), ("session", true)] {
        connection
            .request(
                RequestId::new(request),
                RequestBody::CommandList {
                    query: String::new(),
                    in_session,
                    slots: haider_rpc::CommandDynamicSlotsWire::default(),
                },
            )
            .await
            .expect("command.list routes");
        let WireFrame::Response {
            body: ResponseBody::CommandList { items },
            ..
        } = sink.next().await
        else {
            panic!("expected command.list response");
        };
        assert_eq!(
            items
                .iter()
                .any(|item| item.name.as_deref() == Some("compact")),
            in_session
        );
        let model = items
            .iter()
            .find(|item| item.name.as_deref() == Some("model"))
            .expect("model is visible in both contexts");
        assert_eq!(
            model.ownership,
            if in_session {
                haider_rpc::CommandOwnershipWire::DaemonOperation
            } else {
                haider_rpc::CommandOwnershipWire::ClientView
            }
        );
    }

    connection.close().await.expect("connection closes");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// MUTATION CHECK: change `/help` from `ClientView` to `DaemonOperation` in
/// the shared registry. Runtime failure: this request stops returning the
/// client-owned outcome and may append daemon state for a help pane the
/// daemon cannot observe.
#[tokio::test]
async fn command_invoke_never_executes_client_owned_view_state() {
    let (_root, store, hub) = open_hub(None, 8).await;
    let sink = Arc::new(CollectSink::default());
    let connection = hub
        .open_connection(
            capabilities(),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("connection");
    assert!(matches!(
        sink.next_raw().await,
        WireFrame::ResidentSessionBinding { .. }
    ));

    connection
        .request(
            RequestId::new("command-help"),
            RequestBody::CommandInvoke {
                command_id: CommandId::new("command-help-id"),
                command: "/help".into(),
                session_id: None,
            },
        )
        .await
        .expect("command.invoke routes");
    assert!(matches!(
        sink.next().await,
        WireFrame::Response {
            request_id,
            body: ResponseBody::CommandInvoke {
                outcome: haider_rpc::CommandInvokeOutcomeWire::ClientOwned { ref command },
            },
        } if request_id.as_str() == "command-help" && command == "help"
    ));
    assert!(
        store.session_ids().await.expect("session ids").is_empty(),
        "client-owned help invocation must not create daemon truth"
    );

    connection
        .request(
            RequestId::new("command-launcher-model"),
            RequestBody::CommandInvoke {
                command_id: CommandId::new("command-launcher-model-id"),
                command: "/model future-model".into(),
                session_id: None,
            },
        )
        .await
        .expect("launcher model routes");
    assert!(matches!(
        sink.next().await,
        WireFrame::Response {
            request_id,
            body: ResponseBody::CommandInvoke {
                outcome: haider_rpc::CommandInvokeOutcomeWire::ClientOwned { ref command },
            },
        } if request_id.as_str() == "command-launcher-model" && command == "model"
    ));

    connection.close().await.expect("connection closes");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// MUTATION CHECK: replace the unknown-command fallback with `Ok(())` or a
/// concrete operation. Runtime failure: the caller receives no correlated
/// refusal, or an unknown slash name mutates daemon state.
#[tokio::test]
async fn command_invoke_unknown_name_degrades_honestly() {
    let (_root, store, hub) = open_hub(None, 8).await;
    let sink = Arc::new(CollectSink::default());
    let connection = hub
        .open_connection(
            capabilities(),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("connection");
    assert!(matches!(
        sink.next_raw().await,
        WireFrame::ResidentSessionBinding { .. }
    ));

    connection
        .request(
            RequestId::new("command-future"),
            RequestBody::CommandInvoke {
                command_id: CommandId::new("command-future-id"),
                command: "/future-command".into(),
                session_id: None,
            },
        )
        .await
        .expect("command.invoke routes");
    assert!(matches!(
        sink.next().await,
        WireFrame::Response {
            request_id,
            body: ResponseBody::CommandInvoke {
                outcome: haider_rpc::CommandInvokeOutcomeWire::Unsupported {
                    ref command,
                    ref reason,
                },
            },
        } if request_id.as_str() == "command-future"
            && command == "future-command"
            && reason.as_deref() == Some("unknown command")
    ));
    assert!(store.session_ids().await.expect("session ids").is_empty());

    connection.close().await.expect("connection closes");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// MUTATION CHECK: change either unknown command-name arm or the reserved
/// `command-door-` version fallback from `Invalid` to `Ordinary`. Runtime
/// failure: `menu.answer` commits but executes nothing, silently consuming an
/// operator choice.
#[tokio::test]
async fn unknown_parked_command_origin_is_rejected_before_answer_commit() {
    let (_root, store, hub) = open_hub(None, 16).await;
    let session_id = SessionId::new("command-door-unknown-origin");
    let menu_id = MenuId::new("command-door-future-menu");
    let sink = Arc::new(CollectSink::default());
    let connection = hub
        .open_connection(
            capabilities(),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("connection");
    assert!(matches!(
        sink.next_raw().await,
        WireFrame::ResidentSessionBinding { .. }
    ));
    create_and_attach_typed_session(&store, &connection, &sink, &session_id, "fake").await;

    let mut opening = menu_opening(&session_id, &menu_id, store.worker_generation());
    let EventPayload::MenuOpened(mut menu) =
        serde_json::from_value::<EventPayload>(opening.payload).expect("menu payload")
    else {
        panic!("opening helper must create a menu");
    };
    menu.origin = "command-door-v1:future-command-kind".into();
    opening.payload = serde_json::to_value(EventPayload::MenuOpened(menu)).expect("menu encodes");
    let mut opening = [opening];
    hub.append(&mut opening).await.expect("future menu opens");
    let request_seq = opening[0].seq;
    let generation = opening[0].worker_generation;
    assert!(matches!(sink.next().await, WireFrame::Event { .. }));

    connection
        .menu_answer(
            Some(RequestId::new("future-menu-answer")),
            CommandId::new("future-menu-answer-id"),
            session_id.clone(),
            menu_id,
            request_seq,
            generation,
            "allow".into(),
            0,
            None,
        )
        .await
        .expect("unknown origin refusal routes");
    assert!(matches!(
        sink.next().await,
        WireFrame::Response {
            request_id,
            body: ResponseBody::Error { ref code, ref message, .. },
        } if request_id.as_str() == "future-menu-answer"
            && code == "invalid_argument"
            && message.contains("no action was taken")
    ));
    assert_eq!(
        store.latest_seq(&session_id).await.expect("head"),
        request_seq,
        "unknown command origin must leave its menu unanswered"
    );

    let v2_menu_id = MenuId::new("command-door-v2-menu");
    let mut v2_opening = menu_opening(&session_id, &v2_menu_id, store.worker_generation());
    v2_opening.event_id = EventId::new("command-door-v2-opened");
    let EventPayload::MenuOpened(mut v2_menu) =
        serde_json::from_value::<EventPayload>(v2_opening.payload).expect("menu payload")
    else {
        panic!("opening helper must create a menu");
    };
    v2_menu.origin = "command-door-v2:rename".into();
    v2_opening.payload =
        serde_json::to_value(EventPayload::MenuOpened(v2_menu)).expect("menu encodes");
    let mut v2_opening = [v2_opening];
    hub.append(&mut v2_opening).await.expect("v2 menu opens");
    assert!(matches!(sink.next().await, WireFrame::Event { .. }));

    connection
        .menu_answer(
            Some(RequestId::new("v2-menu-answer")),
            CommandId::new("v2-menu-answer-id"),
            session_id.clone(),
            v2_menu_id,
            v2_opening[0].seq,
            v2_opening[0].worker_generation,
            "allow".into(),
            0,
            None,
        )
        .await
        .expect("future version refusal routes");
    assert!(matches!(
        sink.next().await,
        WireFrame::Response {
            request_id,
            body: ResponseBody::Error { ref code, ref message, .. },
        } if request_id.as_str() == "v2-menu-answer"
            && code == "invalid_argument"
            && message.contains("origin version")
    ));
    assert_eq!(
        store.latest_seq(&session_id).await.expect("v2 head"),
        v2_opening[0].seq,
        "future command-door version must remain unanswered"
    );

    connection.close().await.expect("connection closes");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// MUTATION CHECK: set `worker_generation` to `None` in the command-card
/// builder. Runtime failure: the returned park cannot construct the existing
/// three-coordinate `menu.answer` fence and this test's unwrap fails.
#[tokio::test]
async fn parked_command_uses_needs_input_and_executes_once_through_menu_answer() {
    let (_root, store, hub) = open_hub(None, 16).await;
    let session_id = SessionId::new("command-door-rename");
    let sink = Arc::new(CollectSink::default());
    let connection = hub
        .open_connection(
            capabilities(),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("connection");
    assert!(matches!(
        sink.next_raw().await,
        WireFrame::ResidentSessionBinding { .. }
    ));
    create_and_attach_typed_session(&store, &connection, &sink, &session_id, "fake").await;

    connection
        .request(
            RequestId::new("command-rename-park"),
            RequestBody::CommandInvoke {
                command_id: CommandId::new("command-rename-park-id"),
                command: "/rename".into(),
                session_id: Some(session_id.clone()),
            },
        )
        .await
        .expect("command.invoke parks");
    let mut opened_seq = None;
    let mut parked = None;
    while opened_seq.is_none() || parked.is_none() {
        match sink.next().await {
            WireFrame::Event { envelope, .. }
                if matches!(
                    serde_json::from_value::<EventPayload>(envelope.payload.clone()),
                    Ok(EventPayload::MenuOpened(_))
                ) =>
            {
                opened_seq = Some(envelope.seq);
            }
            WireFrame::Response {
                request_id,
                body:
                    ResponseBody::CommandInvoke {
                        outcome: haider_rpc::CommandInvokeOutcomeWire::Parked { needs_input },
                    },
            } if request_id.as_str() == "command-rename-park" => parked = Some(needs_input),
            frame => panic!("unexpected command park frame: {frame:?}"),
        }
    }
    let parked = parked.expect("parked card");
    let menu_id = parked.menu_id.expect("menu id");
    let request_seq = parked.request_seq.expect("request seq");
    let worker_generation = parked.worker_generation.expect("worker generation");
    assert_eq!(Some(request_seq), opened_seq);
    assert_eq!(parked.kind, haider_rpc::NeedsInputKindWire::Question);

    connection
        .menu_answer(
            Some(RequestId::new("command-rename-answer")),
            CommandId::new("command-rename-answer-id"),
            session_id.clone(),
            menu_id.clone(),
            request_seq,
            worker_generation,
            String::new(),
            0,
            Some(MenuInput::Text {
                text: "Door renamed".into(),
            }),
        )
        .await
        .expect("menu.answer executes command");
    loop {
        if matches!(
            sink.next().await,
            WireFrame::Response {
                request_id,
                body: ResponseBody::MenuAnswer { .. },
            } if request_id.as_str() == "command-rename-answer"
        ) {
            break;
        }
    }
    assert_eq!(
        store
            .session_metadata(&session_id)
            .await
            .expect("metadata read")
            .expect("typed metadata")
            .title
            .as_deref(),
        Some("Door renamed")
    );

    connection
        .request(
            RequestId::new("command-rename-invoke-replay"),
            RequestBody::CommandInvoke {
                command_id: CommandId::new("command-rename-park-id"),
                command: "/rename".into(),
                session_id: Some(session_id.clone()),
            },
        )
        .await
        .expect("resolved invoke replay routes");
    assert!(matches!(
        sink.next().await,
        WireFrame::Response {
            request_id,
            body: ResponseBody::CommandInvoke {
                outcome: haider_rpc::CommandInvokeOutcomeWire::Receipt { receipt },
            },
        } if request_id.as_str() == "command-rename-invoke-replay"
            && matches!(*receipt, ResponseBody::SessionRename { .. })
    ));

    connection
        .menu_answer(
            Some(RequestId::new("command-rename-loser")),
            CommandId::new("command-rename-loser-id"),
            session_id,
            menu_id,
            request_seq,
            worker_generation,
            String::new(),
            0,
            Some(MenuInput::Text {
                text: "Different name".into(),
            }),
        )
        .await
        .expect("second answer routes");
    loop {
        match sink.next().await {
            WireFrame::Response {
                request_id,
                body: ResponseBody::Error { code, .. },
            } if request_id.as_str() == "command-rename-loser" => {
                assert_eq!(code, "already_resolved");
                break;
            }
            WireFrame::Event { .. } => {}
            frame => panic!("unexpected second-answer frame: {frame:?}"),
        }
    }

    connection.close().await.expect("connection closes");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// MUTATION CHECK: replace the deterministic command-menu identity with
/// `random_id` or omit the preflight journal lookup. Runtime failure: the
/// retry appends a second `MenuOpened`, advances the head, and returns a
/// different full answerable coordinate set.
#[tokio::test]
async fn parked_command_invoke_retry_replays_one_durable_menu() {
    let (_root, store, hub) = open_hub(None, 16).await;
    let session_id = SessionId::new("command-door-park-retry");
    let sink = Arc::new(CollectSink::default());
    let connection = hub
        .open_connection(
            capabilities(),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("connection");
    assert!(matches!(
        sink.next_raw().await,
        WireFrame::ResidentSessionBinding { .. }
    ));
    create_and_attach_typed_session(&store, &connection, &sink, &session_id, "fake").await;

    let command_id = CommandId::new("command-park-retry-id");
    connection
        .request(
            RequestId::new("command-park-first"),
            RequestBody::CommandInvoke {
                command_id: command_id.clone(),
                command: "/rename".into(),
                session_id: Some(session_id.clone()),
            },
        )
        .await
        .expect("first invoke parks");
    let first = loop {
        match sink.next().await {
            WireFrame::Response {
                request_id,
                body:
                    ResponseBody::CommandInvoke {
                        outcome: haider_rpc::CommandInvokeOutcomeWire::Parked { needs_input },
                    },
            } if request_id.as_str() == "command-park-first" => break needs_input,
            WireFrame::Event { .. } => {}
            frame => panic!("unexpected first park frame: {frame:?}"),
        }
    };
    let first_head = store.latest_seq(&session_id).await.expect("first head");

    connection
        .request(
            RequestId::new("command-park-retry"),
            RequestBody::CommandInvoke {
                command_id,
                command: "/rename".into(),
                session_id: Some(session_id.clone()),
            },
        )
        .await
        .expect("retry routes");
    let WireFrame::Response {
        request_id,
        body:
            ResponseBody::CommandInvoke {
                outcome:
                    haider_rpc::CommandInvokeOutcomeWire::Parked {
                        needs_input: replay,
                    },
            },
    } = sink.next().await
    else {
        panic!("retry must replay the parked response")
    };
    assert_eq!(request_id.as_str(), "command-park-retry");
    assert_eq!(replay.menu_id, first.menu_id);
    assert_eq!(replay.request_seq, first.request_seq);
    assert_eq!(replay.worker_generation, first.worker_generation);
    assert_eq!(
        store.latest_seq(&session_id).await.expect("retry head"),
        first_head,
        "retry must not append a second answerable card"
    );

    connection.close().await.expect("connection closes");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// MUTATION CHECK: force command menus into `allow_prior_generation: false`
/// in `menu_answer`. Runtime failure: answering the replayed card after the
/// store reopens returns `stale_generation` and the rename never commits.
#[tokio::test]
async fn parked_command_remains_answerable_after_worker_generation_changes() {
    let (root, store, hub) = open_hub(None, 16).await;
    let session_id = SessionId::new("command-door-restart");
    let sink = Arc::new(CollectSink::default());
    let connection = hub
        .open_connection(
            capabilities(),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("connection");
    assert!(matches!(
        sink.next_raw().await,
        WireFrame::ResidentSessionBinding { .. }
    ));
    create_and_attach_typed_session(&store, &connection, &sink, &session_id, "fake").await;
    connection
        .request(
            RequestId::new("command-restart-park"),
            RequestBody::CommandInvoke {
                command_id: CommandId::new("command-restart-park-id"),
                command: "/rename".into(),
                session_id: Some(session_id.clone()),
            },
        )
        .await
        .expect("command parks");
    let parked = loop {
        match sink.next().await {
            WireFrame::Response {
                request_id,
                body:
                    ResponseBody::CommandInvoke {
                        outcome: haider_rpc::CommandInvokeOutcomeWire::Parked { needs_input },
                    },
            } if request_id.as_str() == "command-restart-park" => break needs_input,
            WireFrame::Event { .. } => {}
            frame => panic!("unexpected park frame: {frame:?}"),
        }
    };
    let menu_id = parked.menu_id.expect("menu id");
    let request_seq = parked.request_seq.expect("request seq");
    let opening_generation = parked.worker_generation.expect("opening generation");
    connection.close().await.expect("first connection closes");
    hub.shutdown().await.expect("first hub stops");
    store.close().await.expect("first store closes");

    let store = SqliteStoreHandle::open(root.path())
        .await
        .expect("store reopens in a new worker generation");
    assert_ne!(store.worker_generation(), opening_generation);
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub restarts");
    let sink = Arc::new(CollectSink::default());
    let connection = hub
        .open_connection(
            capabilities(),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("restart connection");
    assert!(matches!(
        sink.next_raw().await,
        WireFrame::ResidentSessionBinding { .. }
    ));
    connection
        .request(
            RequestId::new("command-restart-attach"),
            RequestBody::SessionAttach {
                session_id: session_id.clone(),
                after_seq: request_seq,
                mode: AttachMode::Control,
                sealed_replay: false,
            },
        )
        .await
        .expect("control attach routes");
    let _ = attachment_from(sink.next().await);
    assert!(matches!(
        sink.next().await,
        WireFrame::AttachCaughtUp { .. }
    ));

    connection
        .menu_answer(
            Some(RequestId::new("command-restart-answer")),
            CommandId::new("command-restart-answer-id"),
            session_id.clone(),
            menu_id,
            request_seq,
            opening_generation,
            String::new(),
            0,
            Some(MenuInput::Text {
                text: "Renamed after restart".into(),
            }),
        )
        .await
        .expect("old opening coordinates recover");
    loop {
        match sink.next().await {
            WireFrame::Response {
                request_id,
                body: ResponseBody::MenuAnswer { .. },
            } if request_id.as_str() == "command-restart-answer" => break,
            WireFrame::Event { .. } => {}
            frame => panic!("unexpected restart answer frame: {frame:?}"),
        }
    }
    assert_eq!(
        store
            .session_metadata(&session_id)
            .await
            .expect("metadata reads")
            .expect("typed metadata")
            .title
            .as_deref(),
        Some("Renamed after restart")
    );

    connection.close().await.expect("connection closes");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// MUTATION CHECK: return the canonical cache-confirmation error directly
/// from `command.invoke`. Runtime failure: no typed `needs_input` card is
/// parked and the operator has no command-door path to confirm the change.
#[tokio::test]
async fn command_cache_epoch_confirmation_parks_and_resumes_through_menu_answer() {
    let (_root, store, hub) = open_hub(None, 16).await;
    let session_id = SessionId::new("command-door-cache-confirm");
    let sink = Arc::new(CollectSink::default());
    let connection = hub
        .open_connection(
            capabilities(),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("connection");
    assert!(matches!(
        sink.next_raw().await,
        WireFrame::ResidentSessionBinding { .. }
    ));
    create_and_attach_typed_session(&store, &connection, &sink, &session_id, "fake").await;

    let mut usage = envelope(
        &session_id,
        "command-door-cache-scope",
        store.worker_generation(),
    );
    usage.payload = serde_json::to_value(EventPayload::Usage(Usage {
        input: 10_000,
        output: 0,
        reasoning: 0,
        cached: 0,
        source: UsageSource::ProviderReported,
        account: None,
        accounts: Vec::new(),
        normalized: None,
        scope: Some(UsageScope {
            provider: "fake".into(),
            model: "test-model".into(),
            account_scope: None,
            auth_scope: "api_key".into(),
            api_family: None,
            effort: None,
            speed: None,
            cache_epoch: "warm-command-door".into(),
            stable_prefix_tokens: 10_000,
            cache_boundaries: None,
            request_kind: UsageRequestKind::MainTurn,
            run: None,
            agent: None,
            prefix_digests: None,
        }),
        cache_cost: None,
        request: None,
    }))
    .expect("usage encodes");
    hub.append(&mut [usage]).await.expect("warm scope commits");
    assert!(matches!(sink.next().await, WireFrame::Event { .. }));

    connection
        .request(
            RequestId::new("command-model-cache"),
            RequestBody::CommandInvoke {
                command_id: CommandId::new("command-model-cache-id"),
                command: "/model next-model".into(),
                session_id: Some(session_id.clone()),
            },
        )
        .await
        .expect("cache-sensitive command routes");
    let parked = loop {
        match sink.next().await {
            WireFrame::Response {
                request_id,
                body:
                    ResponseBody::CommandInvoke {
                        outcome: haider_rpc::CommandInvokeOutcomeWire::Parked { needs_input },
                    },
            } if request_id.as_str() == "command-model-cache" => break needs_input,
            WireFrame::Event { .. } => {}
            frame => panic!("unexpected cache park frame: {frame:?}"),
        }
    };
    assert_eq!(parked.kind, haider_rpc::NeedsInputKindWire::Choice);
    assert_eq!(parked.options.len(), 1);
    assert_eq!(parked.options[0].key, "confirm");

    connection
        .menu_answer(
            Some(RequestId::new("command-model-cache-answer")),
            CommandId::new("command-model-cache-answer-id"),
            session_id.clone(),
            parked.menu_id.expect("menu id"),
            parked.request_seq.expect("request seq"),
            parked.worker_generation.expect("worker generation"),
            "confirm".into(),
            0,
            None,
        )
        .await
        .expect("confirmation answer routes");
    loop {
        match sink.next().await {
            WireFrame::Response {
                request_id,
                body: ResponseBody::MenuAnswer { .. },
            } if request_id.as_str() == "command-model-cache-answer" => break,
            WireFrame::Event { .. } => {}
            frame => panic!("unexpected cache answer frame: {frame:?}"),
        }
    }
    assert_eq!(
        store
            .session_metadata(&session_id)
            .await
            .expect("metadata reads")
            .expect("typed metadata")
            .model,
        "next-model"
    );

    connection.close().await.expect("connection closes");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// MUTATION CHECK: replace receipt-derived generation recovery with the
/// current worker generation. Runtime failure: the same direct invocation
/// after reopen conflicts with its canonical receipt instead of replaying it.
#[tokio::test]
async fn direct_command_invoke_returns_the_canonical_receipt_nested_in_the_door() {
    let (root, store, hub) = open_hub(None, 16).await;
    let session_id = SessionId::new("command-door-direct");
    let sink = Arc::new(CollectSink::default());
    let connection = hub
        .open_connection(
            capabilities(),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("connection");
    assert!(matches!(
        sink.next_raw().await,
        WireFrame::ResidentSessionBinding { .. }
    ));
    create_and_attach_typed_session(&store, &connection, &sink, &session_id, "fake").await;

    connection
        .request(
            RequestId::new("command-rename-direct"),
            RequestBody::CommandInvoke {
                command_id: CommandId::new("command-rename-direct-id"),
                command: "/rename Direct door".into(),
                session_id: Some(session_id.clone()),
            },
        )
        .await
        .expect("direct command routes");
    loop {
        match sink.next().await {
            WireFrame::Response {
                request_id,
                body:
                    ResponseBody::CommandInvoke {
                        outcome: haider_rpc::CommandInvokeOutcomeWire::Receipt { receipt },
                    },
            } if request_id.as_str() == "command-rename-direct" => {
                assert!(matches!(
                    *receipt,
                    ResponseBody::SessionRename {
                        ref session_id,
                        ref title,
                        ..
                    } if session_id.as_str() == "command-door-direct"
                        && title.as_deref() == Some("Direct door")
                ));
                break;
            }
            WireFrame::Event { .. } => {}
            frame => panic!("unexpected direct command frame: {frame:?}"),
        }
    }

    let committed_head = store.latest_seq(&session_id).await.expect("committed head");
    connection.close().await.expect("first connection closes");
    hub.shutdown().await.expect("first hub stops");
    store.close().await.expect("first store closes");

    let store = SqliteStoreHandle::open(root.path())
        .await
        .expect("store reopens");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub reopens");
    let sink = Arc::new(CollectSink::default());
    let connection = hub
        .open_connection(
            capabilities(),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("replay connection");
    assert!(matches!(
        sink.next_raw().await,
        WireFrame::ResidentSessionBinding { .. }
    ));
    connection
        .request(
            RequestId::new("command-direct-replay-attach"),
            RequestBody::SessionAttach {
                session_id: session_id.clone(),
                after_seq: committed_head,
                mode: AttachMode::Control,
                sealed_replay: false,
            },
        )
        .await
        .expect("replay control attach routes");
    let _ = attachment_from(sink.next().await);
    assert!(matches!(
        sink.next().await,
        WireFrame::AttachCaughtUp { .. }
    ));
    connection
        .request(
            RequestId::new("command-rename-direct-replay"),
            RequestBody::CommandInvoke {
                command_id: CommandId::new("command-rename-direct-id"),
                command: "/rename Direct door".into(),
                session_id: Some(session_id.clone()),
            },
        )
        .await
        .expect("direct receipt replays after restart");
    assert!(matches!(
        sink.next().await,
        WireFrame::Response {
            request_id,
            body: ResponseBody::CommandInvoke {
                outcome: haider_rpc::CommandInvokeOutcomeWire::Receipt { receipt },
            },
        } if request_id.as_str() == "command-rename-direct-replay"
            && matches!(*receipt, ResponseBody::SessionRename { .. })
    ));
    assert_eq!(
        store.latest_seq(&session_id).await.expect("replay head"),
        committed_head,
        "receipt replay must not append a second rename"
    );

    connection.close().await.expect("connection closes");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// CG-M1 receipt law: a lost graph.pin response is recoverable without a
/// replacement control attachment. Only the first, genuinely new mutation is
/// attachment-fenced, and replay appends no second graph instance.
#[tokio::test]
async fn graph_pin_rpc_replays_its_receipt_before_control_attachment_validation() {
    let (_root, store, hub) = open_hub(None, 16).await;
    let session_id = SessionId::new("graph-pin-rpc-receipt");
    let first_sink = Arc::new(CollectSink::default());
    let first = hub
        .open_connection(
            capabilities(),
            first_sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("first connection");
    create_and_attach_typed_session(&store, &first, &first_sink, &session_id, "fake").await;
    let request = RequestBody::GraphPin {
        command_id: CommandId::new("graph-pin-rpc-command"),
        session_id: session_id.clone(),
        worker_generation: store.worker_generation(),
        template: haider_protocol::graph::SHIP_LOOP_TEMPLATE.into(),
        expected_digest: None,
    };
    first
        .request(RequestId::new("graph-pin-first"), request.clone())
        .await
        .expect("pin routes");
    let original = loop {
        if let WireFrame::Response {
            body: body @ ResponseBody::GraphPin { .. },
            ..
        } = first_sink.next().await
        {
            break body;
        }
    };
    first.close().await.expect("first connection closes");
    let head = store.latest_seq(&session_id).await.expect("head");

    let replay_sink = Arc::new(CollectSink::default());
    let replay = hub
        .open_connection(
            capabilities(),
            replay_sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("replay connection");
    replay
        .request(RequestId::new("graph-pin-replay"), request)
        .await
        .expect("receipt replay routes");
    let WireFrame::Response { body, .. } = replay_sink.next().await else {
        panic!("unattached replay must answer directly");
    };
    assert_eq!(body, original);
    assert_eq!(store.latest_seq(&session_id).await.expect("head"), head);

    replay.close().await.expect("replay connection closes");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// ITEM #3 harness: a pinned rev remains addressable after registry edit,
/// while fresh pin/switch requests fenced to that stale digest return the
/// typed current digest + revision instead of selecting different bytes.
#[tokio::test]
async fn workflow_instance_rpc_retains_pinned_revision_and_fences_pin_and_switch() {
    let (_root, store, hub) = open_hub(None, 16).await;
    let first_registration = store
        .loom_register_workflow("rpc-retained: A -> A\nstep \"one\" :cmd".into())
        .await
        .expect("register rev 1");
    let first = store
        .loom_workflow("rpc-retained".into())
        .await
        .expect("read rev 1")
        .expect("rev 1 exists");
    let first_digest = haider_protocol::graph::graph_template_digest(&first.template);

    let sink = Arc::new(CollectSink::default());
    let connection = hub
        .open_connection(
            capabilities(),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("connection");
    let pinned_session = SessionId::new("workflow-instance-pinned");
    create_and_attach_typed_session(&store, &connection, &sink, &pinned_session, "fake").await;
    connection
        .request(
            RequestId::new("workflow-instance-pin-rev-1"),
            RequestBody::GraphPin {
                command_id: CommandId::new("workflow-instance-pin-rev-1-command"),
                session_id: pinned_session.clone(),
                worker_generation: store.worker_generation(),
                template: "rpc-retained".into(),
                expected_digest: Some(first_digest.clone()),
            },
        )
        .await
        .expect("fenced pin routes");
    let (old_graph_id, pinned_digest) = loop {
        if let WireFrame::Response {
            body: ResponseBody::GraphPin {
                graph_id, digest, ..
            },
            ..
        } = sink.next().await
        {
            break (graph_id, digest);
        }
    };
    assert_eq!(pinned_digest, first_digest);

    store
        .loom_register_workflow_cas(
            "rpc-retained: A -> A\nstep \"two\" :cmd".into(),
            haider_protocol::loom::LoomRevisionExpectation {
                rev: first_registration.rev,
                digest: Some(first_registration.digest),
            },
        )
        .await
        .expect("register rev 2");
    let current = store
        .loom_workflow("rpc-retained".into())
        .await
        .expect("read rev 2")
        .expect("rev 2 exists");
    let current_digest = haider_protocol::graph::graph_template_digest(&current.template);

    connection
        .request(
            RequestId::new("workflow-instance-read-pinned"),
            RequestBody::WorkflowInstance {
                workflow_id: "rpc-retained".into(),
                template_digest: Some(pinned_digest.clone()),
            },
        )
        .await
        .expect("historical instance read routes");
    let WireFrame::Response {
        body: ResponseBody::WorkflowInstance { instance },
        ..
    } = sink.next().await
    else {
        panic!("expected workflow.instance response");
    };
    let instance = instance.expect("retained revision descriptor");
    assert_eq!(instance.id, "rpc-retained");
    assert_eq!(instance.revision, 1);
    assert_eq!(instance.digest.as_deref(), Some(first.digest.as_str()));
    assert_eq!(instance.template_digest, pinned_digest);
    assert_eq!(
        instance.pipe_version.as_deref(),
        Some(first.pipe_version.as_str())
    );
    assert_eq!(instance.source, haider_rpc::WorkflowInstanceSourceV1::User);
    assert_eq!(instance.node_metadata.as_ref(), Some(&first.meta));
    assert_eq!(instance.compiled_template, first.template);

    let stale_session = SessionId::new("workflow-instance-stale-pin");
    create_and_attach_typed_session(&store, &connection, &sink, &stale_session, "fake").await;
    connection
        .request(
            RequestId::new("workflow-instance-stale-pin"),
            RequestBody::GraphPin {
                command_id: CommandId::new("workflow-instance-stale-pin-command"),
                session_id: stale_session,
                worker_generation: store.worker_generation(),
                template: "rpc-retained".into(),
                expected_digest: Some(pinned_digest.clone()),
            },
        )
        .await
        .expect("stale pin routes");
    let WireFrame::Response {
        body:
            ResponseBody::Error {
                code,
                data:
                    Some(ErrorData::WorkflowRevisionConflict {
                        expected_digest,
                        current_digest: reported_digest,
                        current_revision,
                    }),
                ..
            },
        ..
    } = sink.next().await
    else {
        panic!("expected typed stale-pin conflict");
    };
    assert_eq!(code, haider_rpc::ERROR_CODE_REVISION_CONFLICT);
    assert_eq!(expected_digest, pinned_digest);
    assert_eq!(reported_digest, current_digest);
    assert_eq!(current_revision, 2);

    connection
        .request(
            RequestId::new("workflow-instance-stale-switch"),
            RequestBody::GraphSwitch {
                command_id: CommandId::new("workflow-instance-stale-switch-command"),
                session_id: pinned_session,
                worker_generation: store.worker_generation(),
                old_graph_id,
                template: "rpc-retained".into(),
                expected_digest: Some(pinned_digest.clone()),
            },
        )
        .await
        .expect("stale switch routes");
    let WireFrame::Response {
        body:
            ResponseBody::Error {
                data:
                    Some(ErrorData::WorkflowRevisionConflict {
                        current_digest: reported_digest,
                        current_revision,
                        ..
                    }),
                ..
            },
        ..
    } = sink.next().await
    else {
        panic!("expected typed stale-switch conflict");
    };
    assert_eq!(reported_digest, current_digest);
    assert_eq!(current_revision, 2);

    connection.close().await.expect("connection closes");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// Native workflow selection and turn admission share one serialization
/// boundary: once the accepted run is nonterminal, neither pin nor switch can
/// alter the authority of an already assembled provider request.
#[tokio::test]
async fn graph_pin_and_switch_refuse_nonterminal_sessions() {
    let (_root, store, hub) = open_hub(None, 16).await;
    let sink = Arc::new(CollectSink::default());
    let connection = hub
        .open_connection(
            capabilities(),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("connection");

    let pin_session = SessionId::new("graph-pin-during-turn");
    create_and_attach_typed_session(&store, &connection, &sink, &pin_session, "fake").await;
    hub.append(&mut [run_state_pipe_event(
        &pin_session,
        "graph-pin-active-run",
        store.worker_generation(),
        "graph-pin-active-run",
        RunState::Thinking,
    )])
    .await
    .expect("active pin run");
    let pin_request_id = RequestId::new("graph-pin-during-turn-request");
    connection
        .request(
            pin_request_id.clone(),
            RequestBody::GraphPin {
                command_id: CommandId::new("graph-pin-during-turn-command"),
                session_id: pin_session.clone(),
                worker_generation: store.worker_generation(),
                template: haider_protocol::graph::SHIP_LOOP_TEMPLATE.into(),
                expected_digest: None,
            },
        )
        .await
        .expect("pin request routes");
    let pin_error = loop {
        if let WireFrame::Response { request_id, body } = sink.next().await
            && request_id == pin_request_id
        {
            break body;
        }
    };
    assert!(matches!(
        pin_error,
        ResponseBody::Error { ref code, .. } if code == ERROR_CODE_BUSY
    ));

    let switch_session = SessionId::new("graph-switch-during-turn");
    create_and_attach_typed_session(&store, &connection, &sink, &switch_session, "fake").await;
    let initial_request_id = RequestId::new("graph-switch-initial-pin");
    connection
        .request(
            initial_request_id.clone(),
            RequestBody::GraphPin {
                command_id: CommandId::new("graph-switch-initial-pin-command"),
                session_id: switch_session.clone(),
                worker_generation: store.worker_generation(),
                template: haider_protocol::graph::SHIP_LOOP_TEMPLATE.into(),
                expected_digest: None,
            },
        )
        .await
        .expect("initial pin routes");
    let old_graph_id = loop {
        if let WireFrame::Response { request_id, body } = sink.next().await
            && request_id == initial_request_id
            && let ResponseBody::GraphPin { graph_id, .. } = body
        {
            break graph_id;
        }
    };
    hub.append(&mut [run_state_pipe_event(
        &switch_session,
        "graph-switch-active-run",
        store.worker_generation(),
        "graph-switch-active-run",
        RunState::Thinking,
    )])
    .await
    .expect("active switch run");
    let switch_request_id = RequestId::new("graph-switch-during-turn-request");
    connection
        .request(
            switch_request_id.clone(),
            RequestBody::GraphSwitch {
                command_id: CommandId::new("graph-switch-during-turn-command"),
                session_id: switch_session,
                worker_generation: store.worker_generation(),
                old_graph_id,
                template: haider_protocol::graph::SHIP_LOOP_TEMPLATE.into(),
                expected_digest: None,
            },
        )
        .await
        .expect("switch request routes");
    let switch_error = loop {
        if let WireFrame::Response { request_id, body } = sink.next().await
            && request_id == switch_request_id
        {
            break body;
        }
    };
    assert!(matches!(
        switch_error,
        ResponseBody::Error { ref code, .. } if code == ERROR_CODE_BUSY
    ));

    connection.close().await.expect("connection closes");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// CG-M1 receipt law for the companion mutation: a lost graph.abandon
/// response also replays before replacement control-attachment validation.
#[tokio::test]
async fn graph_abandon_rpc_replays_its_receipt_before_control_attachment_validation() {
    let (_root, store, hub) = open_hub(None, 16).await;
    let session_id = SessionId::new("graph-abandon-rpc-receipt");
    let first_sink = Arc::new(CollectSink::default());
    let first = hub
        .open_connection(
            capabilities(),
            first_sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("first connection");
    create_and_attach_typed_session(&store, &first, &first_sink, &session_id, "fake").await;
    first
        .request(
            RequestId::new("graph-abandon-pin"),
            RequestBody::GraphPin {
                command_id: CommandId::new("graph-abandon-pin-command"),
                session_id: session_id.clone(),
                worker_generation: store.worker_generation(),
                template: haider_protocol::graph::SHIP_LOOP_TEMPLATE.into(),
                expected_digest: None,
            },
        )
        .await
        .expect("pin routes");
    loop {
        if matches!(
            first_sink.next().await,
            WireFrame::Response {
                body: ResponseBody::GraphPin { .. },
                ..
            }
        ) {
            break;
        }
    }
    let request = RequestBody::GraphAbandon {
        command_id: CommandId::new("graph-abandon-rpc-command"),
        session_id: session_id.clone(),
        worker_generation: store.worker_generation(),
        why: "operator stopped".into(),
    };
    first
        .request(RequestId::new("graph-abandon-first"), request.clone())
        .await
        .expect("abandon routes");
    let original = loop {
        if let WireFrame::Response {
            body: body @ ResponseBody::GraphAbandon { .. },
            ..
        } = first_sink.next().await
        {
            break body;
        }
    };
    first.close().await.expect("first connection closes");
    let head = store.latest_seq(&session_id).await.expect("head");

    let replay_sink = Arc::new(CollectSink::default());
    let replay = hub
        .open_connection(
            capabilities(),
            replay_sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("replay connection");
    replay
        .request(RequestId::new("graph-abandon-replay"), request)
        .await
        .expect("receipt replay routes");
    let WireFrame::Response { body, .. } = replay_sink.next().await else {
        panic!("unattached replay must answer directly");
    };
    assert_eq!(body, original);
    assert_eq!(store.latest_seq(&session_id).await.expect("head"), head);

    replay.close().await.expect("replay connection closes");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

fn fleet_delegation(
    root_session_id: &SessionId,
    parent_session_id: &SessionId,
    child_session_id: &SessionId,
    suffix: &str,
    parent_agent_id: Option<AgentId>,
    depth: u32,
) -> DelegationRecord {
    let agent_id = AgentId::new(format!("fleet-agent-{suffix}"));
    DelegationRecord {
        agent_id: agent_id.clone(),
        child_session_id: child_session_id.clone(),
        child_run_id: RunId::new(format!("fleet-child-run-{suffix}")),
        parent_session_id: parent_session_id.clone(),
        parent_run_id: RunId::new(format!("fleet-parent-run-{suffix}")),
        parent_branch_id: None,
        call_id: format!("fleet-call-{suffix}"),
        tool_item_id: ItemId::new(format!("fleet-tool-item-{suffix}")),
        parent_agent_id: parent_agent_id.clone(),
        root_session_id: root_session_id.clone(),
        depth,
        task: format!("task {suffix}"),
        prompt: format!("prompt {suffix}"),
        manifest: AgentManifest {
            agent: agent_id,
            role: AgentRole::Subagent,
            task: format!("task {suffix}"),
            callsign: Some(format!("CALL-{suffix}")),
            model_profile: "gpt-5.2".into(),
            grant: Grant {
                tools: vec!["fs_read".into()],
                effect_ceiling: Vec::new(),
            },
            budget_tokens: Some(4096),
            placement: Placement::Local,
            lease: LeaseId::new(format!("fleet-lease-{suffix}")),
            fencing_epoch: 1,
            attempt: 0,
            parent: parent_agent_id,
            coordinates: Some(serde_json::json!({"provider": "anthropic"})),
            cli_scope: None,
        },
        state: DelegationState::Spawned,
        report: None,
    }
}

async fn append_fleet_metrics(
    hub: &SessionHub,
    generation: u64,
    record: &DelegationRecord,
    final_state: RunState,
    tokens: u64,
) {
    let scoped = |suffix: &str, payload: serde_json::Value| {
        let mut event = envelope(
            &record.child_session_id,
            format!("fleet-{suffix}-{}", record.agent_id),
            generation,
        );
        event.run_id = Some(record.child_run_id.clone());
        event.agent_id = Some(record.agent_id.clone());
        event.payload = payload;
        event
    };
    let started = scoped(
        "started",
        serde_json::to_value(EventPayload::RunState(RunState::Thinking)).expect("thinking state"),
    );
    let tool = scoped(
        "tool",
        serde_json::to_value(EventPayload::Item(ItemEvent::Started {
            item_id: ItemId::new(format!("fleet-metrics-tool-{}", record.agent_id)),
            item: TurnItem::ToolCall {
                call_id: format!("fleet-metrics-call-{}", record.agent_id),
                name: "fs_read".into(),
                args: serde_json::json!({}),
                status: ToolStatus::InProgress,
            },
        }))
        .expect("tool event"),
    );
    let usage = scoped(
        "usage",
        serde_json::to_value(EventPayload::Usage(Usage {
            input: tokens,
            output: 1,
            reasoning: 0,
            cached: 0,
            source: UsageSource::ProviderReported,
            account: Some(CredentialAlias::new("fleet-billing")),
            accounts: Vec::new(),
            normalized: None,
            scope: Some(UsageScope {
                provider: "openai".into(),
                model: "gpt-5.2".into(),
                account_scope: Some(CredentialAlias::new("fleet-billing")),
                auth_scope: "api_key".into(),
                api_family: None,
                effort: None,
                speed: None,
                cache_epoch: "fleet-test".into(),
                stable_prefix_tokens: 0,
                cache_boundaries: None,
                request_kind: UsageRequestKind::DelegatedAgent,
                run: Some(record.child_run_id.clone()),
                agent: Some(record.agent_id.clone()),
                prefix_digests: None,
            }),
            cache_cost: None,
            request: None,
        }))
        .expect("usage event"),
    );
    let terminal = scoped(
        "state",
        serde_json::to_value(EventPayload::RunState(final_state)).expect("final state"),
    );
    hub.append(&mut [started, tool, usage, terminal])
        .await
        .expect("fleet metrics commit");
}

fn pdf_fixture(pages: u32, content: Option<&str>) -> Vec<u8> {
    let kids = (0..pages)
        .map(|index| format!("{} 0 R", index + 3))
        .collect::<Vec<_>>()
        .join(" ");
    let content_id = pages + 3;
    let mut pdf = format!(
        "%PDF-1.4\n1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n\
         2 0 obj\n<< /Type /Pages /Count {pages} /Kids [{kids}] >>\nendobj\n"
    );
    for index in 0..pages {
        let contents = content.map_or_else(String::new, |_| format!(" /Contents {content_id} 0 R"));
        pdf.push_str(&format!(
            "{} 0 obj\n<< /Type /Page /Parent 2 0 R{contents} >>\nendobj\n",
            index + 3
        ));
    }
    if let Some(content) = content {
        pdf.push_str(&format!(
            "{content_id} 0 obj\n<< /Length {} >>\nstream\n{content}\nendstream\nendobj\n",
            content.len()
        ));
    }
    pdf.push_str("trailer\n<< /Root 1 0 R >>\n%%EOF\n");
    pdf.into_bytes()
}

/// v0.0.940 stopped marking reasoning rows `compat`, but the fix could not
/// reach the 93 rows already on disk: with `SIDECAR_VERSION` unchanged the
/// rebuild trigger never fired, so every file kept v4's flags and went on
/// advertising reasoning as droppable. v5 exists to force that rewrite.
///
/// This pins the REACH, not the fix — that a REBUILD lands the corrected flag
/// on a file that previously carried the wrong one. The failure mode it guards
/// is quiet: files rebuild, versions match, flags stay wrong, nothing errors.
///
/// MUTATION CHECK (executed): revert `SIDECAR_VERSION` to 4. Expected RUNTIME
/// failure: the stale file is considered current, no rebuild runs, and the
/// wrong `compat: true` survives the assertion below.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_v4_file_rebuilds_with_the_reasoning_compat_flag_cleared() {
    let (root, store, hub) = open_hub(None, 8).await;
    let session_id = SessionId::new("native-pipe-compat-reach");
    let generation = store.worker_generation();
    let reasoning_id = ItemId::new("reach-reasoning");

    let mut started = pipe_event(
        &session_id,
        "reach-started",
        generation,
        EventPayload::Item(ItemEvent::Started {
            item_id: reasoning_id.clone(),
            item: TurnItem::Reasoning {
                summary: String::new(),
            },
        }),
    );
    started.run_id = Some(RunId::new("reach-run"));
    let mut assistant = pipe_event(
        &session_id,
        "reach-assistant",
        generation,
        EventPayload::NodeCommitted(TreeNode {
            node: NodeId::new("reach-assistant-node"),
            parent: None,
            kind: NodeKind::AssistantCommit {
                text: "the answer".into(),
                verdict: VerifyVerdict::Unverified,
            },
        }),
    );
    assistant.run_id = Some(RunId::new("reach-run"));
    let mut sealed = pipe_event(
        &session_id,
        "reach-sealed",
        generation,
        EventPayload::Item(ItemEvent::Completed {
            item_id: reasoning_id,
            item: TurnItem::Reasoning {
                summary: "the thinking".into(),
            },
        }),
    );
    sealed.run_id = Some(RunId::new("reach-run"));
    let mut seed = vec![started, assistant, sealed];
    hub.append(&mut seed).await.expect("seed commits");
    hub.shutdown().await.expect("hub stops");

    // Stand in for what v0.0.939 actually left on disk: a CURRENT-looking v4
    // file whose reasoning row wears the flag the contract now forbids.
    let path = sidecar_path(&root, &session_id);
    let head = seed.last().expect("head").seq;
    std::fs::write(
        &path,
        format!(
            "{{\"pipe\":\"haider.session.jsonl\",\"version\":4,\"session_id\":\"{session_id}\",\"generation\":4}}\n             {{\"role\":\"assistant\",\"text\":\"the answer\",\"reasoning\":\"the thinking\",\"at_ms\":1,\"seq\":{head},\"ordinal\":0,\"compat\":true}}\n"
        ),
    )
    .expect("stale v4 sidecar writes");
    assert!(
        std::fs::read_to_string(&path)
            .expect("stale reads")
            .contains("\"compat\":true"),
        "precondition: the stale file carries the WRONG flag"
    );

    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub restarts");
    let mut trigger = vec![user_pipe_event(
        &session_id,
        "reach-trigger",
        generation,
        "next",
    )];
    hub.append(&mut trigger).await.expect("trigger commits");
    hub.shutdown().await.expect("second hub stops");

    let rebuilt = std::fs::read_to_string(&path).expect("rebuilt reads");
    let header: serde_json::Value =
        serde_json::from_str(rebuilt.lines().next().expect("header")).expect("header JSON");
    assert_eq!(header["version"], 6, "the bump forced a rebuild");

    let reasoning_row: serde_json::Value = rebuilt
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|row| row.get("reasoning").is_some())
        .expect("the rebuilt reasoning row");
    assert_eq!(reasoning_row["reasoning"], "the thinking");
    assert!(
        reasoning_row.get("compat").is_none(),
        "the rebuild must land the CORRECTED flag, not reproduce the stale one: {reasoning_row}"
    );
}

/// V6 must rewrite files already projected by v5; otherwise the daemon can
/// advertise typed tool status while the cold file at EOF still omits it.
/// The authoritative old journal has no node field either, so the rebuild
/// must recover Rejected from the preceding completed tool item.
///
/// MUTATION CHECK: revert `SIDECAR_VERSION` to 5. Expected runtime failure:
/// the stale v5 row remains current and has no typed `status` field.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_v5_file_rebuilds_with_rejected_tool_status() {
    let (root, store, hub) = open_hub(None, 8).await;
    let session_id = SessionId::new("native-pipe-tool-status-reach");
    let generation = store.worker_generation();
    let run_id = RunId::new("tool-status-reach-run");

    let mut completed = pipe_event(
        &session_id,
        "tool-status-reach-item",
        generation,
        EventPayload::Item(ItemEvent::Completed {
            item_id: ItemId::new("tool-status-reach-item"),
            item: TurnItem::ToolCall {
                call_id: "tool-status-reach-call".into(),
                name: "fs_write".into(),
                args: serde_json::json!({"path": "guarded"}),
                status: ToolStatus::Rejected,
            },
        }),
    );
    completed.run_id = Some(run_id.clone());
    let mut legacy_node = pipe_event(
        &session_id,
        "tool-status-reach-node",
        generation,
        EventPayload::NodeCommitted(TreeNode {
            node: NodeId::new("tool-status-reach-node"),
            parent: None,
            kind: NodeKind::ToolExchange {
                tool: "fs_write".into(),
                summary: "tool call settled as Rejected".into(),
                artifact: None,
            },
        }),
    );
    legacy_node.run_id = Some(run_id);
    let mut seed = vec![completed, legacy_node];
    hub.append(&mut seed).await.expect("legacy journal commits");
    hub.shutdown().await.expect("hub stops");

    let path = sidecar_path(&root, &session_id);
    let head = seed.last().expect("head").seq;
    std::fs::write(
        &path,
        format!(
            "{{\"pipe\":\"haider.session.jsonl\",\"version\":5,\"session_id\":\"{session_id}\",\"generation\":{generation}}}\n\
             {{\"role\":\"tool\",\"name\":\"fs_write\",\"summary\":\"tool call settled as Rejected\",\"at_ms\":1,\"seq\":{head},\"ordinal\":0}}\n"
        ),
    )
    .expect("stale v5 sidecar writes");

    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub restarts");
    let mut trigger = vec![user_pipe_event(
        &session_id,
        "tool-status-reach-trigger",
        generation,
        "next",
    )];
    hub.append(&mut trigger).await.expect("trigger commits");
    hub.shutdown().await.expect("second hub stops");

    let rebuilt = std::fs::read_to_string(&path).expect("rebuilt reads");
    let header: serde_json::Value =
        serde_json::from_str(rebuilt.lines().next().expect("header")).expect("header JSON");
    assert_eq!(header["version"], 6, "the bump forced a rebuild");
    let tool_row: serde_json::Value = rebuilt
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|row| row.get("name").is_some_and(|name| name == "fs_write"))
        .expect("rebuilt tool row");
    assert_eq!(tool_row["status"], "rejected");
    assert_eq!(tool_row["summary"], "tool call settled as Rejected");
}

/// MUTATION CHECK: bypass daemon CAS ingress, return a caller-provided ref,
/// or rewrite an existing object. Expected RUNTIME failure: the verified
/// BLAKE3 ref/byte count differs, the stored bytes differ, or the second put
/// does not return the identical content address.
#[tokio::test]
async fn artifact_put_roundtrip_is_content_addressed_and_idempotent() {
    let (_root, store, hub) = open_hub(None, 8).await;
    let sink = Arc::new(CollectSink::default());
    let connection = hub
        .open_connection(
            capabilities(),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("connection");
    let mut bytes = vec![0_u8; 5 * 1024 * 1024];
    bytes[..8].copy_from_slice(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
    let first = upload_bytes(&connection, &sink, "put-first", &bytes).await;
    let second = upload_bytes(&connection, &sink, "put-second", &bytes).await;
    let (
        ResponseBody::ArtifactPut {
            artifact: first_ref,
            bytes: first_bytes,
        },
        ResponseBody::ArtifactPut {
            artifact: second_ref,
            bytes: second_bytes,
        },
    ) = (first, second)
    else {
        panic!("both uploads must succeed");
    };
    assert_eq!(first_ref, second_ref);
    assert_eq!(first_bytes, bytes.len() as u64);
    assert_eq!(second_bytes, bytes.len() as u64);
    assert_eq!(
        store.get(&first_ref).await.expect("verified CAS read"),
        bytes
    );

    connection.close().await.expect("connection closes");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// MUTATION CHECK: remove the decoded put cap, the per-attachment cap, or
/// the aggregate per-turn cap. Expected RUNTIME failure: one of the three
/// requests succeeds or returns an untyped/general error instead of its
/// stable code and structured limit coordinates.
#[tokio::test]
async fn oversized_put_and_oversized_turn_are_typed_errors() {
    let (_root, store, hub) = open_hub(None, 8).await;
    let sink = Arc::new(CollectSink::default());
    let connection = hub
        .open_connection(
            capabilities(),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("connection");

    let oversized = vec![0_u8; ARTIFACT_PUT_MAX_BYTES + 1];
    assert!(matches!(
        upload_bytes(&connection, &sink, "put-oversized", &oversized).await,
        ResponseBody::Error {
            ref code,
            data: Some(ErrorData::ArtifactTooLarge { .. }),
            ..
        } if code == ERROR_CODE_ARTIFACT_TOO_LARGE
    ));

    let session_id = SessionId::new("attachment-size-validation");
    attach_control_session(&hub, &store, &connection, &sink, &session_id).await;
    let per_attachment = vec![b'x'; 5 * 1024 * 1024 + 1];
    let ResponseBody::ArtifactPut {
        artifact: oversized_ref,
        ..
    } = upload_bytes(&connection, &sink, "put-over-turn-limit", &per_attachment).await
    else {
        panic!("put below the 8 MiB ingress cap succeeds");
    };
    connection
        .request(
            RequestId::new("submit-over-attachment-limit"),
            RequestBody::TurnSubmit {
                command_id: CommandId::new("submit-over-attachment-limit"),
                session_id: session_id.clone(),
                worker_generation: store.worker_generation(),
                text: "inspect paste".into(),
                attachments: vec![AttachmentBlock::PastedText {
                    artifact: oversized_ref,
                    lines: 1,
                }],
                mode: DeliveryMode::Queue,
            },
        )
        .await
        .expect("oversized submit routes");
    assert!(matches!(
        sink.next().await,
        WireFrame::Response {
            body: ResponseBody::Error { ref code, data: Some(ErrorData::AttachmentTooLarge { .. }), .. },
            ..
        } if code == ERROR_CODE_ATTACHMENT_TOO_LARGE
    ));

    let exact_five = vec![b'y'; 5 * 1024 * 1024];
    let ResponseBody::ArtifactPut {
        artifact: five_ref, ..
    } = upload_bytes(&connection, &sink, "put-five-mib", &exact_five).await
    else {
        panic!("5 MiB put succeeds");
    };
    connection
        .request(
            RequestId::new("submit-over-total-limit"),
            RequestBody::TurnSubmitWithBranch {
                command_id: CommandId::new("submit-over-total-limit"),
                session_id: session_id.clone(),
                worker_generation: store.worker_generation(),
                branch_id: None,
                text: "inspect repeated pastes".into(),
                attachments: (0..4)
                    .map(|_| AttachmentBlock::PastedText {
                        artifact: five_ref.clone(),
                        lines: 1,
                    })
                    .collect(),
                mode: DeliveryMode::Queue,
            },
        )
        .await
        .expect("aggregate submit routes");
    assert!(matches!(
        sink.next().await,
        WireFrame::Response {
            body: ResponseBody::Error { ref code, data: Some(ErrorData::AttachmentsTooLarge { .. }), .. },
            ..
        } if code == ERROR_CODE_ATTACHMENTS_TOO_LARGE
    ));
    connection
        .request(
            RequestId::new("submit-too-many-attachments"),
            RequestBody::TurnSubmit {
                command_id: CommandId::new("submit-too-many-attachments"),
                session_id: session_id.clone(),
                worker_generation: store.worker_generation(),
                text: "too many pastes".into(),
                attachments: (0..6)
                    .map(|_| AttachmentBlock::PastedText {
                        artifact: five_ref.clone(),
                        lines: 1,
                    })
                    .collect(),
                mode: DeliveryMode::Queue,
            },
        )
        .await
        .expect("count-limited submit routes");
    assert!(matches!(
        sink.next().await,
        WireFrame::Response {
            body: ResponseBody::Error { ref code, data: Some(ErrorData::TooManyAttachments { .. }), .. },
            ..
        } if code == ERROR_CODE_TOO_MANY_ATTACHMENTS
    ));
    assert_eq!(store.latest_seq(&session_id).await.expect("head"), 1);

    connection.close().await.expect("connection closes");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

#[tokio::test]
async fn pdf_byte_page_and_parse_caps_are_typed_at_daemon_admission() {
    let (_root, store, hub) = open_hub(None, 8).await;
    let sink = Arc::new(CollectSink::default());
    let connection = hub
        .open_connection(
            capabilities(),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("connection");
    let session_id = SessionId::new("pdf-admission-validation");
    create_and_attach_typed_session(&store, &connection, &sink, &session_id, "openai").await;

    let cases = [
        (
            "pdf-malformed",
            b"%PDF-1.4\nnot a PDF".to_vec(),
            ERROR_CODE_PDF_MALFORMED,
            "pdf-malformed",
        ),
        (
            "pdf-too-many-pages",
            pdf_fixture(haider_pdf::MAX_PDF_PAGES + 1, None),
            ERROR_CODE_PDF_TOO_MANY_PAGES,
            "pdf-too-many-pages",
        ),
    ];
    for (label, bytes, expected_code, expected_subcode) in cases {
        let ResponseBody::ArtifactPut { artifact, .. } =
            upload_bytes(&connection, &sink, &format!("put-{label}"), &bytes).await
        else {
            panic!("PDF fixture upload succeeds")
        };
        connection
            .request(
                RequestId::new(format!("submit-{label}")),
                RequestBody::TurnSubmit {
                    command_id: CommandId::new(format!("submit-{label}")),
                    session_id: session_id.clone(),
                    worker_generation: store.worker_generation(),
                    text: "read PDF".into(),
                    attachments: vec![AttachmentBlock::Pdf {
                        artifact,
                        name: "report.pdf".into(),
                        pages: 1,
                        delivery: PdfDeliveryMode::ExtractedText,
                    }],
                    mode: DeliveryMode::Queue,
                },
            )
            .await
            .expect("PDF submit routes");
        let WireFrame::Response {
            body: ResponseBody::Error { code, data, .. },
            ..
        } = sink.next().await
        else {
            panic!("PDF rejection response")
        };
        assert_eq!(code, expected_code);
        let presentation = match data.expect("typed PDF error data") {
            ErrorData::PdfMalformed { presentation, .. }
            | ErrorData::PdfTooManyPages { presentation, .. } => presentation,
            other => panic!("wrong PDF error data: {other:?}"),
        };
        assert_eq!(presentation.subcode.as_str(), expected_subcode);
    }

    let mut oversized = pdf_fixture(1, None);
    oversized.resize(haider_pdf::MAX_PDF_BYTES + 1, b' ');
    let ResponseBody::ArtifactPut { artifact, .. } =
        upload_bytes(&connection, &sink, "put-pdf-too-large", &oversized).await
    else {
        panic!("artifact ingress intentionally exceeds the narrower PDF cap")
    };
    connection
        .request(
            RequestId::new("submit-pdf-too-large"),
            RequestBody::TurnSubmit {
                command_id: CommandId::new("submit-pdf-too-large"),
                session_id: session_id.clone(),
                worker_generation: store.worker_generation(),
                text: "read oversized PDF".into(),
                attachments: vec![AttachmentBlock::Pdf {
                    artifact,
                    name: "large.pdf".into(),
                    pages: 1,
                    delivery: PdfDeliveryMode::ExtractedText,
                }],
                mode: DeliveryMode::Queue,
            },
        )
        .await
        .expect("oversized PDF submit routes");
    assert!(matches!(
        sink.next().await,
        WireFrame::Response {
            body: ResponseBody::Error {
                ref code,
                data: Some(ErrorData::PdfTooLarge { ref presentation, .. }),
                ..
            },
            ..
        } if code == ERROR_CODE_PDF_TOO_LARGE
            && presentation.subcode.as_str() == "pdf-too-large"
    ));
    assert_eq!(store.latest_seq(&session_id).await.expect("head"), 1);

    connection.close().await.expect("connection closes");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// MUTATION CHECK: defer artifact/mime checks until worker startup. Expected
/// RUNTIME failure: either submit is durably accepted (head advances beyond
/// one) or the correlated response loses the exact remediable error code.
#[tokio::test]
async fn dangling_ref_rejected_at_submit_not_at_run() {
    let (_root, store, hub) = open_hub(None, 8).await;
    let sink = Arc::new(CollectSink::default());
    let connection = hub
        .open_connection(
            capabilities(),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("connection");
    let session_id = SessionId::new("attachment-reference-validation");
    attach_control_session(&hub, &store, &connection, &sink, &session_id).await;
    let ResponseBody::ArtifactPut { artifact, .. } =
        upload_bytes(&connection, &sink, "put-mime-fixture", b"image bytes").await
    else {
        panic!("mime fixture upload succeeds");
    };

    connection
        .request(
            RequestId::new("submit-bad-mime"),
            RequestBody::TurnSubmit {
                command_id: CommandId::new("submit-bad-mime"),
                session_id: session_id.clone(),
                worker_generation: store.worker_generation(),
                text: "bad mime".into(),
                attachments: vec![AttachmentBlock::Image {
                    artifact,
                    mime: "image/svg+xml".into(),
                    width: None,
                    height: None,
                }],
                mode: DeliveryMode::Queue,
            },
        )
        .await
        .expect("bad MIME submit routes");
    assert!(matches!(
        sink.next().await,
        WireFrame::Response {
            body: ResponseBody::Error { ref code, data: Some(ErrorData::AttachmentMimeUnsupported { .. }), .. },
            ..
        } if code == ERROR_CODE_ATTACHMENT_MIME_UNSUPPORTED
    ));

    let dangling = ArtifactRef::new(format!("blake3:{}", "0".repeat(64)));
    connection
        .request(
            RequestId::new("submit-dangling"),
            RequestBody::TurnSubmitWithBranch {
                command_id: CommandId::new("submit-dangling"),
                session_id: session_id.clone(),
                worker_generation: store.worker_generation(),
                branch_id: None,
                text: "dangling image".into(),
                attachments: vec![AttachmentBlock::Image {
                    artifact: dangling.clone(),
                    mime: "image/png".into(),
                    width: None,
                    height: None,
                }],
                mode: DeliveryMode::Queue,
            },
        )
        .await
        .expect("dangling submit routes");
    assert!(matches!(
        sink.next().await,
        WireFrame::Response {
            body: ResponseBody::Error {
                ref code,
                data: Some(ErrorData::AttachmentNotFound { artifact, .. }),
                ..
            },
            ..
        } if code == ERROR_CODE_ATTACHMENT_NOT_FOUND && artifact == dangling
    ));
    assert_eq!(store.latest_seq(&session_id).await.expect("head"), 1);

    connection.close().await.expect("connection closes");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// MUTATION CHECK: accept an arbitrary caller-declared image MIME. Expected
/// RUNTIME failure: SVG reaches durable acceptance instead of returning the
/// exact typed allowlist refusal while the journal head remains unchanged.
#[tokio::test]
async fn mime_allowlist_enforced_at_acceptance() {
    assert_eq!(
        IMAGE_ATTACHMENT_MIME_ALLOWLIST,
        ["image/jpeg", "image/png", "image/gif", "image/webp"]
    );
    let (_root, store, hub) = open_hub(None, 8).await;
    let sink = Arc::new(CollectSink::default());
    let connection = hub
        .open_connection(
            capabilities(),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("connection");
    let session_id = SessionId::new("attachment-mime-validation");
    attach_control_session(&hub, &store, &connection, &sink, &session_id).await;
    let ResponseBody::ArtifactPut { artifact, .. } =
        upload_bytes(&connection, &sink, "put-mime-only-fixture", b"image bytes").await
    else {
        panic!("mime fixture upload succeeds");
    };
    connection
        .request(
            RequestId::new("submit-mime-only"),
            RequestBody::TurnSubmit {
                command_id: CommandId::new("submit-mime-only"),
                session_id: session_id.clone(),
                worker_generation: store.worker_generation(),
                text: "bad mime".into(),
                attachments: vec![AttachmentBlock::Image {
                    artifact,
                    mime: "image/svg+xml".into(),
                    width: None,
                    height: None,
                }],
                mode: DeliveryMode::Queue,
            },
        )
        .await
        .expect("bad MIME submit routes");
    assert!(matches!(
        sink.next().await,
        WireFrame::Response {
            body: ResponseBody::Error {
                ref code,
                data: Some(ErrorData::AttachmentMimeUnsupported { .. }),
                ..
            },
            ..
        } if code == ERROR_CODE_ATTACHMENT_MIME_UNSUPPORTED
    ));
    assert_eq!(store.latest_seq(&session_id).await.expect("head"), 1);

    connection.close().await.expect("connection closes");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// LAW (LA2 daemon half + LA3 name sanity, G2): the daemon re-gates File
/// attachments independently of the client — a CAS payload that is not
/// UTF-8 is refused at ACCEPTANCE (never at run), and a name that is empty,
/// over 120 characters, path-shaped, or control-laced is refused before any
/// CAS read. The journal head never advances for a refused submit.
///
/// MUTATION CHECK: remove the `requires_utf8` decode check or the name
/// sanity gate in `validate_turn_attachments`. Expected RUNTIME failure:
/// the matching submit below is durably accepted (head advances) instead of
/// returning `invalid_argument`.
#[tokio::test]
async fn file_attachment_utf8_and_name_sanity_enforced_at_acceptance() {
    let (_root, store, hub) = open_hub(None, 8).await;
    let sink = Arc::new(CollectSink::default());
    let connection = hub
        .open_connection(
            capabilities(),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("connection");
    let session_id = SessionId::new("attachment-file-validation");
    attach_control_session(&hub, &store, &connection, &sink, &session_id).await;

    // A verified CAS object that is NOT UTF-8: the client gate is not
    // trusted — the daemon decodes and refuses.
    let ResponseBody::ArtifactPut {
        artifact: binary_ref,
        ..
    } = upload_bytes(
        &connection,
        &sink,
        "put-binary-file",
        &[0xff, 0xfe, 0x00, 0x80],
    )
    .await
    else {
        panic!("binary fixture upload succeeds");
    };
    connection
        .request(
            RequestId::new("submit-non-utf8-file"),
            RequestBody::TurnSubmit {
                command_id: CommandId::new("submit-non-utf8-file"),
                session_id: session_id.clone(),
                worker_generation: store.worker_generation(),
                text: "read this file".into(),
                attachments: vec![AttachmentBlock::File {
                    artifact: binary_ref,
                    name: "blob.bin".into(),
                    lines: 1,
                }],
                mode: DeliveryMode::Queue,
            },
        )
        .await
        .expect("non-UTF-8 submit routes");
    assert!(matches!(
        sink.next().await,
        WireFrame::Response {
            body: ResponseBody::Error { ref code, ref message, .. },
            ..
        } if code == haider_rpc::ERROR_CODE_INVALID_ARGUMENT && message.contains("not UTF-8")
    ));

    // Name sanity: a path-shaped name is refused before any CAS read.
    let ResponseBody::ArtifactPut {
        artifact: text_ref, ..
    } = upload_bytes(&connection, &sink, "put-text-file", b"plain text").await
    else {
        panic!("text fixture upload succeeds");
    };
    for (label, name) in [
        ("submit-path-name", "../etc/passwd".to_owned()),
        ("submit-empty-name", String::new()),
        ("submit-control-name", "notes\u{7}.md".to_owned()),
        ("submit-long-name", "n".repeat(121)),
    ] {
        connection
            .request(
                RequestId::new(label),
                RequestBody::TurnSubmit {
                    command_id: CommandId::new(label),
                    session_id: session_id.clone(),
                    worker_generation: store.worker_generation(),
                    text: "read this file".into(),
                    attachments: vec![AttachmentBlock::File {
                        artifact: text_ref.clone(),
                        name,
                        lines: 1,
                    }],
                    mode: DeliveryMode::Queue,
                },
            )
            .await
            .expect("bad-name submit routes");
        assert!(matches!(
            sink.next().await,
            WireFrame::Response {
                body: ResponseBody::Error { ref code, ref message, .. },
                ..
            } if code == haider_rpc::ERROR_CODE_INVALID_ARGUMENT
                && message.contains("invalid file name")
        ));
    }
    assert_eq!(store.latest_seq(&session_id).await.expect("head"), 1);

    connection.close().await.expect("connection closes");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// MUTATION CHECK: validate only persisted worker_generation and omit the
/// active lease token. Expected failure: the superseded first worker appends
/// successfully in the same daemon generation.
#[tokio::test]
async fn superseded_worker_lease_is_fenced_before_store_append() {
    let (_root, store, hub) = open_hub(None, 8).await;
    let session_id = SessionId::new("lease-fence");
    let generation = store.worker_generation();
    append_one(&hub, &session_id, generation, "seed").await;
    let first = hub
        .acquire_worker_lease(session_id.clone())
        .await
        .expect("first lease");
    let second = hub
        .acquire_worker_lease(session_id.clone())
        .await
        .expect("replacement lease");

    let mut stale = [envelope(&session_id, "stale", generation)];
    let error = StoreHandle::append(&first, &mut stale)
        .await
        .expect_err("stale lease must fail");
    assert_eq!(
        error.code,
        haider_protocol::error::ErrorCode::SingleWriterViolation
    );
    let mut current = [envelope(&session_id, "current", generation)];
    StoreHandle::append(&second, &mut current)
        .await
        .expect("current lease appends");
    assert_eq!(current[0].seq, 2);

    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
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

async fn attach_and_collect_replay(
    connection: &HubConnection,
    sink: &Arc<CollectSink>,
    session_id: &SessionId,
    request_id: &str,
    sealed_replay: bool,
) -> (AttachmentId, Vec<u64>, u64) {
    connection
        .request(
            RequestId::new(request_id),
            RequestBody::SessionAttach {
                session_id: session_id.clone(),
                after_seq: 0,
                mode: AttachMode::View,
                sealed_replay,
            },
        )
        .await
        .expect("attach routes");
    let (attachment_id, response_high_water) = attachment_from(sink.next().await);
    let mut replayed = Vec::new();
    let caught_up_high_water = loop {
        match sink.next().await {
            WireFrame::Event {
                attachment_id: found,
                envelope,
                ..
            } => {
                assert_eq!(found, attachment_id);
                replayed.push(envelope.seq);
            }
            WireFrame::AttachCaughtUp {
                attachment_id: found,
                high_water_seq,
            } => {
                assert_eq!(found, attachment_id);
                break high_water_seq;
            }
            frame => panic!("unexpected replay frame: {frame:?}"),
        }
    };
    assert_eq!(caught_up_high_water, response_high_water);
    (attachment_id, replayed, caught_up_high_water)
}

#[tokio::test]
async fn sealed_replay_skips_only_durable_item_deltas_and_preserves_high_water() {
    let (_root, store, hub) = open_hub(None, 8).await;
    let session_id = SessionId::new("sealed-replay");
    let generation = store.worker_generation();
    append_one(&hub, &session_id, generation, "before-delta").await;
    append_delta(&hub, &session_id, generation, "durable-delta").await;
    append_one(&hub, &session_id, generation, "after-delta").await;

    let unsealed_sink = Arc::new(CollectSink::default());
    let unsealed = hub
        .open_connection(
            capabilities(),
            unsealed_sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("unsealed connection");
    let (_, unsealed_seqs, unsealed_high_water) = attach_and_collect_replay(
        &unsealed,
        &unsealed_sink,
        &session_id,
        "unsealed-attach",
        false,
    )
    .await;

    let sealed_sink = Arc::new(CollectSink::default());
    let sealed = hub
        .open_connection(
            capabilities(),
            sealed_sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("sealed connection");
    let (_, sealed_seqs, sealed_high_water) =
        attach_and_collect_replay(&sealed, &sealed_sink, &session_id, "sealed-attach", true).await;

    assert_eq!(unsealed_seqs, [1, 2, 3]);
    assert_eq!(sealed_seqs, [1, 3]);
    assert_eq!(sealed_high_water, unsealed_high_water);
    assert_eq!(sealed_high_water, 3);

    let live_seq = append_delta(&hub, &session_id, generation, "live-delta").await;
    for sink in [&unsealed_sink, &sealed_sink] {
        assert!(matches!(
            sink.next().await,
            WireFrame::Event { envelope, .. } if envelope.seq == live_seq
        ));
    }

    unsealed.close().await.expect("unsealed connection closes");
    sealed.close().await.expect("sealed connection closes");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
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
        hub.open_connection(
            capabilities(),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
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
                        sealed_replay: false,
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
        .open_connection(
            capabilities(),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("connection");
    connection
        .request(
            RequestId::new("attach"),
            RequestBody::SessionAttach {
                session_id: session_id.clone(),
                after_seq: 0,
                mode: AttachMode::View,
                sealed_replay: false,
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
    let metrics = hub.metrics();
    assert!(metrics.catch_up_overflows >= 1);
    assert!(metrics.reregistrations >= 1);
    assert!(metrics.store_resumes >= 1);

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
        .open_connection(
            capabilities(),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("connection");
    connection
        .request(
            RequestId::new("ahead"),
            RequestBody::SessionAttach {
                session_id,
                after_seq: 9,
                mode: AttachMode::View,
                sealed_replay: false,
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

/// The additive activation-graph read doors use view authority, preserve
/// not-found/argument errors, and expose typed cursor recovery coordinates.
///
/// MUTATION CHECK: deriving the watch head only from graph-event rows makes
/// the ahead error report `head: 0` and the empty page stop at cursor zero;
/// both assertions below must fail because watch cursors live in the sparse
/// session-journal coordinate space.
#[tokio::test]
async fn workflow_graph_read_rpcs_are_authorized_bounded_and_replayable() {
    let (_root, store, hub) = open_hub(None, 8).await;
    let session_id = SessionId::new("workflow-graph-reads");
    append_one(
        &hub,
        &session_id,
        store.worker_generation(),
        "workflow-graph-session",
    )
    .await;

    let denied_sink = Arc::new(CollectSink::default());
    let denied = hub
        .open_connection(
            CapabilitySet::new(),
            denied_sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("denied connection");
    denied
        .request(
            RequestId::new("workflow-state-denied"),
            RequestBody::WorkflowGraphState {
                session_id: session_id.clone(),
                graph_id: None,
            },
        )
        .await
        .expect("state denial routes");
    assert!(matches!(
        denied_sink.next().await,
        WireFrame::Response {
            body: ResponseBody::Error { ref code, .. },
            ..
        } if code == ERROR_CODE_CAPABILITY_DENIED
    ));

    let sink = Arc::new(CollectSink::default());
    let connection = hub
        .open_connection(
            CapabilitySet::from([Capability::View]),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("view connection");
    connection
        .request(
            RequestId::new("workflow-state-empty"),
            RequestBody::WorkflowGraphState {
                session_id: session_id.clone(),
                graph_id: None,
            },
        )
        .await
        .expect("empty state routes");
    assert!(matches!(
        sink.next().await,
        WireFrame::Response {
            body: ResponseBody::WorkflowGraphState { state: None },
            ..
        }
    ));

    connection
        .request(
            RequestId::new("workflow-state-missing"),
            RequestBody::WorkflowGraphState {
                session_id: SessionId::new("missing-workflow-graph-session"),
                graph_id: None,
            },
        )
        .await
        .expect("missing state routes");
    assert!(matches!(
        sink.next().await,
        WireFrame::Response {
            body: ResponseBody::Error { ref code, .. },
            ..
        } if code == haider_rpc::ERROR_CODE_NOT_FOUND
    ));

    connection
        .request(
            RequestId::new("workflow-watch-limit"),
            RequestBody::WorkflowGraphWatch {
                session_id: session_id.clone(),
                after_cursor: 0,
                limit: 0,
            },
        )
        .await
        .expect("invalid watch routes");
    assert!(matches!(
        sink.next().await,
        WireFrame::Response {
            body: ResponseBody::Error { ref code, .. },
            ..
        } if code == haider_rpc::ERROR_CODE_INVALID_ARGUMENT
    ));

    connection
        .request(
            RequestId::new("workflow-watch-ahead"),
            RequestBody::WorkflowGraphWatch {
                session_id: session_id.clone(),
                after_cursor: 2,
                limit: 1,
            },
        )
        .await
        .expect("ahead watch routes");
    assert!(matches!(
        sink.next().await,
        WireFrame::Response {
            body: ResponseBody::Error {
                ref code,
                data: Some(ErrorData::CursorAhead { requested: 2, head: 1 }),
                ..
            },
            ..
        } if code == haider_rpc::ERROR_CODE_CURSOR_AHEAD
    ));

    connection
        .request(
            RequestId::new("workflow-watch-empty"),
            RequestBody::WorkflowGraphWatch {
                session_id,
                after_cursor: 0,
                limit: 1,
            },
        )
        .await
        .expect("empty watch routes");
    assert!(matches!(
        sink.next().await,
        WireFrame::Response {
            body: ResponseBody::WorkflowGraphWatch { page },
            ..
        } if page.requested_after_cursor == 0
            && page.replay_through_cursor == 1
            && page.next_cursor == 1
            && page.events.is_empty()
    ));

    denied.close().await.expect("denied connection closes");
    connection.close().await.expect("view connection closes");
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
        hub.open_connection(
            capabilities(),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("connection"),
    );
    connection
        .request(
            RequestId::new("attach"),
            RequestBody::SessionAttach {
                session_id,
                after_seq: 0,
                mode: AttachMode::View,
                sealed_replay: false,
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
        .open_connection(
            capabilities(),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
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

/// MUTATION CHECK: derive the latest footprint only from the requested read
/// range or stop at the first snapshot. Expected runtime failure: a narrow
/// range returns None/12k instead of the head-fenced latest 18k snapshot.
#[tokio::test]
async fn session_read_exposes_latest_footprint_independent_of_requested_range() {
    let (_root, store, hub) = open_hub(None, 8).await;
    let session_id = SessionId::new("read-latest-footprint");
    let generation = store.worker_generation();
    append_one(&hub, &session_id, generation, "seed").await;
    let mut events = vec![
        footprint_envelope(&session_id, "footprint-old", generation, 12_000),
        envelope(&session_id, "between-footprints", generation),
        footprint_envelope(&session_id, "footprint-new", generation, 18_000),
    ];
    hub.append(&mut events).await.expect("footprints append");

    let sink = Arc::new(CollectSink::default());
    let connection = hub
        .open_connection(
            capabilities(),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("connection");
    connection
        .request(
            RequestId::new("read-latest-footprint"),
            RequestBody::SessionRead {
                session_id,
                range: SeqRange {
                    start_seq: 1,
                    end_seq: 1,
                },
            },
        )
        .await
        .expect("session.read routes");
    let WireFrame::Response {
        body: ResponseBody::SessionRead { result },
        ..
    } = sink.next().await
    else {
        panic!("expected session.read response");
    };
    assert_eq!(result.envelopes.len(), 1);
    assert_eq!(result.head_seq, 4);
    assert_eq!(
        result
            .latest_context_footprint
            .expect("latest footprint")
            .used_tokens,
        18_000
    );

    connection.close().await.expect("connection closes");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// One committed main-timeline user turn. `agent_scoped` marks a prompt
/// steered INTO a subagent (the delegation projection shape), which is not
/// a roster turn.
fn user_turn_envelope(
    session_id: &SessionId,
    event_id: &str,
    worker_generation: u64,
    agent_scoped: bool,
) -> RawEnvelope {
    let mut envelope = envelope(session_id, event_id, worker_generation);
    if agent_scoped {
        envelope.agent_id = Some(AgentId::new("agent-under-test"));
    }
    envelope.payload = serde_json::to_value(EventPayload::UserMessage {
        text: format!("turn {event_id}"),
        attachments: Vec::new(),
        mode: DeliveryMode::Queue,
    })
    .expect("user message serializes");
    envelope
}

fn model_selected_envelope(
    session_id: &SessionId,
    event_id: &str,
    worker_generation: u64,
    provider: &str,
    model: &str,
) -> RawEnvelope {
    let mut envelope = envelope(session_id, event_id, worker_generation);
    envelope.payload = ModelSelected {
        provider: provider.into(),
        model: model.into(),
    }
    .to_payload_value()
    .expect("model selection serializes");
    envelope
}

async fn list_summaries(hub: &SessionHub) -> Vec<SessionSummary> {
    let sink = Arc::new(CollectSink::default());
    let connection = hub
        .open_connection(
            capabilities(),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("connection");
    connection
        .request(
            RequestId::new("roster-list"),
            RequestBody::SessionList {
                cursor: None,
                limit: 10,
            },
        )
        .await
        .expect("session.list routes");
    let WireFrame::Response {
        body: ResponseBody::SessionList { sessions, .. },
        ..
    } = sink.next().await
    else {
        panic!("expected session.list response");
    };
    connection.close().await.expect("connection closes");
    sessions
}

/// WIRE-GAPS item 4: a non-attached roster read carries the workspace that
/// was committed at session creation, independent of the listing process's
/// own cwd.
#[tokio::test]
async fn session_summary_carries_its_committed_workspace_cwd() {
    let (_root, store, hub) = open_hub(None, 8).await;
    let session_id = SessionId::new("summary-workspace");
    create_typed_session(&store, &session_id, "fake").await;
    let summary = list_summaries(&hub)
        .await
        .into_iter()
        .find(|summary| summary.session_id == session_id)
        .expect("created session summary");
    assert_eq!(summary.workspace_cwd.as_deref(), Some("/tmp"));

    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// The promoted provider and nested compatibility copy have one authority:
/// the committed typed metadata row.
///
/// MUTATION CHECK (executed): replace the promoted provider clone in
/// `session_summaries` with the literal `mutated-provider`. Expected RUNTIME
/// failure: the top-level assertion differs from `metadata.provider` while the
/// nested value remains `anthropic`.
#[tokio::test]
async fn session_summary_provider_matches_metadata_authority() {
    let (_root, store, hub) = open_hub(None, 8).await;
    let session_id = SessionId::new("summary-known-provider");
    create_typed_session(&store, &session_id, "anthropic").await;

    let summary = list_summaries(&hub)
        .await
        .into_iter()
        .find(|summary| summary.session_id == session_id)
        .expect("created session summary");
    assert_eq!(summary.provider.as_deref(), Some("anthropic"));
    assert_eq!(
        summary.provider.as_deref(),
        summary
            .metadata
            .as_ref()
            .map(|metadata| metadata.provider.as_str()),
        "the promoted and nested provider must never disagree"
    );
    let wire = serde_json::to_value(&summary).expect("summary serializes");
    assert_eq!(wire["provider"], "anthropic");
    assert_eq!(wire["metadata"]["provider"], "anthropic");

    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// A legacy session has no typed provider authority. Its summary must express
/// that as wire absence, never an empty string or a manufactured default.
///
/// MUTATION CHECK (executed): remove `skip_serializing_if` from the promoted
/// provider's serde attributes. Expected RUNTIME failure: the serialized
/// summary contains `"provider": null` instead of omitting the key.
#[tokio::test]
async fn session_summary_unknown_provider_is_omitted() {
    let (_root, store, hub) = open_hub(None, 8).await;
    let session_id = SessionId::new("summary-unknown-provider");
    append_one(
        &hub,
        &session_id,
        store.worker_generation(),
        "legacy-provider-seed",
    )
    .await;

    let summary = list_summaries(&hub)
        .await
        .into_iter()
        .find(|summary| summary.session_id == session_id)
        .expect("legacy session summary");
    assert_eq!(summary.metadata, None);
    assert_eq!(summary.provider, None);
    let wire = serde_json::to_value(&summary).expect("summary serializes");
    assert!(
        wire.get("provider").is_none(),
        "unknown provider must be absent, not null, empty, or a placeholder: {wire}"
    );

    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// The first watch tick is a full baseline; subsequent ticks carry only
/// changed/new summaries, and a stable roster is silent. v1 intentionally
/// has no removal tombstone.
#[tokio::test]
async fn session_list_watch_pushes_baseline_then_changes_and_skips_quiet_ticks() {
    let (_root, store, hub) = open_hub(None, 8).await;
    let session_id = SessionId::new("roster-watch");
    create_typed_session(&store, &session_id, "fake").await;
    let sink = Arc::new(CollectSink::default());
    let connection = hub
        .open_connection(
            capabilities(),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("connection");

    connection
        .request(
            RequestId::new("roster-watch-start"),
            RequestBody::SessionListWatch {},
        )
        .await
        .expect("session.list_watch routes");
    assert!(matches!(
        sink.next().await,
        WireFrame::Response {
            request_id,
            body: ResponseBody::SessionListWatch { accepted: true },
        } if request_id.as_str() == "roster-watch-start"
    ));

    let WireFrame::SessionRosterDelta { summaries } = sink.next().await else {
        panic!("expected roster baseline");
    };
    let baseline = summaries
        .iter()
        .find(|summary| summary.session_id == session_id)
        .expect("baseline contains watched session");

    append_one(
        &hub,
        &session_id,
        store.worker_generation(),
        "roster-watch-change",
    )
    .await;
    let WireFrame::SessionRosterDelta { summaries } = sink.next().await else {
        panic!("expected changed roster delta");
    };
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].session_id, session_id);
    assert!(summaries[0].head_seq > baseline.head_seq);

    assert!(
        tokio::time::timeout(Duration::from_millis(1_200), sink.next())
            .await
            .is_err(),
        "a quiet tick must not enqueue an empty roster delta"
    );

    connection.close().await.expect("connection closes");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// OWNER BUG: the roster model is head truth, not the model copied into
/// metadata when the session was created. Multiple switches prove sequence
/// ordering; the user turns on both sides make the switches mid-life.
///
/// MUTATION CHECK: remove the `SessionFolder::active_model` result from
/// `session_agent_metrics_truth`, or stop `SessionFolder::push` from applying
/// `model_selected`. Expected RUNTIME failure: `last_model` remains
/// `test-model` (or becomes `None`) instead of `deepseek-reasoner`, while the
/// untouched metadata assertion continues to report `test-model`.
#[tokio::test]
async fn session_list_last_model_tracks_latest_main_timeline_switch() {
    let (_root, store, hub) = open_hub(None, 8).await;
    let session_id = SessionId::new("roster-last-model-switched");
    let generation = store.worker_generation();
    create_typed_session(&store, &session_id, "fake").await;
    let mut events = vec![
        user_turn_envelope(&session_id, "before-switch", generation, false),
        model_selected_envelope(
            &session_id,
            "switch-to-deepseek-chat",
            generation,
            "deepseek",
            "deepseek-chat",
        ),
        user_turn_envelope(&session_id, "between-switches", generation, false),
        model_selected_envelope(
            &session_id,
            "switch-to-deepseek-reasoner",
            generation,
            "deepseek",
            "deepseek-reasoner",
        ),
        user_turn_envelope(&session_id, "after-switch", generation, false),
    ];
    hub.append(&mut events).await.expect("timeline appends");

    let summary = list_summaries(&hub)
        .await
        .into_iter()
        .find(|summary| summary.session_id == session_id)
        .expect("switched session summary");
    assert_eq!(summary.last_model.as_deref(), Some("deepseek-reasoner"));
    assert_eq!(
        summary
            .metadata
            .as_ref()
            .map(|metadata| metadata.model.as_str()),
        Some("test-model"),
        "create-time metadata remains untouched by replay projection"
    );
    assert_eq!(
        store
            .session_metadata(&session_id)
            .await
            .expect("metadata read")
            .expect("typed metadata")
            .model,
        "test-model"
    );

    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// A typed session with no model-selection fact still has an exact roster
/// model: the create-time metadata model seeds the same head fold.
///
/// MUTATION CHECK: initialize `SessionFolder` with an empty model in
/// `session_agent_metrics_truth`. Expected RUNTIME failure: this summary's
/// `last_model` is `None` instead of the metadata fallback `test-model`.
#[tokio::test]
async fn session_list_last_model_falls_back_to_creation_metadata() {
    let (_root, store, hub) = open_hub(None, 8).await;
    let session_id = SessionId::new("roster-last-model-metadata");
    create_typed_session(&store, &session_id, "fake").await;

    let summary = list_summaries(&hub)
        .await
        .into_iter()
        .find(|summary| summary.session_id == session_id)
        .expect("typed session summary");
    assert_eq!(summary.last_model.as_deref(), Some("test-model"));
    assert_eq!(
        summary
            .metadata
            .as_ref()
            .map(|metadata| metadata.model.as_str()),
        Some("test-model")
    );

    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// A legacy session with no typed metadata, user turns, or model-selection
/// facts has no model truth to report; absence must stay honest.
///
/// MUTATION CHECK: coerce the empty `SessionFolder::active_model` seed into a
/// string or synthesize a default in `session_list`. Expected RUNTIME
/// failure: this metadata-less empty summary reports `Some(...)` instead of
/// `None`.
#[tokio::test]
async fn session_list_last_model_is_none_without_metadata_or_switch() {
    let (_root, store, hub) = open_hub(None, 8).await;
    let session_id = SessionId::new("roster-last-model-empty-legacy");
    append_one(
        &hub,
        &session_id,
        store.worker_generation(),
        "legacy-session-seed",
    )
    .await;

    let summary = list_summaries(&hub)
        .await
        .into_iter()
        .find(|summary| summary.session_id == session_id)
        .expect("legacy session summary");
    assert_eq!(summary.metadata, None);
    assert_eq!(summary.turn_count, Some(0));
    assert_eq!(summary.last_model, None);

    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// The owner bug (launcher roster "0 turns · 0 tok" until attach): rosters
/// hydrate from `session.list` summaries, so a session with COMMITTED
/// turns and a durable footprint snapshot must report both from the
/// summary alone — this test never attaches and never observes. The count
/// excludes subagent-scoped prompts, and the footprint is the LATEST
/// durable snapshot with its honesty marker.
///
/// MUTATION CHECK: zero the summary's `turn_count`, count agent-scoped
/// prompts as turns, or drop/first-match the footprint projection.
/// Expected RUNTIME failure: the exact roster numbers below change.
#[tokio::test]
async fn summaries_report_turns_and_tokens_for_unattached_sessions() {
    let (_root, store, hub) = open_hub(None, 8).await;
    let session_id = SessionId::new("roster-truth");
    let generation = store.worker_generation();
    let mut events = vec![
        user_turn_envelope(&session_id, "turn-1", generation, false),
        footprint_envelope(&session_id, "footprint-mid", generation, 12_000),
        user_turn_envelope(&session_id, "turn-2", generation, false),
        user_turn_envelope(&session_id, "child-prompt", generation, true),
        footprint_envelope(&session_id, "footprint-new", generation, 18_000),
    ];
    hub.append(&mut events).await.expect("turns append");

    let sessions = list_summaries(&hub).await;
    assert_eq!(sessions.len(), 1);
    let summary = &sessions[0];
    assert_eq!(summary.session_id, session_id);
    assert_eq!(summary.turn_count, Some(2));
    assert_eq!(summary.footprint_tokens, Some(18_000));
    assert_eq!(
        summary.footprint_truth,
        Some(ContextFootprintTruth::Estimated)
    );

    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// The roster carries a compact replace-by-head snapshot rebuilt from the
/// sealed journal; tool lifecycle duplication does not inflate attempts.
#[tokio::test]
async fn session_list_publishes_compact_agent_metrics_at_committed_head() {
    let (_root, store, hub) = open_hub(None, 8).await;
    let session_id = SessionId::new("roster-agent-metrics");
    let generation = store.worker_generation();
    let run_id = RunId::new("metrics-run");
    let tool = |status| haider_protocol::item::TurnItem::ToolCall {
        call_id: "tool-call".into(),
        name: "fs_read".into(),
        args: serde_json::json!({}),
        status,
    };
    let mut started = envelope(&session_id, "metrics-started", generation);
    started.run_id = Some(run_id.clone());
    started.committed_at_ms = 100;
    started.payload =
        serde_json::to_value(EventPayload::RunState(RunState::Thinking)).expect("run state");
    let mut tool_started = envelope(&session_id, "metrics-tool-started", generation);
    tool_started.run_id = Some(run_id.clone());
    tool_started.committed_at_ms = 200;
    tool_started.payload = serde_json::to_value(EventPayload::Item(ItemEvent::Started {
        item_id: ItemId::new("tool-item"),
        item: tool(haider_protocol::item::ToolStatus::InProgress),
    }))
    .expect("tool started");
    let mut tool_completed = envelope(&session_id, "metrics-tool-completed", generation);
    tool_completed.run_id = Some(run_id.clone());
    tool_completed.committed_at_ms = 300;
    tool_completed.payload = serde_json::to_value(EventPayload::Item(ItemEvent::Completed {
        item_id: ItemId::new("tool-item"),
        item: tool(haider_protocol::item::ToolStatus::Completed),
    }))
    .expect("tool completed");
    let mut done = envelope(&session_id, "metrics-done", generation);
    done.run_id = Some(run_id);
    done.committed_at_ms = 400;
    done.payload =
        serde_json::to_value(EventPayload::RunState(RunState::Done)).expect("done state");
    hub.append(&mut [started, tool_started, tool_completed, done])
        .await
        .expect("metrics journal");

    let summary = list_summaries(&hub)
        .await
        .into_iter()
        .find(|summary| summary.session_id == session_id)
        .expect("metrics summary");
    let snapshot = summary.agent_metrics.expect("compact metrics");
    assert_eq!(snapshot.head_seq, summary.head_seq);
    assert!(snapshot.started_at_ms > 0);
    assert!(
        snapshot
            .terminal_at_ms
            .is_some_and(|terminal| terminal >= snapshot.started_at_ms)
    );
    assert!(!snapshot.live);
    assert_eq!(snapshot.tool_attempts, 1);
    assert!(snapshot.usage.is_none(), "no usage fact stays unknown");

    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// Promoted cache rates and the retained nested compatibility copy are one
/// fact: both are read from the same folded usage snapshot at the sealed head.
///
/// MUTATION `PROMOTED_REREAD_DIVERGES` (executed, observed red): replace the
/// promoted re-read clone in `session_summaries` with `Some(0)`. Expected
/// RUNTIME failure: the exact 9_058 assertion and nested-equality assertion.
#[tokio::test]
async fn session_summary_promotes_measured_cache_rates_from_nested_authority() {
    let (_root, store, hub) = open_hub(None, 8).await;
    let session_id = SessionId::new("roster-promoted-cache-rate");
    let generation = store.worker_generation();
    let mut events = vec![
        cache_rate_usage_envelope(
            &session_id,
            "cache-rate-first",
            generation,
            "cache-rate-run-first",
            4_098,
            0,
        ),
        cache_rate_usage_envelope(
            &session_id,
            "cache-rate-reread",
            generation,
            "cache-rate-run-reread",
            4_098,
            3_712,
        ),
    ];
    hub.append(&mut events).await.expect("cache usage commits");

    let summary = list_summaries(&hub)
        .await
        .into_iter()
        .find(|summary| summary.session_id == session_id)
        .expect("cache-rate summary");
    let usage = summary
        .agent_metrics
        .as_ref()
        .and_then(|metrics| metrics.usage.as_ref())
        .expect("nested usage authority");
    assert_eq!(summary.cache_reread_hit_basis_points, Some(9_058));
    assert_eq!(
        summary.cache_lifetime_hit_basis_points,
        usage.cache_hit_basis_points
    );
    assert_eq!(
        summary.cache_reread_hit_basis_points, usage.cache_reread_hit_basis_points,
        "promoted and nested cache health must never disagree"
    );

    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// One first request has complete lifetime telemetry but no preceding prefix
/// that could be re-read. Its missing re-read rate stays absent on the wire;
/// it must never become a numeric zero or placeholder.
///
/// MUTATION `ABSENT_REREAD_BECOMES_ZERO` (executed, observed red): default the
/// promoted clone with `unwrap_or_default()`. Expected RUNTIME failure: the
/// field is `Some(0)` and serializes as `"cache_reread_hit_basis_points":0`.
#[tokio::test]
async fn session_summary_omits_unmeasured_reread_rate_instead_of_zero() {
    let (_root, store, hub) = open_hub(None, 8).await;
    let session_id = SessionId::new("roster-no-cacheable-prefix");
    let generation = store.worker_generation();
    let mut events = vec![cache_rate_usage_envelope(
        &session_id,
        "cache-rate-first-only",
        generation,
        "cache-rate-run-only",
        4_098,
        0,
    )];
    hub.append(&mut events).await.expect("first usage commits");

    let summary = list_summaries(&hub)
        .await
        .into_iter()
        .find(|summary| summary.session_id == session_id)
        .expect("first-turn summary");
    assert_eq!(summary.cache_lifetime_hit_basis_points, Some(0));
    assert_eq!(summary.cache_reread_hit_basis_points, None);
    let wire = serde_json::to_value(&summary).expect("summary serializes");
    assert_eq!(wire["cache_lifetime_hit_basis_points"], 0);
    assert!(
        wire.get("cache_reread_hit_basis_points").is_none(),
        "no denominator must omit the field, not publish zero: {wire}"
    );

    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// Zero-honesty law: zero turns/tokens appear EXCLUSIVELY for truly empty
/// sessions (no committed user turn, no durable snapshot — zero is then
/// exact truth). A session WITH committed turns but no snapshot reports
/// UNKNOWN tokens (`None`), never zero.
///
/// MUTATION CHECK: report `Some(0)` tokens whenever no snapshot exists, or
/// report `None` for the truly empty session. Expected RUNTIME failure:
/// one of the two rows below flips.
#[tokio::test]
async fn zero_is_only_reported_for_truly_empty_sessions() {
    let (_root, store, hub) = open_hub(None, 8).await;
    let generation = store.worker_generation();
    let empty = SessionId::new("roster-empty");
    append_one(&hub, &empty, generation, "non-turn-seed").await;
    let with_turns = SessionId::new("roster-turns-without-footprint");
    let mut events = vec![user_turn_envelope(&with_turns, "turn-1", generation, false)];
    hub.append(&mut events).await.expect("turn appends");

    let sessions = list_summaries(&hub).await;
    let by_id = |id: &SessionId| {
        sessions
            .iter()
            .find(|summary| &summary.session_id == id)
            .expect("listed summary")
    };
    let empty_summary = by_id(&empty);
    assert_eq!(empty_summary.turn_count, Some(0));
    assert_eq!(empty_summary.footprint_tokens, Some(0));
    assert_eq!(
        empty_summary.footprint_truth,
        Some(ContextFootprintTruth::Exact)
    );
    let with_turns_summary = by_id(&with_turns);
    assert_eq!(with_turns_summary.turn_count, Some(1));
    assert_eq!(
        with_turns_summary.footprint_tokens, None,
        "unknown tokens must never be rendered as zero"
    );
    assert_eq!(with_turns_summary.footprint_truth, None);

    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// MUTATION CHECK: collapse both parked run states to one generic blocked
/// value, or serialize whole menu/event payloads into the digest. Expected
/// RUNTIME failure: the simultaneous fixtures cease to report distinct
/// `parked_permission`/`parked_input`, a newer queued branch displaces the
/// executing branch, or a literal vault/OAuth sentinel appears in JSON.
#[tokio::test]
async fn session_observe_distinguishes_parked_states_and_never_leaks_secret_material() {
    const VAULT_SENTINEL: &str = "sk-vault-observe-sentinel-7a4e";
    const OAUTH_SENTINEL: &str = "oauth-refresh-observe-sentinel-4c91";

    let (_root, store, hub) = open_hub(None, 8).await;
    let generation = store.worker_generation();
    let permission_session = SessionId::new("observe-permission");
    let input_session = SessionId::new("observe-input");
    let active_session = SessionId::new("observe-active-versus-queued");
    let recovery_session = SessionId::new("observe-recovery");
    append_one(&hub, &permission_session, generation, "permission-seed").await;
    append_one(&hub, &input_session, generation, "input-seed").await;
    append_one(&hub, &active_session, generation, "active-seed").await;
    append_one(&hub, &recovery_session, generation, "recovery-seed").await;

    let permission_menu_id = MenuId::new("permission-menu");
    let mut permission_menu = menu_opening(&permission_session, &permission_menu_id, generation);
    permission_menu.event_id = EventId::new("permission-menu-opened");
    permission_menu.run_id = Some(RunId::new("permission-run"));
    permission_menu.payload = serde_json::to_value(EventPayload::MenuOpened(Menu {
        id: permission_menu_id.clone(),
        kind: MenuKind::Permission {
            effect_summary: "write src/lib.rs".into(),
        },
        title: "Allow write?".into(),
        // v0.0.937 policy: permission bodies are broker-authored display
        // copy by construction and are EXPOSED on the digest so any surface
        // can render the card; only Secret menus may carry vault material.
        body: vec!["write src/lib.rs".into(), "Effect class: Write".into()],
        options: vec![MenuOption {
            key: "approve_once".into(),
            label: "Approve once".into(),
            detail: None,
            decision: Some(DecisionKind::AllowOnce),
        }],
        blocking: true,
        scope: MenuScope::Session,
        origin: "observe-test".into(),
        ttl_ms: None,
        timeout_option: None,
    }))
    .expect("permission menu serializes");
    let mut permission_state = envelope(&permission_session, "permission-state", generation);
    permission_state.run_id = Some(RunId::new("permission-run"));
    permission_state.payload =
        serde_json::to_value(EventPayload::RunState(RunState::PermissionRequired {
            menu: permission_menu_id,
        }))
        .expect("permission state serializes");

    let input_menu_id = MenuId::new("input-menu");
    let mut input_menu = menu_opening(&input_session, &input_menu_id, generation);
    input_menu.event_id = EventId::new("input-menu-opened");
    input_menu.run_id = Some(RunId::new("input-run"));
    input_menu.payload = serde_json::to_value(EventPayload::MenuOpened(Menu {
        id: input_menu_id.clone(),
        kind: MenuKind::Secret,
        title: "Credential required".into(),
        body: vec![OAUTH_SENTINEL.into()],
        options: vec![MenuOption {
            key: OAUTH_SENTINEL.into(),
            label: OAUTH_SENTINEL.into(),
            detail: Some(OAUTH_SENTINEL.into()),
            decision: None,
        }],
        blocking: true,
        scope: MenuScope::Session,
        origin: "oauth".into(),
        ttl_ms: None,
        timeout_option: None,
    }))
    .expect("input menu serializes");
    let mut input_state = envelope(&input_session, "input-state", generation);
    input_state.run_id = Some(RunId::new("input-run"));
    input_state.payload = serde_json::to_value(EventPayload::RunState(RunState::InputRequired {
        menu: input_menu_id,
    }))
    .expect("input state serializes");
    let mut opaque_oauth = envelope(&input_session, "opaque-oauth", generation);
    opaque_oauth.payload = serde_json::json!({
        "type": "future_oauth_token_event",
        "access_token": OAUTH_SENTINEL,
        "refresh_token": VAULT_SENTINEL,
    });

    let branch_fact = |event_id: &str, branch_id: &str, name: &str, created_seq: u64| {
        let mut fact = envelope(&active_session, event_id, generation);
        fact.payload = BranchCreated {
            branch: BranchDescriptor {
                branch_id: BranchId::new(branch_id),
                name: name.into(),
                source_branch_id: None,
                fork_node_id: NodeId::new("active-main-node"),
                fork_seq: 1,
                created_seq,
                created_at_ms: 1_800_000_000_000 + created_seq,
                head_node_id: NodeId::new("active-main-node"),
                head_seq: 1,
            },
        }
        .to_payload_value()
        .expect("branch fact serializes");
        fact
    };
    let executing_branch = branch_fact(
        "executing-branch-created",
        "branch-executing",
        "executing",
        2,
    );
    let queued_branch = branch_fact("queued-branch-created", "branch-queued", "queued", 3);
    let mut active_state = envelope(&active_session, "active-state", generation);
    active_state.branch_id = Some(BranchId::new("branch-executing"));
    active_state.run_id = Some(RunId::new("run-executing"));
    active_state.payload = serde_json::to_value(EventPayload::RunState(RunState::Thinking))
        .expect("active state serializes");
    let mut queued_state = envelope(&active_session, "queued-state", generation);
    queued_state.branch_id = Some(BranchId::new("branch-queued"));
    queued_state.run_id = Some(RunId::new("run-queued"));
    queued_state.payload = serde_json::to_value(EventPayload::RunState(RunState::Queued))
        .expect("queued state serializes");

    hub.append(&mut [permission_menu, permission_state])
        .await
        .expect("permission fixture commits");
    hub.append(&mut [input_menu, input_state, opaque_oauth])
        .await
        .expect("input fixture commits");
    hub.append(&mut [executing_branch, queued_branch, active_state, queued_state])
        .await
        .expect("active/queued fixture commits");

    // The daemon-authored crash-window card: its body (probe evidence) and
    // options (Probe/Retry/…) DO survive the digest so the recover door can
    // render them — the positive half of the effect_recovery_v1 boundary.
    let recovery_menu_id = MenuId::new("recovery-menu");
    let mut recovery_menu = menu_opening(&recovery_session, &recovery_menu_id, generation);
    recovery_menu.event_id = EventId::new("recovery-menu-opened");
    recovery_menu.run_id = Some(RunId::new("recovery-run"));
    recovery_menu.payload = serde_json::to_value(EventPayload::MenuOpened(Menu {
        id: recovery_menu_id.clone(),
        kind: MenuKind::Recovery {
            effect: EffectId::new("effect-observe-recovery"),
            presentation: Some(ErrorPresentation::new(
                "effect-outcome-unknown",
                "Effect outcome unknown",
                "reconcile before continuing",
                ErrorScope::Turn,
                [ErrorAction::Retry],
            )),
            option_actions: vec![EffectRecoveryAction::Probe],
        },
        title: "Effect outcome unknown".into(),
        body: vec!["probe: process dead \u{b7} no result committed".into()],
        options: vec![MenuOption {
            key: "probe".into(),
            label: "Probe".into(),
            detail: Some("Re-check whether the effect completed.".into()),
            decision: None,
        }],
        blocking: true,
        scope: MenuScope::Session,
        origin: "recovery-test".into(),
        ttl_ms: None,
        timeout_option: None,
    }))
    .expect("recovery menu serializes");
    let mut recovery_state = envelope(&recovery_session, "recovery-state", generation);
    recovery_state.run_id = Some(RunId::new("recovery-run"));
    recovery_state.payload =
        serde_json::to_value(EventPayload::RunState(RunState::EffectOutcomeUnknown))
            .expect("recovery state serializes");
    hub.append(&mut [recovery_menu, recovery_state])
        .await
        .expect("recovery fixture commits");

    let sink = Arc::new(CollectSink::default());
    let connection = hub
        .open_connection(
            capabilities(),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("connection");
    for (request, session_id) in [
        ("observe-permission", permission_session),
        ("observe-input", input_session),
        ("observe-active", active_session),
        ("observe-recovery", recovery_session),
    ] {
        connection
            .request(
                RequestId::new(request),
                RequestBody::SessionObserve {
                    session_id,
                    last_event_limit: 20,
                    metadata_only: false,
                },
            )
            .await
            .expect("session.observe routes");
    }
    let mut digests = Vec::new();
    for _ in 0..4 {
        let WireFrame::Response {
            body: ResponseBody::SessionObserve { digest },
            ..
        } = sink.next().await
        else {
            panic!("expected session.observe response");
        };
        digests.push(digest);
    }
    let permission = digests
        .iter()
        .find(|digest| digest.session_id.as_str() == "observe-permission")
        .expect("permission digest");
    let input = digests
        .iter()
        .find(|digest| digest.session_id.as_str() == "observe-input")
        .expect("input digest");
    let active = digests
        .iter()
        .find(|digest| digest.session_id.as_str() == "observe-active-versus-queued")
        .expect("active digest");
    assert_eq!(permission.run_state, ObserveRunStateWire::ParkedPermission);
    assert_eq!(input.run_state, ObserveRunStateWire::ParkedInput);
    assert_eq!(
        permission.pending_menus[0]
            .permission_description
            .as_deref(),
        Some("write src/lib.rs")
    );
    assert_eq!(input.pending_menus[0].kind, "secret");
    assert!(
        input
            .last_event_kinds
            .contains(&"future_oauth_token_event".to_owned())
    );
    assert_eq!(active.run_state, ObserveRunStateWire::Running);
    assert_eq!(
        active.active_branch_id.as_ref().map(BranchId::as_str),
        Some("branch-executing")
    );
    assert_eq!(
        active
            .branches
            .iter()
            .map(|branch| branch.name.as_str())
            .collect::<Vec<_>>(),
        ["executing", "queued"]
    );
    assert!(active.branches.iter().all(|branch| {
        branch.created_seq <= active.head_seq && branch.head_seq <= active.head_seq
    }));
    let json = serde_json::to_string(&digests).expect("digests serialize");
    assert!(!json.contains(VAULT_SENTINEL));
    assert!(!json.contains(OAUTH_SENTINEL));
    // effect_recovery_v1 boundary: non-recovery menus expose title +
    // permission_description ONLY; their durable body/options (which can
    // carry vaulted credentials) are stripped from the observe digest.
    // v0.0.937 unified input contract: the permission card IS exposed —
    // body (display copy), options, and the typed decision rider — so the
    // ADE can render and answer it without a terminal.
    assert_eq!(
        permission.pending_menus[0].body,
        vec![
            "write src/lib.rs".to_owned(),
            "Effect class: Write".to_owned()
        ],
    );
    assert_eq!(
        permission.pending_menus[0].options[0].decision.as_deref(),
        Some("allow_once"),
        "the typed decision rides the exposed option"
    );
    let permission_card = permission
        .needs_input
        .as_ref()
        .expect("permission park publishes needs_input");
    assert_eq!(
        permission_card.kind,
        haider_rpc::NeedsInputKindWire::Permission
    );
    assert_eq!(permission_card.menu_id, permission.pending_menus[0].menu_id);
    assert_eq!(
        permission_card.request_seq,
        permission.pending_menus[0].request_seq
    );
    assert!(!permission_card.secret_answer);
    assert!(
        input.pending_menus[0].body.is_empty() && input.pending_menus[0].options.is_empty(),
        "a secret menu's body/options never reach the observe digest"
    );
    let secret_card = input
        .needs_input
        .as_ref()
        .expect("secret park still publishes a typed badge");
    assert_eq!(secret_card.kind, haider_rpc::NeedsInputKindWire::Secret);
    assert!(
        secret_card.secret_answer,
        "secret answers must travel as references"
    );
    assert!(
        secret_card.safe_body.is_empty() && secret_card.options.is_empty(),
        "the secret card is badge-only — never material"
    );
    let recovery = digests
        .iter()
        .find(|digest| digest.session_id.as_str() == "observe-recovery")
        .expect("recovery digest");
    assert_eq!(recovery.run_state, ObserveRunStateWire::EffectUnknown);
    // v0.0.935 shipped defect lock: the crash-window park is
    // `MenuKind::Recovery` — wire kind "recovery", the string the headless
    // recover door filters on — NOT the provider/account "error_recovery"
    // card this fixture used to (wrongly) model it with.
    let recovery_menu = recovery
        .pending_menus
        .iter()
        .find(|menu| menu.kind == "recovery")
        .expect("crash-window recovery menu present under wire kind 'recovery'");
    assert!(
        !recovery_menu.body.is_empty() && !recovery_menu.options.is_empty(),
        "the crash-window card's probe evidence and options DO reach the digest"
    );
    assert_eq!(recovery_menu.options[0].key, "probe");

    connection.close().await.expect("connection closes");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// MUTATION CHECK (v0.0.935 #7/#13): let the metadata-only fast path skip
/// the user-message scan (empty/foreign title), diverge an authoritative
/// field from the full replay, or stamp the digest's roster truth from a
/// different source than session.list. Expected RUNTIME failure: the two
/// observe modes disagree on metadata/title/head, or turn_count and
/// agent_metrics disagree with the session listing's.
#[tokio::test]
async fn metadata_only_observe_shares_authoritative_fields_and_roster_truth() {
    let (_root, store, hub) = open_hub(None, 8).await;
    let generation = store.worker_generation();
    let session_id = SessionId::new("observe-metadata-only");
    create_typed_session(&store, &session_id, "openai").await;
    let mut message = envelope(&session_id, "meta-user-message", generation);
    message.run_id = Some(RunId::new("meta-run"));
    message.payload = serde_json::to_value(EventPayload::UserMessage {
        text: "compare the two observe doors".into(),
        attachments: Vec::new(),
        mode: DeliveryMode::Queue,
    })
    .expect("user message serializes");
    hub.append(&mut [message]).await.expect("message commits");

    let sink = Arc::new(CollectSink::default());
    let connection = hub
        .open_connection(
            capabilities(),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("connection");
    let mut digests = Vec::new();
    for (request, metadata_only) in [("observe-full", false), ("observe-fast", true)] {
        connection
            .request(
                RequestId::new(request),
                RequestBody::SessionObserve {
                    session_id: session_id.clone(),
                    last_event_limit: 0,
                    metadata_only,
                },
            )
            .await
            .expect("session.observe routes");
        let WireFrame::Response {
            body: ResponseBody::SessionObserve { digest },
            ..
        } = sink.next().await
        else {
            panic!("expected session.observe response");
        };
        digests.push(digest);
    }
    let (full, fast) = (&digests[0], &digests[1]);
    // Authoritative fields are identical across the two modes — the export
    // door reads only these, so its bytes cannot change.
    assert_eq!(fast.session_id, full.session_id);
    assert_eq!(fast.head_seq, full.head_seq);
    assert_eq!(fast.worker_generation, full.worker_generation);
    assert_eq!(fast.metadata, full.metadata);
    assert_eq!(fast.title, full.title);
    assert_eq!(full.title, "Compare the two observe doors");
    // Roster truth rides the FULL digest only, from the same truth functions
    // the session listing uses.
    assert!(fast.turn_count.is_none());
    assert!(fast.agent_metrics.is_none());
    connection
        .request(
            RequestId::new("observe-parity-list"),
            RequestBody::SessionList {
                cursor: None,
                limit: 16,
            },
        )
        .await
        .expect("session.list routes");
    let WireFrame::Response {
        body: ResponseBody::SessionList { sessions, .. },
        ..
    } = sink.next().await
    else {
        panic!("expected session.list response");
    };
    let summary = sessions
        .iter()
        .find(|summary| summary.session_id == session_id)
        .expect("listed summary");
    assert_eq!(full.turn_count, summary.turn_count);
    assert!(full.turn_count.is_some());
    assert_eq!(full.agent_metrics, summary.agent_metrics);

    connection.close().await.expect("connection closes");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// MUTATION CHECK (v0.0.935 #9): deliver a surface delta on every watch tick
/// regardless of the generation compare, or break the compare-first snapshot
/// so a moved generation returns nothing. Expected RUNTIME failure: the idle
/// window below accumulates extra `SessionSurfaceDelta` frames, or the
/// published change never reaches the watcher.
#[tokio::test]
async fn surface_watch_delivers_on_change_and_stays_silent_when_idle() {
    let (_root, store, hub) = open_hub(None, 8).await;
    let generation = store.worker_generation();
    let session_id = SessionId::new("surface-watch-idle");
    append_one(&hub, &session_id, generation, "surface-seed").await;

    let sink = Arc::new(CollectSink::default());
    let connection = hub
        .open_connection(
            capabilities(),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("connection");
    connection
        .request(
            RequestId::new("surface-watch"),
            RequestBody::SessionSurfaceWatch {
                session_id: session_id.clone(),
            },
        )
        .await
        .expect("surface watch routes");
    let WireFrame::Response {
        body: ResponseBody::SessionSurfaceWatching { .. },
        ..
    } = sink.next().await
    else {
        panic!("expected surface watching response");
    };
    connection
        .request(
            RequestId::new("surface-publish"),
            RequestBody::SessionSurfacePublish {
                session_id: session_id.clone(),
                input: Some(SurfaceInputPublishWire {
                    text: "draft in flight".into(),
                    attachments: Vec::new(),
                    revision: 1,
                }),
                status: None,
            },
        )
        .await
        .expect("surface publish routes");
    let WireFrame::Response {
        body: ResponseBody::SessionSurfacePublished { .. },
        ..
    } = sink.next().await
    else {
        panic!("expected surface published response");
    };
    let delta = sink.next().await;
    let WireFrame::SessionSurfaceDelta {
        session_id: delta_session,
        input,
        ..
    } = delta
    else {
        panic!("expected surface delta, got {delta:?}");
    };
    assert_eq!(delta_session, session_id);
    assert_eq!(
        input.expect("published input surface").text,
        "draft in flight"
    );

    // Idle ticks (50ms period) compare generations under the lock and must
    // deliver nothing while the surface is unchanged.
    tokio::time::sleep(Duration::from_millis(400)).await;
    let extra = sink.snapshot();
    assert!(
        extra.is_empty(),
        "idle ticks must stay silent, got {extra:?}"
    );

    connection.close().await.expect("connection closes");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// DESCENDANT STREAM LAW: the baseline seals one head per child, replay is
/// strictly after that child's supplied cursor, reconnect neither duplicates
/// nor skips the detached suffix, and live terminal/parent-result deltas keep
/// the child session and lineage agent identities distinct.
///
/// MUTATION CHECK: use one tree-global cursor, omit either outer identity,
/// replay inclusively, or derive anchors from delegation coordinates.
/// Expected RUNTIME failure: the exact sequence/identity/anchor assertions
/// below turn red.
#[tokio::test]
async fn descendant_stream_reconnects_per_child_without_gaps_or_duplicates() {
    let (_root, store, hub) = open_hub(None, 8).await;
    let generation = store.worker_generation();
    let root = SessionId::new("descendant-stream-root");
    let child_session = SessionId::new("descendant-stream-child-session");
    create_typed_session(&store, &root, "openai").await;
    create_typed_session(&store, &child_session, "openai").await;
    let record = fleet_delegation(&root, &root, &child_session, "stream-child", None, 1);
    store
        .create_delegation(record.clone())
        .await
        .expect("stream relation");

    let mut spawn = envelope(&root, "descendant-parent-spawn", generation);
    spawn.run_id = Some(record.parent_run_id.clone());
    spawn.payload = serde_json::to_value(EventPayload::AgentSpawned(record.manifest.clone()))
        .expect("spawn fact");
    let mut spawn_item = envelope(&root, "descendant-parent-spawn-item", generation);
    spawn_item.run_id = Some(record.parent_run_id.clone());
    spawn_item.payload = serde_json::to_value(EventPayload::Item(ItemEvent::Completed {
        item_id: record.tool_item_id.clone(),
        item: TurnItem::ChildSpawn {
            agent: record.agent_id.clone(),
        },
    }))
    .expect("spawn item");
    let committed = hub
        .append(&mut [spawn, spawn_item])
        .await
        .expect("parent anchors commit");
    let spawn_seq = committed.first_seq;
    let spawn_item_seq = committed.last_seq;

    let mut thinking = envelope(&child_session, "descendant-child-thinking", generation);
    thinking.run_id = Some(record.child_run_id.clone());
    thinking.agent_id = Some(record.agent_id.clone());
    thinking.payload =
        serde_json::to_value(EventPayload::RunState(RunState::Thinking)).expect("thinking state");
    hub.append(&mut [thinking])
        .await
        .expect("child thinking commits");
    assert_eq!(
        store.latest_seq(&child_session).await.expect("child head"),
        2
    );

    let sink = Arc::new(CollectSink::default());
    let connection = hub
        .open_connection(
            CapabilitySet::from([Capability::View]),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("descendant stream connection");
    connection
        .request(
            RequestId::new("descendant-first-attach"),
            RequestBody::SessionDescendantsAttach {
                session_id: root.clone(),
                cursors: vec![haider_rpc::DescendantReplayCursorWire {
                    session_id: child_session.clone(),
                    agent_id: record.agent_id.clone(),
                    after_seq: 1,
                }],
                max_children: 4,
            },
        )
        .await
        .expect("first descendant attach routes");
    let WireFrame::Response {
        body:
            ResponseBody::SessionDescendantsAttach {
                attachment_id: first_attachment,
                baseline,
            },
        ..
    } = sink.next().await
    else {
        panic!("expected descendant baseline");
    };
    assert_eq!(baseline.roots.len(), 1);
    let baseline_child = &baseline.roots[0];
    assert_eq!(baseline_child.session_id, child_session);
    assert_eq!(baseline_child.agent_id, record.agent_id);
    assert_ne!(
        baseline_child.session_id.as_str(),
        baseline_child.agent_id.as_str(),
        "session identity is not lineage identity"
    );
    assert_eq!(baseline_child.child_run_id, record.child_run_id);
    assert_eq!(
        baseline_child.callsign.as_deref(),
        Some("CALL-stream-child")
    );
    assert_eq!(baseline_child.model.as_deref(), Some("gpt-5.2"));
    assert_eq!(baseline_child.provider.as_deref(), Some("anthropic"));
    assert_eq!(baseline_child.requested_after_seq, 1);
    assert_eq!(baseline_child.replay_through_seq, 2);
    assert_eq!(baseline_child.parent_anchors.spawn_seq, Some(spawn_seq));
    assert_eq!(
        baseline_child.parent_anchors.spawn_item_seq,
        Some(spawn_item_seq)
    );
    assert_eq!(baseline_child.parent_anchors.result_seq, None);

    let WireFrame::SessionDescendantStream {
        attachment_id,
        event:
            haider_rpc::SessionDescendantStreamEventWire::Envelope {
                session_id,
                agent_id,
                envelope: replayed_envelope,
            },
    } = sink.next().await
    else {
        panic!("expected first child replay envelope");
    };
    assert_eq!(attachment_id, first_attachment);
    assert_eq!(session_id, child_session);
    assert_eq!(agent_id, record.agent_id);
    assert_eq!(replayed_envelope.session_id, child_session);
    assert_eq!(replayed_envelope.seq, 2);
    assert!(matches!(
        sink.next().await,
        WireFrame::SessionDescendantStream {
            attachment_id,
            event: haider_rpc::SessionDescendantStreamEventWire::ChildCaughtUp {
                high_water_seq: 2,
                ..
            },
        } if attachment_id == first_attachment
    ));

    connection
        .request(
            RequestId::new("descendant-first-detach"),
            RequestBody::SessionDetach {
                attachment_id: first_attachment,
            },
        )
        .await
        .expect("first descendant detach routes");
    assert!(matches!(
        sink.next().await,
        WireFrame::Response {
            body: ResponseBody::SessionDetach { .. },
            ..
        }
    ));

    let child_suffix_seq = append_one(
        &hub,
        &child_session,
        generation,
        "descendant-detached-suffix",
    )
    .await;
    assert_eq!(child_suffix_seq, 3);
    connection
        .request(
            RequestId::new("descendant-reconnect"),
            RequestBody::SessionDescendantsAttach {
                session_id: root.clone(),
                cursors: vec![haider_rpc::DescendantReplayCursorWire {
                    session_id: child_session.clone(),
                    agent_id: record.agent_id.clone(),
                    after_seq: 2,
                }],
                max_children: 4,
            },
        )
        .await
        .expect("descendant reconnect routes");
    let WireFrame::Response {
        body:
            ResponseBody::SessionDescendantsAttach {
                attachment_id: reconnect_attachment,
                baseline,
            },
        ..
    } = sink.next().await
    else {
        panic!("expected reconnect baseline");
    };
    assert_eq!(baseline.roots[0].requested_after_seq, 2);
    assert_eq!(baseline.roots[0].replay_through_seq, 3);
    let WireFrame::SessionDescendantStream {
        event:
            haider_rpc::SessionDescendantStreamEventWire::Envelope {
                envelope: replayed_envelope,
                ..
            },
        ..
    } = sink.next().await
    else {
        panic!("expected detached suffix replay");
    };
    assert_eq!(
        replayed_envelope.seq, 3,
        "resume is strict and duplicate-free"
    );
    assert!(matches!(
        sink.next().await,
        WireFrame::SessionDescendantStream {
            event: haider_rpc::SessionDescendantStreamEventWire::ChildCaughtUp {
                high_water_seq: 3,
                ..
            },
            ..
        }
    ));

    let mut done = envelope(&child_session, "descendant-child-done", generation);
    done.run_id = Some(record.child_run_id.clone());
    done.agent_id = Some(record.agent_id.clone());
    done.payload =
        serde_json::to_value(EventPayload::RunState(RunState::Done)).expect("done state");
    hub.append(&mut [done])
        .await
        .expect("terminal child commits");
    let mut saw_live_seq = false;
    let mut saw_terminal = false;
    while !saw_live_seq || !saw_terminal {
        match sink.next().await {
            WireFrame::SessionDescendantStream {
                attachment_id,
                event:
                    haider_rpc::SessionDescendantStreamEventWire::Envelope {
                        session_id,
                        agent_id,
                        envelope,
                    },
            } if attachment_id == reconnect_attachment && envelope.seq == 4 => {
                assert_eq!(session_id, child_session);
                assert_eq!(agent_id, record.agent_id);
                saw_live_seq = true;
            }
            WireFrame::SessionDescendantStream {
                attachment_id,
                event:
                    haider_rpc::SessionDescendantStreamEventWire::Delta {
                        change: haider_rpc::DescendantChangeKindWire::Terminated,
                        child,
                    },
            } if attachment_id == reconnect_attachment => {
                assert_eq!(child.session_id, child_session);
                assert_eq!(child.agent_id, record.agent_id);
                assert_eq!(child.state, FleetAgentStateWire::Done);
                saw_terminal = true;
            }
            WireFrame::SessionDescendantStream { .. } => {}
            frame => panic!("unexpected descendant live frame: {frame:?}"),
        }
    }

    let report = ChildReport {
        agent: record.agent_id.clone(),
        summary: "complete".into(),
        verified: ReportVerification::Unverified,
        workspace_revision: None,
    };
    let mut report_fact = envelope(&root, "descendant-parent-result", generation);
    report_fact.run_id = Some(record.parent_run_id.clone());
    report_fact.payload =
        serde_json::to_value(EventPayload::AgentReport(report.clone())).expect("report fact");
    let mut result_item = envelope(&root, "descendant-parent-result-item", generation);
    result_item.run_id = Some(record.parent_run_id.clone());
    result_item.payload = serde_json::to_value(EventPayload::Item(ItemEvent::Completed {
        item_id: ItemId::new("descendant-result-item"),
        item: TurnItem::ChildResult { report },
    }))
    .expect("result item");
    let committed = hub
        .append(&mut [report_fact, result_item])
        .await
        .expect("parent result anchors commit");
    loop {
        if let WireFrame::SessionDescendantStream {
            event:
                haider_rpc::SessionDescendantStreamEventWire::Delta {
                    change: haider_rpc::DescendantChangeKindWire::Updated,
                    child,
                },
            ..
        } = sink.next().await
            && child.parent_anchors.result_seq.is_some()
        {
            assert_eq!(child.parent_anchors.result_seq, Some(committed.first_seq));
            assert_eq!(
                child.parent_anchors.result_item_seq,
                Some(committed.last_seq)
            );
            break;
        }
    }

    let late_session = SessionId::new("descendant-stream-late-session");
    create_typed_session(&store, &late_session, "openai").await;
    let late = fleet_delegation(&root, &root, &late_session, "late", None, 1);
    store
        .create_delegation(late.clone())
        .await
        .expect("late lineage relation");
    loop {
        if let WireFrame::SessionDescendantStream {
            event:
                haider_rpc::SessionDescendantStreamEventWire::Delta {
                    change: haider_rpc::DescendantChangeKindWire::Appeared,
                    child,
                },
            ..
        } = sink.next().await
            && child.agent_id == late.agent_id
        {
            assert_eq!(child.session_id, late_session);
            assert_ne!(child.session_id.as_str(), child.agent_id.as_str());
            break;
        }
    }

    connection.close().await.expect("connection closes");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// FAN-OUT LAW: the daemon clamps the requested live fan-out to a typed cap
/// and reports every known omission. A bounded baseline never presents an
/// empty child list as complete.
#[tokio::test]
async fn descendant_stream_fanout_truncation_is_explicit() {
    let (_root, store, hub) = open_hub(None, 8).await;
    let root = SessionId::new("descendant-truncated-root");
    create_typed_session(&store, &root, "openai").await;
    for suffix in ["first", "second"] {
        let child = SessionId::new(format!("descendant-truncated-{suffix}"));
        create_typed_session(&store, &child, "openai").await;
        store
            .create_delegation(fleet_delegation(&root, &root, &child, suffix, None, 1))
            .await
            .expect("truncation relation");
    }

    let sink = Arc::new(CollectSink::default());
    let connection = hub
        .open_connection(
            CapabilitySet::from([Capability::View]),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("truncation connection");
    connection
        .request(
            RequestId::new("descendant-truncated-attach"),
            RequestBody::SessionDescendantsAttach {
                session_id: root,
                cursors: Vec::new(),
                max_children: 1,
            },
        )
        .await
        .expect("bounded descendant attach routes");
    let WireFrame::Response {
        body: ResponseBody::SessionDescendantsAttach { baseline, .. },
        ..
    } = sink.next().await
    else {
        panic!("expected bounded descendant baseline");
    };
    assert_eq!(baseline.fanout.requested_children, 1);
    assert_eq!(baseline.fanout.accepted_children, 1);
    assert_eq!(baseline.fanout.hard_limit, 64);
    assert_eq!(baseline.roots.len(), 1);
    assert!(baseline.truncation.truncated);
    assert_eq!(baseline.truncation.streamed_children, 1);
    assert_eq!(baseline.truncation.omitted_children, 1);
    assert!(baseline.truncation.count_complete);

    connection.close().await.expect("connection closes");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// COHORT RECONNECT LAW: cursors from the prior negotiated cohort seed the
/// next baseline before a newly created shallower child can displace a nested
/// child from the fresh BFS prefix.
#[tokio::test]
async fn descendant_reconnect_preserves_cursor_seeded_ancestry() {
    let (_root, store, hub) = open_hub(None, 8).await;
    let root = SessionId::new("descendant-cohort-root");
    let parent_session = SessionId::new("descendant-cohort-parent");
    let nested_session = SessionId::new("descendant-cohort-nested");
    create_typed_session(&store, &root, "openai").await;
    create_typed_session(&store, &parent_session, "openai").await;
    create_typed_session(&store, &nested_session, "openai").await;
    let parent = fleet_delegation(&root, &root, &parent_session, "parent", None, 1);
    let nested = fleet_delegation(
        &root,
        &parent_session,
        &nested_session,
        "nested",
        Some(parent.agent_id.clone()),
        2,
    );
    store
        .create_delegation(parent.clone())
        .await
        .expect("parent lineage");
    store
        .create_delegation(nested.clone())
        .await
        .expect("nested lineage");

    let sink = Arc::new(CollectSink::default());
    let connection = hub
        .open_connection(
            CapabilitySet::from([Capability::View]),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("cohort connection");
    connection
        .request(
            RequestId::new("cohort-first"),
            RequestBody::SessionDescendantsAttach {
                session_id: root.clone(),
                cursors: Vec::new(),
                max_children: 2,
            },
        )
        .await
        .expect("first cohort attach");
    let WireFrame::Response {
        body:
            ResponseBody::SessionDescendantsAttach {
                attachment_id,
                baseline,
            },
        ..
    } = sink.next().await
    else {
        panic!("expected first cohort baseline");
    };
    assert_eq!(baseline.roots.len(), 1);
    assert_eq!(baseline.roots[0].session_id, parent_session);
    assert_eq!(baseline.roots[0].children[0].session_id, nested_session);
    let mut caught_up = 0;
    while caught_up < 2 {
        if matches!(
            sink.next().await,
            WireFrame::SessionDescendantStream {
                event: haider_rpc::SessionDescendantStreamEventWire::ChildCaughtUp {
                    high_water_seq: 1,
                    ..
                },
                ..
            }
        ) {
            caught_up += 1;
        }
    }
    connection
        .request(
            RequestId::new("cohort-detach"),
            RequestBody::SessionDetach { attachment_id },
        )
        .await
        .expect("detach first cohort");
    assert!(matches!(
        sink.next().await,
        WireFrame::Response {
            body: ResponseBody::SessionDetach { .. },
            ..
        }
    ));

    let shallower_session = SessionId::new("descendant-cohort-shallower");
    create_typed_session(&store, &shallower_session, "openai").await;
    store
        .create_delegation(fleet_delegation(
            &root,
            &root,
            &shallower_session,
            "shallower",
            None,
            1,
        ))
        .await
        .expect("shallower lineage");
    connection
        .request(
            RequestId::new("cohort-reconnect"),
            RequestBody::SessionDescendantsAttach {
                session_id: root,
                cursors: vec![
                    haider_rpc::DescendantReplayCursorWire {
                        session_id: parent_session.clone(),
                        agent_id: parent.agent_id,
                        after_seq: 1,
                    },
                    haider_rpc::DescendantReplayCursorWire {
                        session_id: nested_session.clone(),
                        agent_id: nested.agent_id,
                        after_seq: 1,
                    },
                ],
                max_children: 2,
            },
        )
        .await
        .expect("cursor-seeded reconnect");
    let WireFrame::Response {
        body: ResponseBody::SessionDescendantsAttach { baseline, .. },
        ..
    } = sink.next().await
    else {
        panic!("expected cursor-seeded baseline");
    };
    assert_eq!(baseline.roots.len(), 1);
    assert_eq!(baseline.roots[0].session_id, parent_session);
    assert_eq!(baseline.roots[0].children[0].session_id, nested_session);
    assert!(baseline.truncation.truncated);
    assert_eq!(baseline.truncation.streamed_children, 2);
    assert_eq!(baseline.truncation.omitted_children, 1);

    connection.close().await.expect("connection closes");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// REPAIR/UNKNOWN-ID LAW: a permanently refused first descendant frame
/// purges the still-staged success and returns one correlated retryable error.
#[tokio::test]
async fn descendant_start_refusal_replaces_staged_success_with_typed_error() {
    let (_root, store, hub) = open_hub(None, 8).await;
    let root = SessionId::new("descendant-refusal-root");
    let child = SessionId::new("descendant-refusal-child");
    create_typed_session(&store, &root, "openai").await;
    create_typed_session(&store, &child, "openai").await;
    store
        .create_delegation(fleet_delegation(&root, &root, &child, "refused", None, 1))
        .await
        .expect("refusal lineage");

    let sink = Arc::new(RefuseDescendantStartSink::default());
    let connection = hub
        .open_connection(
            CapabilitySet::from([Capability::View]),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("refusal connection");
    connection
        .request(
            RequestId::new("descendant-refused-attach"),
            RequestBody::SessionDescendantsAttach {
                session_id: root,
                cursors: Vec::new(),
                max_children: 1,
            },
        )
        .await
        .expect("refused attach routes");
    assert!(matches!(
        sink.next().await,
        WireFrame::Response {
            request_id,
            body: ResponseBody::Error {
                ref code,
                retryable: true,
                ..
            },
        } if request_id.as_str() == "descendant-refused-attach"
            && code == haider_rpc::ERROR_CODE_OVERLOADED
    ));

    connection.close().await.expect("connection closes");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// FLEET LAW: a View-only, receipt-free read rebuilds nested shape, every
/// display state, direct v0.0.902 metrics, and the complete rollup from
/// durable child journals even after the root session itself is terminal.
/// A connection without View is denied without requiring a control attach.
#[tokio::test]
async fn session_fleet_reduces_nested_terminal_tree_and_rollup_with_view_capability() {
    let (_root, store, hub) = open_hub(None, 8).await;
    let generation = store.worker_generation();
    let root = SessionId::new("fleet-root");
    create_typed_session(&store, &root, "openai").await;

    let specs = [
        ("queued", RunState::Queued, 10_u64),
        ("live", RunState::Thinking, 20),
        ("done", RunState::Done, 30),
        ("failed", RunState::Errored, 40),
        ("cancelled", RunState::Cancelled, 50),
    ];
    let mut records = Vec::new();
    for (suffix, state, tokens) in specs {
        let child = SessionId::new(format!("fleet-session-{suffix}"));
        create_typed_session(&store, &child, "openai").await;
        let record = fleet_delegation(&root, &root, &child, suffix, None, 1);
        store
            .create_delegation(record.clone())
            .await
            .expect("root fleet relation");
        append_fleet_metrics(&hub, generation, &record, state, tokens).await;
        records.push(record);
    }
    let live = records
        .iter()
        .find(|record| record.agent_id.as_str() == "fleet-agent-live")
        .expect("live parent");
    let waiting_session = SessionId::new("fleet-session-waiting");
    create_typed_session(&store, &waiting_session, "openai").await;
    let waiting = fleet_delegation(
        &root,
        &live.child_session_id,
        &waiting_session,
        "waiting",
        Some(live.agent_id.clone()),
        2,
    );
    store
        .create_delegation(waiting.clone())
        .await
        .expect("nested fleet relation");
    append_fleet_metrics(
        &hub,
        generation,
        &waiting,
        RunState::Waiting {
            reason: WaitReason::LocalChild,
        },
        60,
    )
    .await;

    let mut root_terminal = envelope(&root, "fleet-root-terminal", generation);
    root_terminal.run_id = Some(RunId::new("fleet-root-run"));
    root_terminal.payload =
        serde_json::to_value(EventPayload::RunState(RunState::Done)).expect("root terminal");
    hub.append(&mut [root_terminal])
        .await
        .expect("terminal root commits");

    let sink = Arc::new(CollectSink::default());
    let connection = hub
        .open_connection(
            CapabilitySet::from([Capability::View]),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("view-only fleet connection");
    connection
        .request(
            RequestId::new("fleet-tree"),
            RequestBody::SessionFleet {
                session_id: root.clone(),
            },
        )
        .await
        .expect("session.fleet routes");
    let WireFrame::Response {
        body: ResponseBody::SessionFleet { snapshot },
        ..
    } = sink.next().await
    else {
        panic!("expected session.fleet response");
    };
    assert_eq!(snapshot.session_id, root);
    assert!(!snapshot.truncated);
    assert_eq!(snapshot.roots.len(), 5);
    assert_eq!(snapshot.rollup.node_count, 6);
    assert_eq!(snapshot.rollup.max_depth, 2);
    assert!(snapshot.rollup.complete);
    assert!(snapshot.rollup.metrics_complete);
    assert_eq!(snapshot.rollup.states.queued, 1);
    assert_eq!(snapshot.rollup.states.live, 1);
    assert_eq!(snapshot.rollup.states.waiting, 1);
    assert_eq!(snapshot.rollup.states.done, 1);
    assert_eq!(snapshot.rollup.states.failed, 1);
    assert_eq!(snapshot.rollup.states.cancelled, 1);
    assert_eq!(snapshot.rollup.metrics.tool_attempts, 6);
    let mut expected_elapsed = 0_u64;
    let mut pending = snapshot.roots.iter().collect::<Vec<_>>();
    while let Some(node) = pending.pop() {
        let metrics = node.metrics.as_ref().expect("direct child metrics");
        expected_elapsed = expected_elapsed.saturating_add(
            metrics
                .terminal_at_ms
                .unwrap_or(snapshot.generated_at_ms)
                .saturating_sub(metrics.started_at_ms),
        );
        pending.extend(node.children.iter());
    }
    assert_eq!(snapshot.rollup.metrics.elapsed_ms, expected_elapsed);
    let totals = snapshot
        .rollup
        .metrics
        .usage
        .as_ref()
        .expect("all child usage is durable");
    assert_eq!(totals.logical_input_tokens, 210);
    assert_eq!(totals.billed_output_tokens, 6);
    assert!(totals.metered_cost_microusd.is_some());
    assert!(totals.api_equivalent_cost_microusd.is_some());
    let live_node = snapshot
        .roots
        .iter()
        .find(|node| node.agent_id.as_str() == "fleet-agent-live")
        .expect("live root node");
    assert_eq!(live_node.state, FleetAgentStateWire::Live);
    assert_eq!(live_node.callsign.as_deref(), Some("CALL-live"));
    assert_eq!(live_node.model.as_deref(), Some("gpt-5.2"));
    assert_eq!(live_node.provider.as_deref(), Some("anthropic"));
    assert_eq!(live_node.children.len(), 1);
    assert_eq!(live_node.children[0].state, FleetAgentStateWire::Waiting);
    assert_eq!(
        live_node.children[0].parent_agent_id.as_ref(),
        Some(&live.agent_id)
    );

    let denied_sink = Arc::new(CollectSink::default());
    let denied = hub
        .open_connection(
            CapabilitySet::new(),
            denied_sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("unprivileged connection");
    denied
        .request(
            RequestId::new("fleet-denied"),
            RequestBody::SessionFleet { session_id: root },
        )
        .await
        .expect("fleet denial routes");
    assert!(matches!(
        denied_sink.next().await,
        WireFrame::Response {
            body: ResponseBody::Error { ref code, .. },
            ..
        } if code == haider_rpc::ERROR_CODE_CAPABILITY_DENIED
    ));

    denied.close().await.expect("denied connection closes");
    connection.close().await.expect("connection closes");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// BOUND LAW: historical terminal children can exceed the concurrent live
/// cap, but the read returns exactly 512 nodes and marks both the tree and its
/// rollup incomplete. No provider work is involved in this fixture.
#[tokio::test]
async fn session_fleet_caps_historical_response_at_512_nodes() {
    let (_root, store, hub) = open_hub(None, 8).await;
    let generation = store.worker_generation();
    let root = SessionId::new("fleet-bounded-root");
    create_typed_session(&store, &root, "openai").await;
    let parent_session = SessionId::new("fleet-bounded-parent");
    create_typed_session(&store, &parent_session, "openai").await;
    let parent = fleet_delegation(&root, &root, &parent_session, "bounded-parent", None, 1);
    store
        .create_delegation(parent.clone())
        .await
        .expect("bounded parent relation");
    let mut parent_done = envelope(
        &parent.child_session_id,
        "fleet-bounded-parent-done",
        generation,
    );
    parent_done.run_id = Some(parent.child_run_id.clone());
    parent_done.agent_id = Some(parent.agent_id.clone());
    parent_done.payload =
        serde_json::to_value(EventPayload::RunState(RunState::Done)).expect("parent done state");
    hub.append(&mut [parent_done])
        .await
        .expect("terminal parent state");
    store
        .record_delegation_report(
            parent.agent_id.clone(),
            ChildReport {
                agent: parent.agent_id.clone(),
                summary: "complete".into(),
                verified: ReportVerification::Unverified,
                workspace_revision: None,
            },
        )
        .await
        .expect("terminal parent bookkeeping");

    for index in 0..512_u32 {
        let suffix = format!("bounded-{index:03}");
        let child = SessionId::new(format!("fleet-bounded-child-{index:03}"));
        create_typed_session(&store, &child, "openai").await;
        let record = fleet_delegation(
            &root,
            &parent_session,
            &child,
            &suffix,
            Some(parent.agent_id.clone()),
            2,
        );
        store
            .create_delegation(record.clone())
            .await
            .expect("historical fleet relation");
        let mut done = envelope(
            &record.child_session_id,
            format!("fleet-bounded-done-{index:03}"),
            generation,
        );
        done.run_id = Some(record.child_run_id.clone());
        done.agent_id = Some(record.agent_id.clone());
        done.payload =
            serde_json::to_value(EventPayload::RunState(RunState::Done)).expect("done state");
        hub.append(&mut [done]).await.expect("terminal child state");
        store
            .record_delegation_report(
                record.agent_id.clone(),
                ChildReport {
                    agent: record.agent_id,
                    summary: "complete".into(),
                    verified: ReportVerification::Unverified,
                    workspace_revision: None,
                },
            )
            .await
            .expect("terminal delegation bookkeeping");
    }

    let sink = Arc::new(CollectSink::default());
    let connection = hub
        .open_connection(
            CapabilitySet::from([Capability::View]),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("fleet connection");
    connection
        .request(
            RequestId::new("fleet-bounded"),
            RequestBody::SessionFleet { session_id: root },
        )
        .await
        .expect("bounded fleet routes");
    let WireFrame::Response {
        body: ResponseBody::SessionFleet { snapshot },
        ..
    } = sink.next().await
    else {
        panic!("expected bounded fleet response");
    };
    assert_eq!(snapshot.roots.len(), 1);
    assert_eq!(snapshot.roots[0].children.len(), 511);
    assert_eq!(snapshot.roots[0].folded_children, 1);
    assert!(
        snapshot.roots[0]
            .children
            .iter()
            .all(|child| child.folded_children == 0),
        "real returned leaves carry no fold witness"
    );
    assert_eq!(snapshot.node_limit, 512);
    assert_eq!(snapshot.depth_limit, haider_rpc::FLEET_MAX_DEPTH);
    assert_eq!(snapshot.rollup.node_count, 512);
    assert_eq!(snapshot.rollup.states.done, 512);
    assert!(snapshot.truncated);
    assert!(!snapshot.rollup.complete);
    assert!(!snapshot.rollup.metrics_complete);

    connection.close().await.expect("connection closes");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// DEPTH-WITNESS LAW: children just outside the depth bound are not
/// materialized, but the returned boundary row carries their exact direct
/// count so it cannot be mistaken for a durable leaf.
#[tokio::test]
async fn session_fleet_counts_children_folded_at_the_depth_bound() {
    let (_root, store, hub) = open_hub(None, 8).await;
    let root = SessionId::new("fleet-depth-root");
    create_typed_session(&store, &root, "openai").await;

    let mut parent_session = root.clone();
    let mut parent_agent = None;
    for depth in 1..=haider_rpc::FLEET_MAX_DEPTH {
        let suffix = format!("depth-{depth:02}");
        let child = SessionId::new(format!("fleet-depth-session-{depth:02}"));
        create_typed_session(&store, &child, "openai").await;
        let record = fleet_delegation(
            &root,
            &parent_session,
            &child,
            &suffix,
            parent_agent.clone(),
            depth,
        );
        store
            .create_delegation(record.clone())
            .await
            .expect("depth chain relation");
        parent_session = child;
        parent_agent = Some(record.agent_id);
    }
    for index in 0..3_u32 {
        let suffix = format!("outside-depth-{index}");
        let child = SessionId::new(format!("fleet-outside-depth-{index}"));
        create_typed_session(&store, &child, "openai").await;
        let record = fleet_delegation(
            &root,
            &parent_session,
            &child,
            &suffix,
            parent_agent.clone(),
            haider_rpc::FLEET_MAX_DEPTH + 1,
        );
        store
            .create_delegation(record)
            .await
            .expect("outside-depth relation");
    }

    let sink = Arc::new(CollectSink::default());
    let connection = hub
        .open_connection(
            CapabilitySet::from([Capability::View]),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("fleet connection");
    connection
        .request(
            RequestId::new("fleet-depth-bound"),
            RequestBody::SessionFleet { session_id: root },
        )
        .await
        .expect("depth-bounded fleet routes");
    let WireFrame::Response {
        body: ResponseBody::SessionFleet { snapshot },
        ..
    } = sink.next().await
    else {
        panic!("expected depth-bounded fleet response");
    };
    assert!(snapshot.truncated);
    assert_eq!(snapshot.rollup.node_count, haider_rpc::FLEET_MAX_DEPTH);
    let mut node = snapshot.roots.first().expect("depth root row");
    for expected_depth in 1..haider_rpc::FLEET_MAX_DEPTH {
        assert_eq!(node.depth, expected_depth);
        assert_eq!(node.folded_children, 0);
        assert_eq!(node.children.len(), 1);
        node = &node.children[0];
    }
    assert_eq!(node.depth, haider_rpc::FLEET_MAX_DEPTH);
    assert!(node.children.is_empty());
    assert_eq!(node.folded_children, 3);

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
        .open_connection(
            capabilities(),
            slow.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("slow connection");
    connection
        .request(
            RequestId::new("slow-attach"),
            RequestBody::SessionAttach {
                session_id: session_id.clone(),
                after_seq: 0,
                mode: AttachMode::View,
                sealed_replay: false,
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
        .open_connection(
            capabilities(),
            resumed.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("resume connection");
    resumed_connection
        .request(
            RequestId::new("resume"),
            RequestBody::SessionAttach {
                session_id,
                after_seq: first,
                mode: AttachMode::View,
                sealed_replay: false,
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
                    .open_connection(
                        capabilities(),
                        sink.clone(),
                        ConnectionTransport::LocalSameUid,
                    )
                    .expect("connection");
                connection
                    .request(
                        RequestId::new(format!("cursor-{after_seq}")),
                        RequestBody::SessionAttach {
                            session_id,
                            after_seq,
                            mode: AttachMode::View,
                            sealed_replay: false,
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
        .open_connection(
            capabilities(),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
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
        .open_connection(
            capabilities(),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
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
    assert!(
        hub.open_connection(capabilities(), sink, ConnectionTransport::LocalSameUid)
            .is_err()
    );

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
            hub.open_connection(
                capabilities(),
                sink.clone(),
                ConnectionTransport::LocalSameUid,
            )
            .expect("connection"),
        );
        connection
            .request(
                RequestId::new(format!("attach-{index}")),
                RequestBody::SessionAttach {
                    session_id: session_id.clone(),
                    after_seq: 0,
                    mode: AttachMode::Control,
                    sealed_replay: false,
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
    // DIRECTED R1 SEAL CHANGE: the old test intentionally coerced
    // `SessionHub` into an unfenced, cross-session `StoreHandle`. That shape
    // must no longer compile. Mint one session lease, give that constrained
    // handle to the harness, and thread the SAME lease into registration.
    let lease = hub
        .acquire_worker_lease(session_id.clone())
        .await
        .expect("worker lease");
    let run_id = RunId::new("live-harness-menu-run");
    let mut accepted_prefix = vec![
        EventEnvelope {
            schema_version: SCHEMA_VERSION,
            event_id: EventId::new("live-harness-menu-queued"),
            seq: 0,
            session_id: session_id.clone(),
            branch_id: None,
            run_id: Some(run_id.clone()),
            agent_id: None,
            device_id: DeviceId::new("hub-harness"),
            authority_epoch: 0,
            worker_generation: generation,
            causation_id: None,
            correlation_id: None,
            committed_at_ms: 0,
            render: RenderTargets {
                ui: true,
                durable: true,
                prompt: PromptRender::Omit,
            },
            payload: serde_json::to_value(EventPayload::RunState(RunState::Queued))
                .expect("queued payload"),
        },
        EventEnvelope {
            schema_version: SCHEMA_VERSION,
            event_id: EventId::new("live-harness-menu-user"),
            seq: 0,
            session_id: session_id.clone(),
            branch_id: None,
            run_id: Some(run_id.clone()),
            agent_id: None,
            device_id: DeviceId::new("hub-harness"),
            authority_epoch: 0,
            worker_generation: generation,
            causation_id: None,
            correlation_id: None,
            committed_at_ms: 0,
            render: RenderTargets {
                ui: true,
                durable: true,
                prompt: PromptRender::Verbatim,
            },
            payload: serde_json::to_value(EventPayload::UserMessage {
                text: "ask through the hub".into(),
                attachments: Vec::new(),
                mode: haider_protocol::DeliveryMode::Queue,
            })
            .expect("user payload"),
        },
    ];
    hub.append(&mut accepted_prefix)
        .await
        .expect("daemon acceptance prefix");
    let harness_store: Arc<dyn StoreHandle> = Arc::new(lease.clone());
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
    lease
        .register_harness(harness.clone())
        .await
        .expect("harness registers");
    let turn = harness
        .submit_committed_turn(SubmitCommittedTurn {
            run_id,
            messages: vec![Message::user_text("ask through the hub")],
        })
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
        .open_connection(
            capabilities(),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("connection");
    connection
        .request(
            RequestId::new("attach"),
            RequestBody::SessionAttach {
                session_id: session_id.clone(),
                after_seq: head,
                mode: AttachMode::Control,
                sealed_replay: false,
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

/// Scenario-13 structural guard for the R1 seal. This deliberately scans
/// production source because Rust has no negative trait-bound assertion: the
/// forbidden coercion must remain absent, while the session-scoped handle is
/// threaded into core and registration.
#[test]
fn worker_surface_is_structurally_lease_scoped() {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let worker = std::fs::read_to_string(manifest.join("src/worker.rs")).expect("worker source");
    let hub = std::fs::read_to_string(manifest.join("src/session_hub/mod.rs")).expect("hub source");
    assert!(!worker.contains("SqliteStoreHandle"));
    assert!(!worker.contains("Arc::new(hub.clone())"));
    assert!(!hub.contains("impl StoreHandle for SessionHub"));
    assert!(!worker.contains("supervisor_hub"));
    let supervisor = worker
        .split("async fn run_supervisor(")
        .nth(1)
        .and_then(|source| source.split("async fn admit_pending(").next())
        .expect("supervisor source");
    assert!(!supervisor.contains("SessionHub"));
    assert!(worker.contains("Arc::new(lease.clone())"));
    assert!(worker.contains(".register_harness(harness.clone())"));
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
        .open_connection(
            capabilities(),
            lost.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("first connection");
    first
        .request(
            RequestId::new("attach-first"),
            RequestBody::SessionAttach {
                session_id: session_id.clone(),
                after_seq: 0,
                mode: AttachMode::Control,
                sealed_replay: false,
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
        .open_connection(
            capabilities(),
            retry_sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("retry connection");
    retry
        .request(
            RequestId::new("attach-retry"),
            RequestBody::SessionAttach {
                session_id: session_id.clone(),
                after_seq: 2,
                mode: AttachMode::Control,
                sealed_replay: false,
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
        .open_connection(
            capabilities(),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("connection");
    connection
        .request(
            RequestId::new("attach"),
            RequestBody::SessionAttach {
                session_id: session_id.clone(),
                after_seq: 1,
                mode: AttachMode::View,
                sealed_replay: false,
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
        hub.open_connection(capabilities(), sink, ConnectionTransport::LocalSameUid)
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
                        sealed_replay: false,
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
        .open_connection(
            capabilities(),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("connection");
    connection
        .request(
            RequestId::new("attach"),
            RequestBody::SessionAttach {
                session_id,
                after_seq: 0,
                mode: AttachMode::Control,
                sealed_replay: false,
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
        .open_connection(
            capabilities(),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("connection");
    connection
        .request(
            RequestId::new("attach"),
            RequestBody::SessionAttach {
                session_id: session_id.clone(),
                after_seq: 0,
                mode: AttachMode::Control,
                sealed_replay: false,
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

/// MUTATION CHECK: clear the recovered menu coordinate after the winning
/// answer commits. Expected failure: a later prior-generation loser is
/// rejected as `stale_generation` instead of observing the durable winner as
/// `already_resolved`.
/// Verified by revert on 2026-07-27.
#[tokio::test]
async fn recovered_menu_coordinate_authorizes_losers_after_the_winner_commits() {
    let (_root, store, hub) = open_hub(None, 8).await;
    let session_id = SessionId::new("recovered-menu-losers");
    let menu_id = MenuId::new("recovered-menu");
    let generation = store.worker_generation();
    let opening_generation = generation.saturating_sub(1);
    let mut opening = vec![menu_opening(&session_id, &menu_id, opening_generation)];
    hub.append(&mut opening).await.expect("menu opens");

    let lease = hub
        .acquire_worker_lease(session_id.clone())
        .await
        .expect("worker lease");
    let config = HarnessConfig::for_session(
        session_id.clone(),
        DeviceId::new("recovered-worker"),
        11,
        generation,
    );
    let (_actor, harness) = HarnessActor::new(
        config,
        Arc::new(FakeProvider::new(Vec::new())),
        Arc::new(lease.clone()),
    );
    lease
        .register_recovered_harness(harness, menu_id.clone(), opening[0].seq, opening_generation)
        .await
        .expect("recovered harness registers");

    let sink = Arc::new(CollectSink::default());
    let connection = hub
        .open_connection(
            capabilities(),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("connection");
    connection
        .request(
            RequestId::new("attach"),
            RequestBody::SessionAttach {
                session_id: session_id.clone(),
                after_seq: opening[0].seq,
                mode: AttachMode::Control,
                sealed_replay: false,
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

    connection
        .menu_answer(
            Some(RequestId::new("winner")),
            CommandId::new("winner-command"),
            session_id.clone(),
            menu_id.clone(),
            opening[0].seq,
            opening_generation,
            "allow".into(),
            0,
            None,
        )
        .await
        .expect("winner routes");
    let mut winner_response = false;
    let mut winner_event = false;
    while !winner_response || !winner_event {
        match sink.next().await {
            WireFrame::Response {
                request_id,
                body: ResponseBody::MenuAnswer { resolution_seq: 2 },
            } if request_id.as_str() == "winner" => winner_response = true,
            WireFrame::Event { envelope, .. } if envelope.seq == 2 => winner_event = true,
            frame => panic!("unexpected winner frame: {frame:?}"),
        }
    }

    connection
        .menu_answer(
            Some(RequestId::new("loser")),
            CommandId::new("loser-command"),
            session_id,
            menu_id,
            opening[0].seq,
            opening_generation,
            "deny".into(),
            1,
            None,
        )
        .await
        .expect("loser routes");
    let loser = sink.next().await;
    assert!(
        matches!(
        loser,
        WireFrame::Response {
            ref request_id,
            body:
                ResponseBody::Error {
                    ref code,
                    data: Some(ErrorData::AlreadyResolved { resolution_seq: 2 }),
                    ..
                },
        } if request_id.as_str() == "loser" && code == "already_resolved"
        ),
        "unexpected loser response: {loser:?}"
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
                sealed_replay: false,
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
        .open_connection(
            capabilities(),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
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
                sealed_replay: false,
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
        .open_connection(
            capabilities(),
            first_sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("first connection");
    let second_sink = Arc::new(CollectSink::default());
    let second = hub
        .open_connection(
            capabilities(),
            second_sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
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
                sealed_replay: false,
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
        .open_connection(
            capabilities(),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("connection");
    connection
        .request(
            RequestId::new("attach"),
            RequestBody::SessionAttach {
                session_id: session_id.clone(),
                after_seq: 0,
                mode: AttachMode::View,
                sealed_replay: false,
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
        .open_connection(
            capabilities(),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("connection");
    connection
        .request(
            RequestId::new("attach"),
            RequestBody::SessionAttach {
                session_id: session_id.clone(),
                after_seq: 0,
                mode: AttachMode::View,
                sealed_replay: false,
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
        .open_connection(
            capabilities(),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("connection");
    connection
        .request(
            RequestId::new("attach"),
            RequestBody::SessionAttach {
                session_id: session_id.clone(),
                after_seq: 1,
                mode: AttachMode::View,
                sealed_replay: false,
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
        hub.open_connection(
            capabilities(),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
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
                        sealed_replay: false,
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
        .open_connection(
            capabilities(),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("connection");
    connection
        .request(
            RequestId::new("attach-unheard"),
            RequestBody::SessionAttach {
                session_id: session_id.clone(),
                after_seq: 0,
                mode: AttachMode::View,
                sealed_replay: false,
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
        .open_connection(
            capabilities(),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("connection");
    connection
        .request(
            RequestId::new("attach"),
            RequestBody::SessionAttach {
                session_id: session_id.clone(),
                after_seq: 0,
                mode: AttachMode::View,
                sealed_replay: false,
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
        .open_connection(
            capabilities(),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("connection");
    connection
        .request(
            RequestId::new("attach"),
            RequestBody::SessionAttach {
                session_id: session_id.clone(),
                after_seq: 0,
                mode: AttachMode::View,
                sealed_replay: false,
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
    // The actor must be gone before releasing the parked replay; otherwise a
    // fast replay may still re-register and take the ordinary replay path.
    // This lifecycle observation supplies the same happens-before edge on
    // every runtime and OS without extending the product drain deadline.
    control.observed_shutdown_actors_stopped().await;
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
        .open_connection(
            capabilities(),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("connection");
    connection
        .request(
            RequestId::new("attach"),
            RequestBody::SessionAttach {
                session_id: session_id.clone(),
                after_seq: 0,
                mode: AttachMode::View,
                sealed_replay: false,
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
        hub.open_connection(
            capabilities(),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("connection"),
    );
    assert!(matches!(
        sink.next().await,
        WireFrame::ResidentSessionBinding { .. }
    ));
    for (index, session_id) in sessions.iter().enumerate() {
        connection
            .request(
                RequestId::new(format!("attach-{index}")),
                RequestBody::SessionAttach {
                    session_id: session_id.clone(),
                    after_seq: 0,
                    mode: AttachMode::View,
                    sealed_replay: false,
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
                sealed_replay: false,
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

/// MUTATION CHECK: let `account.oauth_import` bypass
/// `secret_surface_facade`. Expected runtime failure: the remote import
/// request is handed to the actor instead of returning the same-UID
/// capability denial asserted for every OAuth secret surface.
#[tokio::test]
async fn remote_control_connection_cannot_reach_any_oauth_secret_surface() {
    let (_root, store, hub) = open_hub(None, 8).await;
    let sink = Arc::new(CollectSink::default());
    let connection = hub
        .open_connection(capabilities(), sink.clone(), ConnectionTransport::Remote)
        .expect("remote connection");
    let flow_id = haider_rpc::OAuthFlowId::new("remote-flow");
    for (index, body) in [
        RequestBody::AccountOAuthStart {
            provider: "fake-oauth".into(),
            desired_alias: "remote".into(),
            attempt_id: "remote-attempt".into(),
        },
        RequestBody::AccountOAuthStatus {
            flow_id: flow_id.clone(),
            attempt_id: "remote-attempt".into(),
        },
        RequestBody::AccountOAuthCancel {
            flow_id: flow_id.clone(),
            attempt_id: "remote-attempt".into(),
        },
        RequestBody::AccountOAuthImportSources,
        RequestBody::AccountOAuthImport {
            command_id: CommandId::new("remote-import-command"),
            source: "codex".into(),
        },
        RequestBody::AccountAdd {
            command_id: CommandId::new("remote-command"),
            provider: "fake-oauth".into(),
            alias: "remote".into(),
            auth_method: haider_rpc::AccountAddMethod::OAuth,
            flow_id: flow_id.clone(),
            attempt_id: "remote-attempt".into(),
            oauth_reference: haider_rpc::OAuthReadyRefWire::new("remote-ready"),
        },
    ]
    .into_iter()
    .enumerate()
    {
        let request_id = RequestId::new(format!("remote-oauth-{index}"));
        connection
            .request(request_id.clone(), body)
            .await
            .expect("remote request routes");
        assert!(matches!(
            sink.next().await,
            WireFrame::Response {
                request_id: response_id,
                body: ResponseBody::Error { code, message, .. },
            } if response_id == request_id
                && code == haider_rpc::ERROR_CODE_CAPABILITY_DENIED
                && message.contains("same-UID")
        ));
    }
    drop(connection);
    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}

/// P3-2 (W3c1 review r2): the `cancellation_fences_start` CALL SITE in
/// `start_turn` is load-bearing and pinned by an EXECUTING guard. The
/// blocked-factory live schedule proves the observable (zero provider
/// requests after a durable cancel), but its enforcement there rides an
/// unbiased wake-race; deleting the call-site block survived that suite
/// 10/10. This guard discriminates the exact production mutation instead.
///
/// MUTATION CHECK: delete the fence block in `start_turn` (the
/// `cancellation_fences_start(durable_run_state(...))` check between tool
/// resolution and `HarnessActor::new_with_dispatcher`). Expected failure:
/// the ordering assertions below — the fence must sit AFTER both factory
/// awaits (the last uncancellable boundary) and BEFORE the harness actor is
/// constructed. Verified by revert on 2026-07-27.
#[test]
fn cancellation_fence_call_site_is_pinned_between_factories_and_harness_spawn() {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let worker = std::fs::read_to_string(manifest.join("src/worker.rs")).expect("worker source");
    let start_turn = worker
        .split("async fn start_turn(")
        .nth(1)
        .and_then(|source| source.split("\nasync fn ").next())
        .expect("start_turn source");
    let fence = start_turn
        .find("if cancellation_fences_start(durable_run_state(lease, &accepted.run_id).await)")
        .expect("start_turn must recheck durable cancellation at its last uncancellable boundary");
    // W-B: the resolution await is now the degrade-aware overload
    // (`resolve_for_turn_with_web`); the ORDER this guard pins is unchanged.
    let provider_resolution = start_turn
        .find("resolve_for_turn_with_web(metadata, web_degrade)")
        .expect("provider resolution present");
    let tool_resolution = start_turn
        .find(".tool_factory")
        .expect("tool resolution present");
    let harness_spawn = start_turn
        .find("HarnessActor::new_with_dispatcher")
        .expect("harness construction present");
    assert!(
        provider_resolution < fence && tool_resolution < fence,
        "the fence must run AFTER both cancellable factory awaits"
    );
    assert!(
        fence < harness_spawn,
        "the fence must run BEFORE the harness actor exists (no provider start after durable Cancelling)"
    );
    // The fenced branch closes the dispatcher and returns the typed error.
    let fenced_branch = &start_turn[fence..harness_spawn];
    assert!(
        fenced_branch.contains("cancellation_fenced_start()"),
        "the fenced branch must return the typed RunNotActive reason"
    );
}

#[tokio::test]
async fn native_pipe_sidecar_matches_shared_renderer_for_all_body_kinds() {
    let (root, store, hub) = open_hub(None, 8).await;
    let session_id = SessionId::new("native-pipe-consistency");
    let generation = store.worker_generation();
    let presentation = ErrorPresentation::new(
        "pipe-test",
        "Broken | title",
        "detail\\line\ntwo",
        ErrorScope::Turn,
        [ErrorAction::Retry],
    );
    let mut events = vec![
        user_pipe_event(&session_id, "user", generation, "hello | pipe\\world\nnext"),
        pipe_event(
            &session_id,
            "assistant",
            generation,
            EventPayload::NodeCommitted(TreeNode {
                node: NodeId::new("node-assistant"),
                parent: None,
                kind: NodeKind::AssistantCommit {
                    text: "answer\nwith | syntax".into(),
                    verdict: VerifyVerdict::NotApplicable,
                },
            }),
        ),
        pipe_event(
            &session_id,
            "incomplete",
            generation,
            EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new("item-incomplete"),
                item: TurnItem::IncompleteAgentMessage {
                    text: "partial\\text".into(),
                    interruption: presentation.clone(),
                },
            }),
        ),
        pipe_event(
            &session_id,
            "error",
            generation,
            EventPayload::RunFailed {
                code: ErrorCode::Internal,
                message: "private diagnostic".into(),
                retryable: true,
                presentation: Some(presentation),
            },
        ),
        pipe_event(
            &session_id,
            "tool",
            generation,
            EventPayload::NodeCommitted(TreeNode {
                node: NodeId::new("node-tool"),
                parent: None,
                kind: NodeKind::ToolExchange {
                    tool: "shell".into(),
                    summary: "ran | command\r\nok".into(),
                    artifact: None,
                },
            }),
        ),
    ];
    // A non-node envelope can carry branch provenance without requiring the
    // test to seed the store's branch graph first.
    events[3].branch_id = Some(BranchId::new("branch-sidecar"));

    hub.append(&mut events).await.expect("pipe batch commits");
    let bytes = stable_sidecar(&sidecar_path(&root, &session_id))
        .await
        .into_bytes();
    assert_eq!(
        bytes,
        expected_sidecar(&session_id, 1, &events).into_bytes()
    );
    assert_eq!(
        String::from_utf8(bytes).expect("sidecar utf8"),
        stored_sidecar(&store, &session_id, 1).await
    );
    let sidecar = stable_sidecar(&sidecar_path(&root, &session_id)).await;
    let values: Vec<serde_json::Value> = sidecar
        .lines()
        .skip(1)
        .map(|line| serde_json::from_str(line).expect("sidecar line is JSON"))
        .collect();
    assert_eq!(
        values
            .iter()
            .find_map(|value| value.get("branch_id"))
            .expect("branched row"),
        "branch-sidecar"
    );
    assert!(
        values
            .iter()
            .filter(|value| value.get("role").is_some())
            .all(|value| value["ordinal"] == 0),
        "every row kind carries its ordinal identity"
    );
    assert_eq!(values.last().expect("coverage")["coverage"], events[4].seq);
    assert_eq!(values.last().expect("coverage")["generation"], 1);
    let row_lines: Vec<&str> = sidecar
        .lines()
        .skip(1)
        .filter(|line| line.contains("\"role\""))
        .collect();
    let expected_rows: Vec<String> = events
        .iter()
        .filter_map(haider_protocol::pipe::sidecar_row_line)
        .collect();
    assert_eq!(row_lines, expected_rows);
    hub.shutdown().await.expect("hub stops");
}

/// MUTATION CHECK: read sealed reasoning from `payload.delta.text` or emit the
/// assistant row before the matching completed item. Expected RUNTIME failure:
/// the durable row carries `stream fragment` or omits `sealed summary`.
#[tokio::test]
async fn native_pipe_carries_only_sealed_reasoning_on_the_assistant_row() {
    let (root, store, hub) = open_hub(None, 8).await;
    let session_id = SessionId::new("native-pipe-sealed-reasoning");
    let generation = store.worker_generation();
    let run_id = RunId::new("reasoning-run");
    let item_id = ItemId::new("reasoning-item");

    let mut started = pipe_event(
        &session_id,
        "reasoning-started",
        generation,
        EventPayload::Item(ItemEvent::Started {
            item_id: item_id.clone(),
            item: TurnItem::Reasoning {
                summary: String::new(),
            },
        }),
    );
    started.run_id = Some(run_id.clone());
    let mut delta = pipe_event(
        &session_id,
        "reasoning-delta",
        generation,
        EventPayload::Item(ItemEvent::Delta {
            item_id: item_id.clone(),
            delta: ItemDelta::Reasoning {
                text: "stream fragment".into(),
            },
        }),
    );
    delta.run_id = Some(run_id.clone());
    let mut assistant = pipe_event(
        &session_id,
        "reasoning-assistant",
        generation,
        EventPayload::NodeCommitted(TreeNode {
            node: NodeId::new("reasoning-assistant-node"),
            parent: None,
            kind: NodeKind::AssistantCommit {
                text: "answer".into(),
                verdict: VerifyVerdict::Unverified,
            },
        }),
    );
    assistant.run_id = Some(run_id.clone());
    let mut sealed = pipe_event(
        &session_id,
        "reasoning-sealed",
        generation,
        EventPayload::Item(ItemEvent::Completed {
            item_id,
            item: TurnItem::Reasoning {
                summary: "sealed summary".into(),
            },
        }),
    );
    sealed.run_id = Some(run_id.clone());
    let done = run_state_pipe_event(
        &session_id,
        "reasoning-done",
        generation,
        run_id.as_str(),
        RunState::Done,
    );
    let mut events = vec![started, delta, assistant, sealed, done];
    hub.append(&mut events)
        .await
        .expect("reasoning turn commits");
    hub.shutdown().await.expect("hub stops");

    let sidecar =
        std::fs::read_to_string(sidecar_path(&root, &session_id)).expect("reasoning sidecar reads");
    let assistant: serde_json::Value = sidecar
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|row| row.get("role").and_then(serde_json::Value::as_str) == Some("assistant"))
        .expect("assistant row");
    assert_eq!(assistant["text"], "answer");
    assert_eq!(assistant["reasoning"], "sealed summary");
    assert!(
        !sidecar.contains("stream fragment"),
        "delta leaked: {sidecar}"
    );
    let tail: serde_json::Value =
        serde_json::from_str(sidecar.lines().last().expect("coverage")).expect("coverage JSON");
    assert_eq!(tail["coverage"], events.last().expect("head").seq);
}

/// MUTATION CHECK: rotate in the `NodeKind::Compaction` observation arm
/// instead of the terminal run-state arm. Expected RUNTIME failure: the two
/// passes below create three physical files instead of one successor.
#[tokio::test]
async fn native_pipe_multiple_compaction_passes_create_exactly_one_segment() {
    let (root, store, hub) = open_hub(None, 8).await;
    let session_id = SessionId::new("native-pipe-one-turn-segment");
    let generation = store.worker_generation();
    let mut seed = vec![user_pipe_event(
        &session_id,
        "segment-seed",
        generation,
        "before",
    )];
    hub.append(&mut seed).await.expect("seed commits");

    let mut compacting = vec![
        compaction_pipe_event(&session_id, "compact-pass-one", generation, "compact-run"),
        run_state_pipe_event(
            &session_id,
            "compact-thinking",
            generation,
            "compact-run",
            RunState::Thinking,
        ),
        compaction_pipe_event(&session_id, "compact-pass-two", generation, "compact-run"),
        run_state_pipe_event(
            &session_id,
            "compact-done",
            generation,
            "compact-run",
            RunState::Done,
        ),
        user_pipe_event(&session_id, "segment-after", generation, "after"),
    ];
    hub.append(&mut compacting)
        .await
        .expect("compacting turn commits");
    hub.shutdown().await.expect("hub stops");

    let base = sidecar_path(&root, &session_id);
    let root_segment = std::fs::read_to_string(&base).expect("root segment reads");
    assert_eq!(
        root_segment
            .matches("\"kind\":\"compaction_boundary\"")
            .count(),
        1,
        "one terminal boundary row: {root_segment}"
    );
    let successor = successor_path(&base, &root_segment);
    let next_segment = std::fs::read_to_string(&successor).expect("successor reads");
    assert!(next_segment.contains("\"starts_after\":"));
    assert!(next_segment.contains("\"text\":\"after\""));
    let files = std::fs::read_dir(base.parent().expect("pipe dir"))
        .expect("pipe dir reads")
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(session_id.as_str())
        })
        .count();
    assert_eq!(files, 2, "root plus exactly one successor");
}

/// MUTATION CHECK: remove the at-durable-head
/// `projector.flush_unresolved_tools()` call from `render_hot_batch`.
/// Expected runtime failure: the hot root has neither the unresolved tool row
/// nor the terminal compaction boundary, and no successor segment exists.
#[tokio::test]
async fn native_pipe_hot_eof_flushes_tool_before_terminal_compaction_boundary() {
    let (root, store, hub) = open_hub(None, 8).await;
    let session_id = SessionId::new("native-pipe-hot-tool-boundary");
    let generation = store.worker_generation();
    let run_id = RunId::new("hot-tool-boundary-run");

    // First touch takes a cold rebuild path; the next append below is the
    // dominant live `render_hot_batch` path this regression must pin.
    let mut seed = vec![user_pipe_event(
        &session_id,
        "hot-tool-boundary-seed",
        generation,
        "before",
    )];
    hub.append(&mut seed).await.expect("seed commits");
    let _ = stable_sidecar(&sidecar_path(&root, &session_id)).await;

    let mut tool_call = pipe_event(
        &session_id,
        "hot-unresolved-tool-call",
        generation,
        EventPayload::Item(ItemEvent::Completed {
            item_id: ItemId::new("hot-unresolved-tool-item"),
            item: TurnItem::ToolCall {
                call_id: "hot-unresolved-call".into(),
                name: "shell".into(),
                args: serde_json::json!({"cmd": "printf hot"}),
                status: haider_protocol::item::ToolStatus::Completed,
            },
        }),
    );
    tool_call.run_id = Some(run_id.clone());
    let mut tool_node = pipe_event(
        &session_id,
        "hot-unresolved-tool-node",
        generation,
        EventPayload::NodeCommitted(TreeNode {
            node: NodeId::new("hot-unresolved-tool-node"),
            parent: None,
            kind: NodeKind::ToolExchange {
                tool: "shell".into(),
                summary: "ran without a provider result".into(),
                artifact: None,
            },
        }),
    );
    tool_node.run_id = Some(run_id.clone());
    let mut hot = vec![
        tool_call,
        tool_node,
        compaction_pipe_event(
            &session_id,
            "hot-tool-boundary-compaction",
            generation,
            run_id.as_str(),
        ),
        run_state_pipe_event(
            &session_id,
            "hot-tool-boundary-done",
            generation,
            run_id.as_str(),
            RunState::Done,
        ),
    ];
    hub.append(&mut hot)
        .await
        .expect("hot compacting batch commits");
    hub.shutdown().await.expect("hub stops");

    let base = sidecar_path(&root, &session_id);
    let root_segment = std::fs::read_to_string(&base).expect("root segment reads");
    let tool_at = root_segment
        .find("\"name\":\"shell\"")
        .expect("unresolved tool row is emitted at hot EOF");
    let boundary_at = root_segment
        .find("\"kind\":\"compaction_boundary\"")
        .expect("terminal boundary follows the tool row");
    assert!(
        tool_at < boundary_at,
        "tool must precede boundary: {root_segment}"
    );
    let successor = successor_path(&base, &root_segment);
    let successor_segment =
        std::fs::read_to_string(successor).expect("terminal boundary creates successor");
    let tail: serde_json::Value = serde_json::from_str(
        successor_segment
            .lines()
            .last()
            .expect("successor coverage"),
    )
    .expect("successor coverage JSON");
    assert_eq!(tail["coverage"], hot.last().expect("durable head").seq);
}

/// MUTATION CHECK: omit the `segment_end` line after writing the boundary.
/// Expected RUNTIME failure: EOF of the root segment with coverage equal to
/// `head_seq` is indistinguishable from the final at-head EOF.
#[tokio::test]
async fn native_pipe_reader_distinguishes_segment_ended_from_at_head() {
    let (root, store, hub) = open_hub(None, 8).await;
    let session_id = SessionId::new("native-pipe-eof-contract");
    let generation = store.worker_generation();
    let mut events = vec![
        compaction_pipe_event(&session_id, "eof-compaction", generation, "eof-run"),
        run_state_pipe_event(
            &session_id,
            "eof-done",
            generation,
            "eof-run",
            RunState::Done,
        ),
    ];
    hub.append(&mut events)
        .await
        .expect("compacting turn commits");
    hub.shutdown().await.expect("hub stops");

    let head_seq = events.last().expect("head event").seq;
    let base = sidecar_path(&root, &session_id);
    let root_segment = std::fs::read_to_string(&base).expect("root reads");
    let terminator: serde_json::Value =
        serde_json::from_str(root_segment.lines().last().expect("sealed root terminator"))
            .expect("terminator JSON");
    assert_eq!(terminator["coverage"], head_seq);
    assert_eq!(terminator["segment_end"], "sealed");
    let successor = successor_path(&base, &root_segment);
    let active = std::fs::read_to_string(&successor).expect("active successor reads");
    let active_tail: serde_json::Value =
        serde_json::from_str(active.lines().last().expect("active coverage"))
            .expect("active tail JSON");
    assert_eq!(active_tail["coverage"], head_seq);
    assert!(active_tail.get("segment_end").is_none());

    // Exercise the reader/reconciler, not just the writer's bytes: a fresh
    // hub must follow the sealed EOF and resume the existing active segment.
    let reopened = SessionHub::new(store.clone(), SessionHubConfig::default())
        .expect("fresh hub reopens sidecar state");
    let mut after = vec![user_pipe_event(
        &session_id,
        "eof-after-reopen",
        generation,
        "after-reopen",
    )];
    reopened
        .append(&mut after)
        .await
        .expect("append after reopen");
    reopened.shutdown().await.expect("reopened hub stops");
    let root_after = std::fs::read_to_string(&base).expect("sealed root still reads");
    assert_eq!(root_after, root_segment, "sealed root stays immutable");
    let active_after = std::fs::read_to_string(&successor).expect("active successor rereads");
    assert!(active_after.contains("\"text\":\"after-reopen\""));
    assert!(active_after.contains("\"generation\":1"));
}

/// MUTATION CHECK: inspect only the final line and ignore an earlier sealed
/// terminator. Expected RUNTIME failure: the fresh writer appends to the root
/// after its terminator, making the new row invisible to chain readers.
#[tokio::test]
async fn native_pipe_rebuilds_when_data_follows_a_sealed_terminator() {
    let (root, store, hub) = open_hub(None, 8).await;
    let session_id = SessionId::new("native-pipe-data-after-seal");
    let generation = store.worker_generation();
    let mut events = vec![
        compaction_pipe_event(&session_id, "after-seal-compact", generation, "seal-run"),
        run_state_pipe_event(
            &session_id,
            "after-seal-done",
            generation,
            "seal-run",
            RunState::Done,
        ),
    ];
    hub.append(&mut events)
        .await
        .expect("segmented turn commits");
    hub.shutdown().await.expect("first hub stops");

    let base = sidecar_path(&root, &session_id);
    let mut corrupt = OpenOptions::new()
        .append(true)
        .open(&base)
        .expect("sealed root opens for corruption fixture");
    use std::io::Write as _;
    writeln!(
        corrupt,
        "{}",
        serde_json::json!({"coverage": events[1].seq, "generation": 1})
    )
    .expect("complete row follows terminator");
    drop(corrupt);

    let reopened = SessionHub::new(store.clone(), SessionHubConfig::default())
        .expect("fresh hub reopens sidecar state");
    let mut after = vec![user_pipe_event(
        &session_id,
        "after-corrupt-seal",
        generation,
        "visible-after-rebuild",
    )];
    reopened
        .append(&mut after)
        .await
        .expect("append triggers rebuild");
    reopened.shutdown().await.expect("reopened hub stops");

    let rebuilt = std::fs::read_to_string(&base).expect("rebuilt root reads");
    assert!(rebuilt.contains("\"generation\":2"));
    assert_eq!(rebuilt.matches("\"segment_end\":\"sealed\"").count(), 1);
    let successor = successor_path(&base, &rebuilt);
    let active = std::fs::read_to_string(successor).expect("rebuilt successor reads");
    assert!(active.contains("\"text\":\"visible-after-rebuild\""));
}

/// MUTATION CHECK: remove `OFlags::NOFOLLOW` from sidecar inspection.
/// Expected RUNTIME failure: torn-line repair truncates the symlink target.
#[cfg(unix)]
#[tokio::test]
async fn native_pipe_inspection_never_repairs_through_a_successor_symlink() {
    use std::os::unix::fs::symlink;

    let (root, store, hub) = open_hub(None, 8).await;
    let session_id = SessionId::new("native-pipe-successor-symlink");
    let generation = store.worker_generation();
    let mut events = vec![
        compaction_pipe_event(&session_id, "symlink-compact", generation, "symlink-run"),
        run_state_pipe_event(
            &session_id,
            "symlink-done",
            generation,
            "symlink-run",
            RunState::Done,
        ),
    ];
    hub.append(&mut events)
        .await
        .expect("segmented turn commits");
    hub.shutdown().await.expect("first hub stops");

    let base = sidecar_path(&root, &session_id);
    let sealed = std::fs::read_to_string(&base).expect("sealed root reads");
    let successor = successor_path(&base, &sealed);
    std::fs::remove_file(&successor).expect("fixture removes active segment");
    let victim = root.path().join("must-not-be-truncated.txt");
    std::fs::write(&victim, b"preserve this unterminated content").expect("fixture victim writes");
    symlink(&victim, &successor).expect("fixture successor symlink creates");

    let reopened = SessionHub::new(store.clone(), SessionHubConfig::default())
        .expect("fresh hub reopens sidecar state");
    let mut after = vec![user_pipe_event(
        &session_id,
        "symlink-after",
        generation,
        "maintenance remains best effort",
    )];
    reopened
        .append(&mut after)
        .await
        .expect("journal append remains authoritative");
    reopened.shutdown().await.expect("reopened hub stops");
    assert_eq!(
        std::fs::read(&victim).expect("victim rereads"),
        b"preserve this unterminated content"
    );
}

/// MUTATION CHECK: removing the zero-row coalescing threshold either writes a
/// watermark too early or leaves the sidecar cursor stale at the 256th delta.
#[tokio::test]
async fn native_pipe_coalesces_255_non_rows_and_covers_the_256th() {
    let (root, store, hub) = open_hub(None, 8).await;
    let session_id = SessionId::new("native-pipe-coverage-coalesce");
    let generation = store.worker_generation();
    // #6 (935): sidecar maintenance is off the publish path — the writer
    // task drains at shutdown, so coalescing is now a SETTLED-STATE law:
    // 255 non-row deltas plus the 256th collapse to exactly ONE coverage
    // line at seq+256, observed after the drain, not via live file timing.
    let mut seed = vec![user_pipe_event(&session_id, "seed", generation, "seed")];
    hub.append(&mut seed).await.expect("seed commits");
    let path = sidecar_path(&root, &session_id);

    let mut deltas: Vec<RawEnvelope> = (0..255)
        .map(|index| delta_pipe_event(&session_id, &format!("coalesce-{index}"), generation))
        .collect();
    hub.append(&mut deltas).await.expect("255 deltas commit");
    append_delta(&hub, &session_id, generation, "coalesce-256").await;
    hub.shutdown().await.expect("hub stops");

    let settled = stable_sidecar(&path).await;
    let coverage_lines: Vec<&str> = settled
        .lines()
        .filter(|line| line.contains("\"coverage\""))
        .collect();
    assert_eq!(
        coverage_lines.len(),
        1,
        "255 coalesced deltas plus the 256th settle to one coverage line: {settled}"
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(coverage_lines[0]).expect("coverage JSON"),
        serde_json::json!({"coverage": seed[0].seq + 256, "generation": 1})
    );
}

#[tokio::test]
async fn native_pipe_truncates_a_torn_tail_before_resume() {
    let (root, store, hub) = open_hub(None, 8).await;
    let session_id = SessionId::new("native-pipe-torn-tail");
    let generation = store.worker_generation();
    let mut first = vec![user_pipe_event(&session_id, "first", generation, "first")];
    hub.append(&mut first).await.expect("first commits");
    hub.shutdown().await.expect("first hub stops");

    let path = sidecar_path(&root, &session_id);
    use std::io::Write as _;
    OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("sidecar opens")
        .write_all(b"{\"role\":\"user\",\"text\":\"torn\",\"at_ms\":999,\"seq\":999}")
        .expect("torn tail written");

    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub restarts");
    let mut second = vec![user_pipe_event(&session_id, "second", generation, "second")];
    hub.append(&mut second).await.expect("second commits");
    // #6 (935): drain the writer task (shutdown) before reading the settled
    // sidecar — maintenance no longer completes on the publish path.
    hub.shutdown().await.expect("second hub stops");
    hub.shutdown().await.expect("second hub stops");
    assert_eq!(
        std::fs::read_to_string(&path).expect("settled sidecar reads"),
        expected_sidecar_batches(&session_id, 1, &[&first, &second])
    );
}

#[tokio::test]
async fn native_pipe_resume_skips_the_already_reconciled_batch() {
    let (root, store, hub) = open_hub(None, 8).await;
    let session_id = SessionId::new("native-pipe-resume-skip");
    let generation = store.worker_generation();
    let mut first = vec![user_pipe_event(&session_id, "first", generation, "one")];
    hub.append(&mut first).await.expect("first commits");
    hub.shutdown().await.expect("first hub stops");

    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub restarts");
    let mut second = vec![user_pipe_event(&session_id, "second", generation, "two")];
    hub.append(&mut second).await.expect("second commits");
    let body = stable_sidecar(&sidecar_path(&root, &session_id)).await;
    assert_eq!(
        body,
        expected_sidecar_batches(&session_id, 1, &[&first, &second])
    );
    assert_eq!(body.lines().count(), 5, "current batch must not duplicate");
    hub.shutdown().await.expect("second hub stops");
}

/// MUTATION CHECK: treating only row-shaped tails as resumable rebuilds this
/// healthy v2 file; using the last row cursor duplicates it during catch-up.
#[tokio::test]
async fn native_pipe_mixed_row_and_coverage_tail_resumes_from_coverage() {
    let (root, store, hub) = open_hub(None, 8).await;
    let session_id = SessionId::new("native-pipe-coverage-resume");
    let generation = store.worker_generation();
    let mut seed = vec![user_pipe_event(&session_id, "seed", generation, "one")];
    hub.append(&mut seed).await.expect("seed commits");
    hub.shutdown().await.expect("first hub stops");

    let mut missed = [delta_pipe_event(&session_id, "missed-delta", generation)];
    store
        .append(&mut missed)
        .await
        .expect("offline delta commits");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub restarts");
    append_delta(&hub, &session_id, generation, "resume-trigger").await;

    let sidecar = stable_sidecar(&sidecar_path(&root, &session_id)).await;
    let rows: Vec<&str> = sidecar
        .lines()
        .filter(|line| line.contains("\"role\""))
        .collect();
    assert_eq!(rows.len(), 1, "the row before coverage must not duplicate");
    let tail: serde_json::Value =
        serde_json::from_str(sidecar.lines().last().expect("tail")).expect("tail JSON");
    assert_eq!(tail["coverage"], missed[0].seq + 1);
    assert_eq!(tail["generation"], 1);
    hub.shutdown().await.expect("second hub stops");
}

#[tokio::test]
async fn native_pipe_first_touch_reconciles_events_committed_while_down() {
    let (root, store, hub) = open_hub(None, 8).await;
    let session_id = SessionId::new("native-pipe-missed-down");
    let generation = store.worker_generation();
    let mut first = vec![user_pipe_event(&session_id, "first", generation, "one")];
    hub.append(&mut first).await.expect("first commits");
    hub.shutdown().await.expect("hub stops");

    std::fs::remove_file(sidecar_path(&root, &session_id)).expect("old sidecar removed");
    let mut missed = vec![user_pipe_event(
        &session_id,
        "missed",
        generation,
        "while down",
    )];
    store
        .append(&mut missed)
        .await
        .expect("out-of-daemon commit");

    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub restarts");
    append_one(&hub, &session_id, generation, "non-rendering-trigger").await;
    assert_eq!(
        stable_sidecar(&sidecar_path(&root, &session_id)).await,
        stored_sidecar(&store, &session_id, 1).await
    );
    let sidecar = stable_sidecar(&sidecar_path(&root, &session_id)).await;
    let tail: serde_json::Value =
        serde_json::from_str(sidecar.lines().last().expect("coverage tail"))
            .expect("coverage tail JSON");
    assert_eq!(
        tail["coverage"],
        store.latest_seq(&session_id).await.expect("journal head"),
        "rebuild/reconcile must end with coverage at the journal head"
    );
}

#[tokio::test]
async fn native_pipe_corrupt_tail_rebuilds_atomically_from_the_journal() {
    let (root, store, hub) = open_hub(None, 8).await;
    let session_id = SessionId::new("native-pipe-corrupt-rebuild");
    let generation = store.worker_generation();
    let mut first = vec![user_pipe_event(&session_id, "first", generation, "one")];
    hub.append(&mut first).await.expect("first commits");
    hub.shutdown().await.expect("hub stops");

    let path = sidecar_path(&root, &session_id);
    std::fs::write(
        &path,
        format!(
            "{{\"pipe\":\"haider.session.jsonl\",\"version\":6,\"session_id\":\"{session_id}\",\"generation\":1}}\ngarbage\n"
        ),
    )
    .expect("corruption writes");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub restarts");
    append_one(&hub, &session_id, generation, "non-rendering-trigger").await;
    hub.shutdown().await.expect("second hub stops");
    assert_eq!(
        std::fs::read_to_string(&path).expect("settled sidecar reads"),
        stored_sidecar(&store, &session_id, 2).await
    );
}

#[tokio::test]
async fn native_pipe_tail_ahead_of_journal_rebuilds_and_increments_generation() {
    use std::io::Write as _;

    let (root, store, hub) = open_hub(None, 8).await;
    let session_id = SessionId::new("native-pipe-ahead-tail");
    let generation = store.worker_generation();
    let mut first = vec![user_pipe_event(&session_id, "first", generation, "one")];
    hub.append(&mut first).await.expect("first commits");
    hub.shutdown().await.expect("hub stops");

    let path = sidecar_path(&root, &session_id);
    OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("sidecar opens")
        .write_all(b"{\"role\":\"user\",\"text\":\"ahead\",\"at_ms\":999,\"seq\":999}\n")
        .expect("ahead row writes");

    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub restarts");
    append_one(&hub, &session_id, generation, "non-rendering-trigger").await;
    hub.shutdown().await.expect("second hub stops");
    assert_eq!(
        std::fs::read_to_string(&path).expect("settled sidecar reads"),
        stored_sidecar(&store, &session_id, 2).await,
        "a syntactically valid cursor ahead of the journal must not be trusted"
    );
}

/// MUTATION CHECK: coverage values participate in the same ahead-of-journal
/// guard as row seqs.
#[tokio::test]
async fn native_pipe_coverage_tail_ahead_rebuilds_and_increments_generation() {
    use std::io::Write as _;

    let (root, store, hub) = open_hub(None, 8).await;
    let session_id = SessionId::new("native-pipe-ahead-coverage");
    let generation = store.worker_generation();
    let mut seed = vec![user_pipe_event(&session_id, "seed", generation, "one")];
    hub.append(&mut seed).await.expect("seed commits");
    hub.shutdown().await.expect("hub stops");

    let path = sidecar_path(&root, &session_id);
    OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("sidecar opens")
        .write_all(b"{\"coverage\":999,\"generation\":1}\n")
        .expect("ahead coverage writes");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub restarts");
    append_one(&hub, &session_id, generation, "rebuild-trigger").await;
    hub.shutdown().await.expect("second hub stops");
    assert_eq!(
        std::fs::read_to_string(&path).expect("settled sidecar reads"),
        stored_sidecar(&store, &session_id, 2).await
    );
}

/// MUTATION CHECK: remove the one-line `sweep_orphan_segments_best_effort`
/// call guarded by `first_touch` in `maintain_inner`. Expected RUNTIME
/// failure: the generation-1 successor still exists after the generation-2
/// root and successor have been durably published.
#[tokio::test]
async fn native_pipe_version_rebuild_sweeps_previous_generation_successor() {
    let (root, store, hub) = open_hub(None, 8).await;
    let session_id = SessionId::new("native-pipe-old-generation-sweep");
    let generation = store.worker_generation();
    let mut seed = vec![
        compaction_pipe_event(
            &session_id,
            "old-generation-compaction",
            generation,
            "old-generation-run",
        ),
        run_state_pipe_event(
            &session_id,
            "old-generation-done",
            generation,
            "old-generation-run",
            RunState::Done,
        ),
    ];
    hub.append(&mut seed).await.expect("segmented seed commits");
    hub.shutdown().await.expect("first hub stops");

    let base = sidecar_path(&root, &session_id);
    let old_root = std::fs::read_to_string(&base).expect("old root reads");
    let old_successor = successor_path(&base, &old_root);
    assert!(old_successor.exists(), "old successor fixture exists");
    std::fs::write(
        &base,
        old_root.replacen("\"version\":6", "\"version\":5", 1),
    )
    .expect("old-version root fixture writes");

    let reopened = SessionHub::new(store.clone(), SessionHubConfig::default())
        .expect("hub reopens old sidecar");
    append_one(
        &reopened,
        &session_id,
        generation,
        "old-generation-rebuild-trigger",
    )
    .await;
    reopened.shutdown().await.expect("reopened hub stops");

    assert!(base.exists(), "live root survives");
    let rebuilt_root = std::fs::read_to_string(&base).expect("rebuilt root reads");
    let live_successor = successor_path(&base, &rebuilt_root);
    assert!(live_successor.exists(), "rebuilt successor survives");
    assert_ne!(old_successor, live_successor, "rebuild changes generation");
    assert!(
        !old_successor.exists(),
        "the unreachable previous-generation successor is swept"
    );
}

/// MUTATION CHECK: replace the one-line `reachable.contains(&path)` guard
/// with `false`.
/// Expected RUNTIME failure: a reachable successor is quarantined, the live
/// root rescan can no longer follow its original pathname, and restoration
/// aborts the sweep before the real orphan is removed; the final orphan
/// assertion fails.
#[tokio::test]
async fn native_pipe_orphan_sweep_preserves_every_reachable_segment() {
    let (root, store, hub) = open_hub(None, 8).await;
    let session_id = SessionId::new("native-pipe-reachable-sweep");
    let generation = store.worker_generation();
    for pass in 1..=2 {
        let run_id = format!("reachable-run-{pass}");
        let mut compacting = vec![
            compaction_pipe_event(
                &session_id,
                &format!("reachable-compaction-{pass}"),
                generation,
                &run_id,
            ),
            run_state_pipe_event(
                &session_id,
                &format!("reachable-done-{pass}"),
                generation,
                &run_id,
                RunState::Done,
            ),
        ];
        hub.append(&mut compacting)
            .await
            .expect("compaction pass commits");
    }
    hub.shutdown().await.expect("first hub stops");

    let base = sidecar_path(&root, &session_id);
    let reachable = sidecar_chain_paths(&base);
    assert!(
        reachable.len() >= 3,
        "fixture has root and two reachable successors: {reachable:?}"
    );
    let orphan = base
        .parent()
        .expect("pipe directory")
        .join(format!("{session_id}.g77.s1.pipe"));
    std::fs::write(
        &orphan,
        format!(
            "{{\"pipe\":\"haider.session.jsonl\",\"version\":5,\"session_id\":\"{session_id}\",\"generation\":77,\"segment\":1}}\n{{\"coverage\":0,\"generation\":77}}\n"
        ),
    )
    .expect("owned orphan fixture writes");

    let reopened = SessionHub::new(store.clone(), SessionHubConfig::default())
        .expect("hub reopens live chain");
    append_one(
        &reopened,
        &session_id,
        generation,
        "reachable-sweep-trigger",
    )
    .await;
    reopened.shutdown().await.expect("reopened hub stops");

    for path in reachable {
        assert!(path.exists(), "reachable chain member survives: {path:?}");
    }
    assert!(!orphan.exists(), "unreferenced owned segment is swept");
}

/// MUTATION CHECK: accepting an old header version would append current
/// line kinds beneath a stale header instead of performing the
/// generation-bumped atomic rebuild (v1 through v3 alike rebuild to v4).
#[tokio::test]
async fn native_pipe_v1_header_rebuilds_to_current_with_generation_bump() {
    let (root, store, hub) = open_hub(None, 8).await;
    let session_id = SessionId::new("native-pipe-v1-rebuild");
    let generation = store.worker_generation();
    let mut seed = vec![user_pipe_event(&session_id, "seed", generation, "one")];
    hub.append(&mut seed).await.expect("seed commits");
    hub.shutdown().await.expect("hub stops");

    let path = sidecar_path(&root, &session_id);
    std::fs::write(
        &path,
        format!(
            "{{\"pipe\":\"haider.session.jsonl\",\"version\":1,\"session_id\":\"{session_id}\",\"generation\":4}}\n{}",
            expected_sidecar_body(&seed)
        ),
    )
    .expect("v1 sidecar writes");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub restarts");
    append_one(&hub, &session_id, generation, "rebuild-trigger").await;
    hub.shutdown().await.expect("second hub stops");
    assert_eq!(
        std::fs::read_to_string(&path).expect("settled sidecar reads"),
        stored_sidecar(&store, &session_id, 5).await
    );
}

/// MUTATION CHECK: change the header-version comparison to accept v3.
/// Expected RUNTIME failure: the at-head v3 file remains generation 4 instead
/// of rebuilding all journal rows into a generation-5 v4 projection.
#[tokio::test]
async fn native_pipe_v3_header_rebuilds_to_current_with_generation_bump() {
    let (root, store, hub) = open_hub(None, 8).await;
    let session_id = SessionId::new("native-pipe-v3-rebuild");
    let generation = store.worker_generation();
    let reasoning_id = ItemId::new("v3-reasoning");
    let mut reasoning_started = pipe_event(
        &session_id,
        "v3-reasoning-started",
        generation,
        EventPayload::Item(ItemEvent::Started {
            item_id: reasoning_id.clone(),
            item: TurnItem::Reasoning {
                summary: String::new(),
            },
        }),
    );
    reasoning_started.run_id = Some(RunId::new("v3-run"));
    let mut reasoning_delta = pipe_event(
        &session_id,
        "v3-reasoning-delta",
        generation,
        EventPayload::Item(ItemEvent::Delta {
            item_id: reasoning_id.clone(),
            delta: ItemDelta::Reasoning {
                text: "v3 partial".into(),
            },
        }),
    );
    reasoning_delta.run_id = Some(RunId::new("v3-run"));
    let mut assistant = pipe_event(
        &session_id,
        "v3-assistant",
        generation,
        EventPayload::NodeCommitted(TreeNode {
            node: NodeId::new("v3-assistant-node"),
            parent: None,
            kind: NodeKind::AssistantCommit {
                text: "v3 answer".into(),
                verdict: VerifyVerdict::Unverified,
            },
        }),
    );
    assistant.run_id = Some(RunId::new("v3-run"));
    let mut reasoning_sealed = pipe_event(
        &session_id,
        "v3-reasoning-sealed",
        generation,
        EventPayload::Item(ItemEvent::Completed {
            item_id: reasoning_id,
            item: TurnItem::Reasoning {
                summary: "sealed v3 summary".into(),
            },
        }),
    );
    reasoning_sealed.run_id = Some(RunId::new("v3-run"));
    let mut seed = vec![
        reasoning_started,
        reasoning_delta,
        assistant,
        reasoning_sealed,
        compaction_pipe_event(&session_id, "v3-compaction", generation, "v3-run"),
        run_state_pipe_event(&session_id, "v3-done", generation, "v3-run", RunState::Done),
    ];
    hub.append(&mut seed).await.expect("compaction commits");
    hub.shutdown().await.expect("hub stops");

    let path = sidecar_path(&root, &session_id);
    std::fs::write(
        &path,
        format!(
            "{{\"pipe\":\"haider.session.jsonl\",\"version\":3,\"session_id\":\"{session_id}\",\"generation\":4}}\n{{\"coverage\":{},\"generation\":4}}\n",
            seed.last().expect("v3 head").seq,
        ),
    )
    .expect("v3 sidecar writes");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub restarts");
    let mut trigger = vec![user_pipe_event(
        &session_id,
        "rebuild-trigger",
        generation,
        "after v3",
    )];
    hub.append(&mut trigger).await.expect("trigger commits");
    hub.shutdown().await.expect("second hub stops");
    let root_segment = std::fs::read_to_string(&path).expect("rebuilt root reads");
    let header: serde_json::Value =
        serde_json::from_str(root_segment.lines().next().expect("v4 header")).expect("header JSON");
    assert_eq!(header["version"], 6);
    assert_eq!(header["generation"], 5);
    assert!(root_segment.contains("\"reasoning\":\"sealed v3 summary\""));
    assert!(!root_segment.contains("v3 partial"));
    assert!(root_segment.contains("\"kind\":\"compaction_boundary\""));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(
            root_segment.lines().last().expect("sealed terminator")
        )
        .expect("terminator JSON")["segment_end"],
        "sealed"
    );
    let successor = successor_path(&path, &root_segment);
    let active = std::fs::read_to_string(successor).expect("rebuilt successor reads");
    assert!(active.contains("\"text\":\"after v3\""));
    let coverage: serde_json::Value =
        serde_json::from_str(active.lines().last().expect("active coverage"))
            .expect("coverage JSON");
    assert_eq!(coverage["coverage"], trigger[0].seq);
    assert_eq!(coverage["generation"], 5);
}

#[tokio::test]
async fn native_pipe_io_failure_never_fails_the_journal_append() {
    let (root, store, hub) = open_hub(None, 8).await;
    let session_id = SessionId::new("native-pipe-io-failure");
    let generation = store.worker_generation();
    std::fs::write(root.path().join("pipe"), b"blocks the sidecar directory")
        .expect("blocking file writes");

    let mut event = vec![user_pipe_event(&session_id, "user", generation, "durable")];
    hub.append(&mut event)
        .await
        .expect("sidecar failure must not fail append");
    assert_eq!(
        store.read(&session_id, 0, 10).await.expect("journal reads"),
        event
    );
    assert!(!sidecar_path(&root, &session_id).exists());

    std::fs::remove_file(root.path().join("pipe")).expect("blocking file removes");
    std::fs::create_dir(root.path().join("pipe")).expect("sidecar directory creates");
    std::fs::write(
        sidecar_path(&root, &session_id),
        b"{\"pipe\":\"haider.session.jsonl\",\"version\":6,\"session_id\":\"native-pipe-io-failure\",\"generation\":9}\n{\"role\":\"user\",\"text\":\"ahead\",\"at_ms\":999,\"seq\":999}\n",
    )
    .expect("stale sidecar writes");
    append_one(&hub, &session_id, generation, "retry-trigger").await;
    hub.shutdown().await.expect("hub stops");
    assert_eq!(
        std::fs::read_to_string(sidecar_path(&root, &session_id)).expect("settled sidecar reads"),
        stored_sidecar(&store, &session_id, 10).await,
        "a dirty session must rebuild instead of trusting the old numeric tail"
    );
}

/// v0.0.936 attention state, roster half: `last_activity_ms` moves ONLY on
/// meaningful committed activity (assistant items — never telemetry, config,
/// or unknown bookkeeping), `session.seen` is receipted+idempotent over the
/// wire, and the scalars converge so a client's `last_activity > seen_at`
/// unseen predicate flips exactly at the mark-seen boundary.
///
/// MUTATION CHECK (executed): widen `is_meaningful_activity` to match every
/// payload and the usage/bookkeeping assertions fail; narrow it to nothing
/// and the item-activity assertion fails; drop the receipt replay lookup and
/// the replay-equality assertion fails.
#[tokio::test]
async fn session_seen_rpc_and_activity_scalars_converge_on_the_roster() {
    let (_root, store, hub) = open_hub(None, 8).await;
    let generation = store.worker_generation();
    let session = SessionId::new("observe-attention");
    append_one(&hub, &session, generation, "attention-seed").await;

    let sink = Arc::new(CollectSink::default());
    let connection = hub
        .open_connection(
            capabilities(),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("connection");

    // session.seen requires a control attachment (the daemon refuses a
    // detached acknowledgement), and an attached sink interleaves event
    // frames — every reader below skips to the next RESPONSE frame.
    macro_rules! next_response {
        () => {{
            loop {
                match sink.next().await {
                    WireFrame::Response { body, .. } => break body,
                    WireFrame::Event { .. } | WireFrame::AttachCaughtUp { .. } => {}
                    other => panic!("unexpected frame: {other:?}"),
                }
            }
        }};
    }
    connection
        .request(
            RequestId::new("att-attach"),
            RequestBody::SessionAttach {
                session_id: session.clone(),
                after_seq: 0,
                mode: AttachMode::Control,
                sealed_replay: false,
            },
        )
        .await
        .expect("control attach routes");
    let ResponseBody::SessionAttach { .. } = next_response!() else {
        panic!("expected attach response");
    };

    macro_rules! list_summary {
        ($label:expr) => {{
            connection
                .request(
                    RequestId::new($label),
                    RequestBody::SessionList {
                        cursor: None,
                        limit: 64,
                    },
                )
                .await
                .expect("session.list routes");
            let ResponseBody::SessionList { sessions, .. } = next_response!() else {
                panic!("expected session.list response");
            };
            sessions
                .into_iter()
                .find(|summary| summary.session_id == session)
                .expect("attention session listed")
        }};
    }

    // The seed payload is unknown bookkeeping: no meaningful activity yet.
    let summary = list_summary!("att-list-0");
    assert_eq!(summary.last_activity_ms, None);
    assert_eq!(summary.seen_at_ms, None);
    assert_eq!(summary.waiting_why, None);

    // A committed assistant item IS meaningful activity.
    let mut item = envelope(&session, "attention-item-1", generation);
    item.payload = serde_json::to_value(EventPayload::Item(ItemEvent::Completed {
        item_id: ItemId::new("attention-item-1"),
        item: TurnItem::AgentMessage {
            text: "hello".into(),
        },
    }))
    .expect("item serializes");
    hub.append(&mut [item]).await.expect("item commits");
    let summary = list_summary!("att-list-1");
    let activity = summary.last_activity_ms.expect("item sets activity");
    assert_eq!(
        summary.seen_at_ms, None,
        "unseen before any acknowledgement"
    );

    // Receipted mark-seen over the wire.
    connection
        .request(
            RequestId::new("att-seen-1"),
            RequestBody::SessionSeen {
                command_id: CommandId::new("att-seen-cmd-1"),
                session_id: session.clone(),
                worker_generation: generation,
            },
        )
        .await
        .expect("session.seen routes");
    let body = next_response!();
    let ResponseBody::SessionSeen {
        session_id: seen_session,
        seen_at_ms,
        seen_seq,
        worker_generation: seen_generation,
    } = body
    else {
        panic!("expected session.seen response, got {body:?}");
    };
    assert_eq!(seen_session, session);
    assert_eq!(seen_generation, generation);
    assert!(seen_at_ms >= activity, "seen advances to now");

    // Idempotent replay returns the exact receipt.
    connection
        .request(
            RequestId::new("att-seen-replay"),
            RequestBody::SessionSeen {
                command_id: CommandId::new("att-seen-cmd-1"),
                session_id: session.clone(),
                worker_generation: generation,
            },
        )
        .await
        .expect("session.seen replay routes");
    let ResponseBody::SessionSeen {
        seen_at_ms: replay_at,
        seen_seq: replay_seq,
        ..
    } = next_response!()
    else {
        panic!("expected session.seen replay response");
    };
    assert_eq!((replay_at, replay_seq), (seen_at_ms, seen_seq));

    // The seen fact and telemetry are non-meaningful: activity is unmoved.
    let summary = list_summary!("att-list-2");
    assert_eq!(summary.seen_at_ms, Some(seen_at_ms));
    assert_eq!(
        summary.last_activity_ms,
        Some(activity),
        "marking seen must never look like new activity"
    );
    let usage = footprint_envelope(&session, "attention-usage", generation, 1_000);
    let mut bookkeeping = envelope(&session, "attention-bookkeeping", generation);
    bookkeeping.payload = serde_json::json!({"type": "graph_telemetry_probe"});
    hub.append(&mut [bookkeeping])
        .await
        .expect("bookkeeping commits");
    let summary = list_summary!("att-list-3");
    assert_eq!(
        summary.last_activity_ms,
        Some(activity),
        "unknown bookkeeping must not move activity"
    );
    drop(usage);

    // New assistant activity AFTER seen: the unseen predicate flips back.
    let mut item = envelope(&session, "attention-item-2", generation);
    item.payload = serde_json::to_value(EventPayload::Item(ItemEvent::Completed {
        item_id: ItemId::new("attention-item-2"),
        item: TurnItem::AgentMessage {
            text: "again".into(),
        },
    }))
    .expect("item serializes");
    hub.append(&mut [item]).await.expect("item commits");
    let summary = list_summary!("att-list-4");
    let renewed = summary.last_activity_ms.expect("activity again");
    assert!(
        renewed > seen_at_ms || renewed >= activity,
        "fresh item is visible activity"
    );
    assert_eq!(summary.seen_at_ms, Some(seen_at_ms), "seen is unmoved");

    connection.close().await.expect("connection closes");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// v0.0.936 attention state: `waiting_why` types the parked-for-human cases
/// exactly — permission parks carry `permission` + the menu id, trust/update
/// style confirmations are `approval`, any other input park is `question`,
/// and a session that is not parked carries no waiting reason.
///
/// MUTATION CHECK (executed): swap the permission arm to `Question` (or
/// derive approval for plain question menus) and the typed assertions fail.
#[tokio::test]
async fn waiting_why_types_parked_states_with_menu_identity() {
    let (_root, store, hub) = open_hub(None, 8).await;
    let generation = store.worker_generation();

    let permission_session = SessionId::new("attention-permission");
    let question_session = SessionId::new("attention-question");
    let approval_session = SessionId::new("attention-approval");
    let idle_session = SessionId::new("attention-idle");
    for (session, seed) in [
        (&permission_session, "perm-seed"),
        (&question_session, "question-seed"),
        (&approval_session, "approval-seed"),
        (&idle_session, "idle-seed"),
    ] {
        append_one(&hub, session, generation, seed).await;
    }

    let park = |session: &SessionId,
                menu_id: &str,
                kind: MenuKind,
                state: fn(MenuId) -> RunState|
     -> Vec<RawEnvelope> {
        let menu_id = MenuId::new(menu_id);
        let mut opened = envelope(session, format!("{menu_id}-opened"), generation);
        opened.run_id = Some(RunId::new(format!("{menu_id}-run")));
        opened.payload = serde_json::to_value(EventPayload::MenuOpened(Menu {
            id: menu_id.clone(),
            kind,
            title: "Attention park".into(),
            body: Vec::new(),
            options: vec![MenuOption {
                key: "ok".into(),
                label: "Ok".into(),
                detail: None,
                decision: None,
            }],
            blocking: true,
            scope: MenuScope::Session,
            origin: "attention-test".into(),
            ttl_ms: None,
            timeout_option: None,
        }))
        .expect("menu serializes");
        let mut parked = envelope(session, format!("{menu_id}-state"), generation);
        parked.run_id = Some(RunId::new(format!("{menu_id}-run")));
        parked.payload =
            serde_json::to_value(EventPayload::RunState(state(menu_id))).expect("state");
        vec![opened, parked]
    };

    let mut permission = park(
        &permission_session,
        "attention-perm-menu",
        MenuKind::Permission {
            effect_summary: "write a file".into(),
        },
        |menu| RunState::PermissionRequired { menu },
    );
    hub.append(&mut permission).await.expect("permission parks");
    let mut question = park(
        &question_session,
        "attention-question-menu",
        MenuKind::Question,
        |menu| RunState::InputRequired { menu },
    );
    hub.append(&mut question).await.expect("question parks");
    let mut approval = park(
        &approval_session,
        "attention-approval-menu",
        MenuKind::TrustHook,
        |menu| RunState::InputRequired { menu },
    );
    hub.append(&mut approval).await.expect("approval parks");

    let sink = Arc::new(CollectSink::default());
    let connection = hub
        .open_connection(
            capabilities(),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("connection");
    connection
        .request(
            RequestId::new("why-list"),
            RequestBody::SessionList {
                cursor: None,
                limit: 64,
            },
        )
        .await
        .expect("session.list routes");
    let WireFrame::Response {
        body: ResponseBody::SessionList { sessions, .. },
        ..
    } = sink.next().await
    else {
        panic!("expected session.list response");
    };
    let why = |session: &SessionId| {
        sessions
            .iter()
            .find(|summary| &summary.session_id == session)
            .expect("session listed")
            .waiting_why
            .clone()
    };

    let permission_why = why(&permission_session).expect("permission park types");
    assert_eq!(
        permission_why.kind,
        haider_rpc::WaitingWhyKindWire::Permission
    );
    assert_eq!(
        permission_why.pending_menu_id,
        Some(MenuId::new("attention-perm-menu"))
    );
    let question_why = why(&question_session).expect("question park types");
    assert_eq!(question_why.kind, haider_rpc::WaitingWhyKindWire::Question);
    assert_eq!(
        question_why.pending_menu_id,
        Some(MenuId::new("attention-question-menu"))
    );
    let approval_why = why(&approval_session).expect("approval park types");
    assert_eq!(approval_why.kind, haider_rpc::WaitingWhyKindWire::Approval);
    assert_eq!(why(&idle_session), None, "idle session has no waiting_why");

    // v0.0.937 unified contract beside the frozen waiting_why: needs_input
    // gives the PRECISE kind (trust_hook stays trust_hook, not approval)
    // plus the answerable card coordinates.
    let card = |session: &SessionId| {
        sessions
            .iter()
            .find(|summary| &summary.session_id == session)
            .expect("session listed")
            .needs_input
            .clone()
    };
    let permission_card = card(&permission_session).expect("permission card");
    assert_eq!(
        permission_card.kind,
        haider_rpc::NeedsInputKindWire::Permission
    );
    assert_eq!(
        permission_card.menu_id,
        Some(MenuId::new("attention-perm-menu"))
    );
    assert!(
        !permission_card.options.is_empty(),
        "the card carries answerable options"
    );
    assert!(permission_card.request_seq.is_some() && permission_card.since_ms.is_some());
    let question_card = card(&question_session).expect("question card");
    assert_eq!(question_card.kind, haider_rpc::NeedsInputKindWire::Question);
    let approval_card = card(&approval_session).expect("trust-hook card");
    assert_eq!(
        approval_card.kind,
        haider_rpc::NeedsInputKindWire::TrustHook,
        "needs_input keeps the precise kind"
    );
    assert_eq!(card(&idle_session), None, "idle session needs no input");

    // ANSWERABLE-SET LAW (v0.0.937, ADE-reported): the answer coordinates
    // travel as a SET. A card carrying options MUST also carry menu_id,
    // request_seq and worker_generation — a client answers with those
    // three verbatim, so options without the fence would invite an answer
    // built from nulls or, worse, re-derived coordinates racing a menu
    // that just closed. The types cannot enforce this (each field is
    // independently optional on the menu wire), so it is pinned here.
    //
    // MUTATION CHECK (executed): drop `menu_id` (or request_seq /
    // worker_generation) from the needs_input derivation while leaving
    // options populated, and every parked card below fails this law.
    for session in [&permission_session, &question_session, &approval_session] {
        let card = card(session).expect("parked session publishes a card");
        if !card.options.is_empty() {
            assert!(
                card.menu_id.is_some()
                    && card.request_seq.is_some()
                    && card.worker_generation.is_some(),
                "an answerable card carries the FULL answer fence: {card:?}"
            );
        }
    }

    connection.close().await.expect("connection closes");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// v0.0.937 answer-door parity: EVERY input-required kind is resolvable
/// headlessly through `menu.answer` — update, trust-hook, and choice parks
/// answer by option, and a secret park answers by VAULT REFERENCE (the raw
/// secret never transits the wire). This is the law behind "full control
/// from any surface, never a terminal".
///
/// MUTATION CHECK (executed): refuse non-recovery kinds in the answer path
/// (kind-gate the resolution) and every leg fails; drop the secret-reference
/// arm and the secret leg fails.
#[tokio::test]
async fn every_input_kind_is_answerable_headlessly_over_rpc() {
    let (_root, store, hub) = open_hub(None, 8).await;
    let generation = store.worker_generation();
    let session = SessionId::new("answer-parity");
    append_one(&hub, &session, generation, "parity-seed").await;

    let sink = Arc::new(CollectSink::default());
    let connection = hub
        .open_connection(
            capabilities(),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("connection");
    connection
        .request(
            RequestId::new("parity-attach"),
            RequestBody::SessionAttach {
                session_id: session.clone(),
                after_seq: 0,
                mode: AttachMode::Control,
                sealed_replay: false,
            },
        )
        .await
        .expect("attach routes");
    macro_rules! next_response {
        () => {{
            loop {
                match sink.next().await {
                    WireFrame::Response { body, .. } => break body,
                    WireFrame::Event { .. } | WireFrame::AttachCaughtUp { .. } => {}
                    other => panic!("unexpected frame: {other:?}"),
                }
            }
        }};
    }
    let ResponseBody::SessionAttach { .. } = next_response!() else {
        panic!("expected attach response");
    };

    let park = |kind: MenuKind, menu_id: &str, seq_hint: &str| {
        let menu_id = MenuId::new(menu_id);
        let mut opened = envelope(&session, format!("parity-{seq_hint}-opened"), generation);
        opened.run_id = Some(RunId::new(format!("parity-{seq_hint}-run")));
        opened.payload = serde_json::to_value(EventPayload::MenuOpened(Menu {
            id: menu_id.clone(),
            kind,
            title: format!("parity {seq_hint}"),
            body: Vec::new(),
            options: vec![MenuOption {
                key: "ok".into(),
                label: "Ok".into(),
                detail: None,
                decision: None,
            }],
            blocking: true,
            scope: MenuScope::Session,
            origin: "parity-test".into(),
            ttl_ms: None,
            timeout_option: None,
        }))
        .expect("menu serializes");
        (menu_id, opened)
    };

    let legs: Vec<(&str, MenuKind, Option<MenuInput>)> = vec![
        ("update", MenuKind::Update, None),
        ("trust-hook", MenuKind::TrustHook, None),
        ("choice", MenuKind::Choice, None),
        (
            "secret",
            MenuKind::Secret,
            Some(MenuInput::SecretVaultReference {
                vault_reference: "vault-ref-parity-1".into(),
            }),
        ),
    ];
    for (label, kind, input) in legs {
        let (menu_id, mut opened) = park(kind, &format!("parity-{label}-menu"), label);
        hub.append(std::slice::from_mut(&mut opened))
            .await
            .expect("park commits");
        let request_seq = opened.seq;
        connection
            .menu_answer(
                Some(RequestId::new(format!("parity-{label}"))),
                CommandId::new(format!("parity-{label}-command")),
                session.clone(),
                menu_id.clone(),
                request_seq,
                generation,
                "ok".into(),
                0,
                input,
            )
            .await
            .expect("answer routes");
        let body = next_response!();
        assert!(
            matches!(body, ResponseBody::MenuAnswer { .. }),
            "{label} park must resolve over RPC, got {body:?}"
        );
    }

    connection.close().await.expect("connection closes");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// PRE-RECEIPT REJECTION LAW (v0.0.938, ADE-reported): every check that runs
/// BEFORE the durable receipt is claimed — draining, capability, control
/// attachment, argument validation — leaves NO durable trace, so the SAME
/// command id may be retried safely once the condition clears. Clients derive
/// their command id from the answer coordinates and retain replay context
/// across these errors precisely because of this ordering; reordering a check
/// to after the receipt claim would silently break that retry.
///
/// MUTATION CHECK (executed): move the control-attachment check to AFTER the
/// actor's receipt claim (or claim a receipt before rejecting) and the retry
/// below stops resolving the menu — the second attempt replays a receipt for
/// an answer that never committed.
#[tokio::test]
async fn a_pre_receipt_rejection_leaves_the_command_id_retryable() {
    let (_root, store, hub) = open_hub(None, 8).await;
    let generation = store.worker_generation();
    let session = SessionId::new("pre-receipt-retry");
    append_one(&hub, &session, generation, "pre-receipt-seed").await;

    let menu_id = MenuId::new("pre-receipt-menu");
    let mut opened = menu_opening(&session, &menu_id, generation);
    opened.event_id = EventId::new("pre-receipt-menu-opened");
    opened.run_id = Some(RunId::new("pre-receipt-run"));
    hub.append(&mut [opened.clone()])
        .await
        .expect("menu commits");
    let request_seq = 2;

    let sink = Arc::new(CollectSink::default());
    let connection = hub
        .open_connection(
            capabilities(),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("connection");
    macro_rules! next_response {
        () => {{
            loop {
                match sink.next().await {
                    WireFrame::Response { body, .. } => break body,
                    WireFrame::Event { .. } | WireFrame::AttachCaughtUp { .. } => {}
                    other => panic!("unexpected frame: {other:?}"),
                }
            }
        }};
    }

    // Answer WITHOUT a control attachment: rejected pre-receipt.
    let command_id = CommandId::new("pre-receipt-command");
    connection
        .menu_answer(
            Some(RequestId::new("denied")),
            command_id.clone(),
            session.clone(),
            menu_id.clone(),
            request_seq,
            generation,
            "allow".into(),
            0,
            None,
        )
        .await
        .expect("routes");
    let body = next_response!();
    let ResponseBody::Error { code, .. } = &body else {
        panic!("expected a pre-receipt rejection, got {body:?}");
    };
    assert_eq!(code, ERROR_CODE_CAPABILITY_DENIED);

    // Clear the condition, then retry the SAME command id: it must resolve
    // the menu for real, not replay a receipt that was never committed.
    connection
        .request(
            RequestId::new("pre-receipt-attach"),
            RequestBody::SessionAttach {
                session_id: session.clone(),
                after_seq: 0,
                mode: AttachMode::Control,
                sealed_replay: false,
            },
        )
        .await
        .expect("attach routes");
    let ResponseBody::SessionAttach { .. } = next_response!() else {
        panic!("expected attach response");
    };
    connection
        .menu_answer(
            Some(RequestId::new("retry")),
            command_id,
            session.clone(),
            menu_id,
            request_seq,
            generation,
            "allow".into(),
            0,
            None,
        )
        .await
        .expect("routes");
    let body = next_response!();
    assert!(
        matches!(body, ResponseBody::MenuAnswer { .. }),
        "the same command id resolves after the condition clears, got {body:?}"
    );

    connection.close().await.expect("connection closes");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}
