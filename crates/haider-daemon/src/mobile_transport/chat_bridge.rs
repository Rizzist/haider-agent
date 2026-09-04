//! Canonical daemon-session bridge for the authenticated Android transport.
//!
//! This module deliberately drives a [`HubConnection`] exactly like every
//! other client: create, control-attach, submit, and model/effort selection all
//! pass through the existing receipted RPC machinery.  The mobile TCP actor
//! owns the socket separately, so a turn can continue while that actor serves
//! accessibility, screen, app, and SMS requests from the mobile tool.

use super::{
    ChatCommand, ChatEvent, ChatResponder, MobileChatBridge, MobileChatError, MobileModel,
    MobileProvider, MobileSelection, MobileSessionConfig, TransportState,
};
use crate::{
    AdmissionTicket, MonitorDeliveryReceipt, MonitorDeliverySink, MonitorError,
    MonitorEventPayload, MonitorReport, MonitorReportStatus,
};
use async_trait::async_trait;
use haider_protocol::envelope::RawEnvelope;
use haider_protocol::item::{ItemDelta, ItemEvent, ToolStatus, TurnItem};
use haider_protocol::session::{SessionInteractionModeV1, SessionPermissionOverridesV1};
use haider_protocol::state::RunState;
use haider_protocol::tool::ToolResultStatus;
use haider_protocol::{DeliveryMode, EventPayload};
use haider_rpc::{
    AttachMode, Capability, CapabilitySet, CommandId, ModelDetailWire, ProviderAvailabilityWire,
    ProviderSummaryWire, RequestBody, RequestId, ResponseBody, SnapshotAvailabilityWire, WireFrame,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, RwLock as StdRwLock, Weak};
use tokio::sync::{Mutex, broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;

const HUB_FRAME_CAPACITY: usize = 256;
// Hub responses use the sink's non-blocking path. Event admission keeps one
// slot free so an event burst cannot starve the response that advances us.
const HUB_RESPONSE_FLOOR: usize = 1;
const HUB_EVENT_CAPACITY: usize = 1_024;
const TOOL_SUMMARY_CHARS: usize = 180;
const TOOL_PREVIEW_CHARS: usize = 4_000;

pub(super) struct DaemonMobileChatBridge {
    hub: crate::SessionHub,
    default_model: String,
    instance_id: String,
    next_id: AtomicU64,
    runtime: Mutex<Option<MobileHubRuntime>>,
    attached_session: StdRwLock<Option<haider_protocol::ids::SessionId>>,
    transport: Weak<TransportState>,
}

pub(super) struct MobileMonitorDeliverySink {
    bridge: Weak<DaemonMobileChatBridge>,
}

impl MobileMonitorDeliverySink {
    pub(super) fn new(bridge: Weak<DaemonMobileChatBridge>) -> Self {
        Self { bridge }
    }
}

impl DaemonMobileChatBridge {
    pub(super) fn new(
        hub: crate::SessionHub,
        default_model: String,
        instance_id: String,
        transport: Weak<TransportState>,
    ) -> Self {
        Self {
            hub,
            default_model,
            instance_id,
            next_id: AtomicU64::new(1),
            runtime: Mutex::new(None),
            attached_session: StdRwLock::new(None),
            transport,
        }
    }

    fn attached_session(&self) -> Option<haider_protocol::ids::SessionId> {
        self.attached_session
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn set_attached_session(&self, session: Option<haider_protocol::ids::SessionId>) {
        *self
            .attached_session
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = session;
    }

    fn next_coordinate(&self, kind: &str) -> String {
        let sequence = self.next_id.fetch_add(1, Ordering::Relaxed);
        format!("mobile-{}-{kind}-{sequence}", self.instance_id)
    }

    async fn initialize_runtime(&self) -> Result<MobileHubRuntime, MobileChatError> {
        let (frames, receiver) = mpsc::channel(HUB_FRAME_CAPACITY);
        let sink = Arc::new(MobileHubSink {
            frames,
            waiters: StdMutex::new(VecDeque::new()),
        });
        let connection = self
            .hub
            .open_connection(
                CapabilitySet::from([Capability::View, Capability::Control]),
                sink.clone(),
                crate::accounts::ConnectionTransport::Remote,
            )
            .map_err(MobileChatError::daemon)?;
        let responses = Arc::new(StdMutex::new(HashMap::new()));
        let (events, _) = broadcast::channel(HUB_EVENT_CAPACITY);
        let lag_epoch = Arc::new(AtomicU64::new(0));
        let pump = tokio::spawn(pump_hub_frames(
            receiver,
            sink.clone(),
            responses.clone(),
            events.clone(),
            Arc::clone(&lag_epoch),
        ));
        let mut runtime = MobileHubRuntime {
            hub: self.hub.clone(),
            connection,
            responses,
            events,
            lag_epoch,
            recovered_lag_epoch: 0,
            pump,
            session: None,
            next_request: 1,
        };

        let active_provider = runtime.active_account_provider().await?;
        let catalog = runtime.provider_catalog().await?;
        let (provider, model) = initial_pair(
            &catalog.providers,
            active_provider.as_deref(),
            &self.default_model,
        );
        let cwd = std::env::current_dir()
            .map_err(|error| {
                MobileChatError::internal(format!("cannot resolve mobile workspace: {error}"))
            })?
            .to_string_lossy()
            .into_owned();
        let created = runtime
            .request(RequestBody::SessionCreateWithPermissionOverrides {
                command_id: CommandId::new(self.next_coordinate("create")),
                cwd,
                provider,
                model,
                max_tokens: haider_client::DEFAULT_MAX_TOKENS,
                permission_overrides: Some(SessionPermissionOverridesV1 {
                    allow_mobile: true,
                    ..SessionPermissionOverridesV1::default()
                }),
                cache_policy: None,
                interaction_mode: SessionInteractionModeV1::Interactive,
                ssh_scope: None,
                account_alias: None,
                resolve_provider: false,
                resolve_model: false,
                effort: None,
                fast: None,
            })
            .await?;
        let (session_id, created_seq, worker_generation) = match created.body {
            ResponseBody::SessionCreate {
                session_id,
                created_seq,
                worker_generation,
                ..
            } => (session_id, created_seq, worker_generation),
            body => return Err(unexpected_response("session.create", body)),
        };
        let attached = runtime
            .request(RequestBody::SessionAttach {
                session_id: session_id.clone(),
                after_seq: created_seq,
                mode: AttachMode::Control,
                sealed_replay: false,
            })
            .await?;
        let attachment_id = match attached.body {
            ResponseBody::SessionAttach { attachment_id, .. } => attachment_id,
            body => return Err(unexpected_response("session.attach", body)),
        };
        runtime.session = Some(MobileSession {
            session_id,
            worker_generation,
            _attachment_id: attachment_id,
            last_seq: created_seq,
        });
        Ok(runtime)
    }

    async fn handle_ready_runtime(
        &self,
        runtime: &mut MobileHubRuntime,
        command: ChatCommand,
        responder: ChatResponder,
    ) -> Result<(), MobileChatError> {
        runtime.recover_attachment_if_lagged().await?;
        if responder.is_closed() {
            return Err(connection_closed_error());
        }
        match command {
            ChatCommand::Send { text } => {
                runtime
                    .submit_turn(
                        activate_mobile_use(&text),
                        CommandId::new(self.next_coordinate("turn")),
                        CommandId::new(self.next_coordinate("cancel")),
                        &responder,
                    )
                    .await?;
                responder.send(ChatEvent::Done).await
            }
            ChatCommand::SessionConfig => {
                let config = runtime.session_config().await?;
                responder.send(ChatEvent::SessionConfig(config)).await
            }
            ChatCommand::SelectModel {
                provider,
                model,
                confirm_new_epoch,
            } => {
                let session = runtime.session()?.clone();
                let selected = runtime
                    .request(RequestBody::SessionSelectModel {
                        command_id: CommandId::new(self.next_coordinate("model")),
                        session_id: session.session_id,
                        worker_generation: session.worker_generation,
                        model,
                        provider: Some(provider),
                        confirm_new_epoch,
                    })
                    .await?;
                let worker_generation = match selected.body {
                    ResponseBody::SessionSelectModel {
                        worker_generation, ..
                    } => worker_generation,
                    body => return Err(unexpected_response("session.select_model", body)),
                };
                runtime.session_mut()?.worker_generation = worker_generation;
                let config = runtime.session_config().await?;
                responder.send(ChatEvent::SessionConfig(config)).await
            }
            ChatCommand::SelectEffort {
                effort,
                confirm_new_epoch,
            } => {
                let session = runtime.session()?.clone();
                let selected = runtime
                    .request(RequestBody::SessionSelectEffort {
                        command_id: CommandId::new(self.next_coordinate("effort")),
                        session_id: session.session_id,
                        worker_generation: session.worker_generation,
                        effort,
                        confirm_new_epoch,
                    })
                    .await?;
                let worker_generation = match selected.body {
                    ResponseBody::SessionSelectEffort {
                        worker_generation, ..
                    } => worker_generation,
                    body => return Err(unexpected_response("session.select_effort", body)),
                };
                runtime.session_mut()?.worker_generation = worker_generation;
                let config = runtime.session_config().await?;
                responder.send(ChatEvent::SessionConfig(config)).await
            }
        }
    }
}

#[async_trait]
impl MobileChatBridge for DaemonMobileChatBridge {
    async fn handle(
        &self,
        command: ChatCommand,
        responder: ChatResponder,
    ) -> Result<(), MobileChatError> {
        let close_observer = responder.clone();
        let mut runtime = tokio::select! {
            runtime = self.runtime.lock() => runtime,
            () = close_observer.wait_closed() => return Err(connection_closed_error()),
        };
        if responder.is_closed() {
            return Err(connection_closed_error());
        }
        if runtime.is_none() {
            let initialized = self.initialize_runtime().await?;
            self.set_attached_session(
                initialized
                    .session
                    .as_ref()
                    .map(|session| session.session_id.clone()),
            );
            *runtime = Some(initialized);
        }
        if responder.is_closed() {
            return Err(connection_closed_error());
        }
        let result = {
            let ready = runtime.as_mut().ok_or_else(|| {
                MobileChatError::internal("mobile chat runtime was not initialized")
            })?;
            self.handle_ready_runtime(ready, command, responder).await
        };
        if matches!(&result, Err(error) if runtime_reset_required(error)) {
            runtime.take();
            self.set_attached_session(None);
        }
        result
    }
}

#[async_trait]
impl MonitorDeliverySink for MobileMonitorDeliverySink {
    async fn deliver(
        &self,
        session: &haider_protocol::ids::SessionId,
        report: MonitorReport,
    ) -> Result<MonitorDeliveryReceipt, MonitorError> {
        let bridge = self.bridge.upgrade().ok_or_else(|| {
            MonitorError::Delivery("mobile chat bridge is no longer attached".into())
        })?;
        if &report.session_id != session {
            return Err(MonitorError::Delivery(
                "mobile monitor delivery session does not match report owner".into(),
            ));
        }
        if bridge.attached_session().as_ref() != Some(session) {
            return Err(MonitorError::Delivery(
                "monitor owner is not attached to the mobile chat bridge".into(),
            ));
        }
        let transport = bridge.transport.upgrade().ok_or_else(|| {
            MonitorError::Delivery("mobile chat transport is no longer available".into())
        })?;
        transport
            .send_monitor_chat(monitor_report_chat_text(&report))
            .await
            .map_err(|error| MonitorError::Delivery(error.to_string()))?;
        Ok(MonitorDeliveryReceipt {
            durable: false,
            handed_off: true,
            disposition: "mobile_chat_delta",
        })
    }
}

fn monitor_report_chat_text(report: &MonitorReport) -> String {
    let mut text = match report.status {
        MonitorReportStatus::Matched => format!("Monitor `{}` matched", report.monitor_id),
        MonitorReportStatus::RateLimited => {
            format!(
                "Monitor `{}` stopped after too many matches",
                report.monitor_id
            )
        }
        MonitorReportStatus::TimedOut => format!("Monitor `{}` timed out", report.monitor_id),
    };
    if report.coalesced_count > 1 {
        text.push_str(&format!(" ({} events)", report.coalesced_count));
    }
    text.push_str(".\n");
    for event in &report.events {
        match &event.payload {
            MonitorEventPayload::Sms(sms) => {
                text.push_str(&format!(
                    "\nSMS from {}:\n{}\n",
                    sms.address,
                    truncate_chars(&sms.body, TOOL_PREVIEW_CHARS),
                ));
            }
            MonitorEventPayload::Process { line, .. } | MonitorEventPayload::Cli { line, .. } => {
                text.push_str(&format!(
                    "\nProcess event:\n{}\n",
                    truncate_chars(line, TOOL_PREVIEW_CHARS),
                ));
            }
            MonitorEventPayload::File { payload } => {
                text.push_str(&format!(
                    "\nFile event:\n{}\n",
                    truncate_chars(payload, TOOL_PREVIEW_CHARS),
                ));
            }
            MonitorEventPayload::Poll { payload } => {
                text.push_str(&format!(
                    "\nPoll event:\n{}\n",
                    truncate_chars(payload, TOOL_PREVIEW_CHARS),
                ));
            }
            MonitorEventPayload::Timer { fired_at_ms, .. } => {
                text.push_str(&format!("\nTimer fired at {fired_at_ms} ms.\n"));
            }
        }
    }
    if report.omitted_count > 0 {
        text.push_str(&format!(
            "\n{} additional matching events were omitted.",
            report.omitted_count
        ));
    }
    text
}

struct MobileHubSink {
    frames: mpsc::Sender<WireFrame>,
    waiters: StdMutex<VecDeque<Weak<tokio::sync::Notify>>>,
}

impl MobileHubSink {
    fn waiters(&self) -> std::sync::MutexGuard<'_, VecDeque<Weak<tokio::sync::Notify>>> {
        self.waiters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn discard_dropped_waiters(waiters: &mut VecDeque<Weak<tokio::sync::Notify>>) {
        while waiters
            .front()
            .is_some_and(|waiter| waiter.strong_count() == 0)
        {
            waiters.pop_front();
        }
    }

    fn capacity_released(&self) {
        if self.frames.capacity() <= HUB_RESPONSE_FLOOR {
            return;
        }
        let mut waiters = self.waiters();
        Self::discard_dropped_waiters(&mut waiters);
        if let Some(waiter) = waiters.front().and_then(Weak::upgrade) {
            waiter.notify_one();
        }
    }
}

impl crate::FrameSink for MobileHubSink {
    fn try_send(&self, frame: WireFrame) -> Result<(), crate::FrameSendError> {
        self.frames
            .try_send(frame)
            .map_err(|_| crate::FrameSendError)
    }

    fn offer(
        &self,
        _attachment_id: &haider_rpc::AttachmentId,
        frame: &WireFrame,
    ) -> crate::SendAdmission {
        let mut waiters = self.waiters();
        Self::discard_dropped_waiters(&mut waiters);
        if !waiters.is_empty() {
            return crate::SendAdmission::Busy;
        }
        if self.frames.capacity() <= HUB_RESPONSE_FLOOR {
            return crate::SendAdmission::Busy;
        }
        match self.frames.try_send(frame.clone()) {
            Ok(()) => crate::SendAdmission::Sent,
            Err(mpsc::error::TrySendError::Full(_)) => crate::SendAdmission::Busy,
            Err(mpsc::error::TrySendError::Closed(_)) => crate::SendAdmission::Refused,
        }
    }

    fn offer_ticketed(
        &self,
        _attachment_id: &haider_rpc::AttachmentId,
        frame: &WireFrame,
        ticket: &AdmissionTicket,
    ) -> crate::SendAdmission {
        let admission = {
            let mut waiters = self.waiters();
            Self::discard_dropped_waiters(&mut waiters);
            let owns_head = waiters
                .front()
                .and_then(Weak::upgrade)
                .is_some_and(|head| Arc::ptr_eq(&head, ticket));
            if !owns_head {
                return crate::SendAdmission::Busy;
            }
            if self.frames.capacity() <= HUB_RESPONSE_FLOOR {
                return crate::SendAdmission::Busy;
            }
            match self.frames.try_send(frame.clone()) {
                Ok(()) => {
                    waiters.pop_front();
                    crate::SendAdmission::Sent
                }
                Err(mpsc::error::TrySendError::Full(_)) => crate::SendAdmission::Busy,
                Err(mpsc::error::TrySendError::Closed(_)) => crate::SendAdmission::Refused,
            }
        };
        if admission == crate::SendAdmission::Sent {
            self.capacity_released();
        }
        admission
    }

    fn drain_ticket(&self) -> Option<AdmissionTicket> {
        let ticket = Arc::new(tokio::sync::Notify::new());
        self.waiters().push_back(Arc::downgrade(&ticket));
        if self.frames.capacity() > HUB_RESPONSE_FLOOR {
            ticket.notify_one();
        }
        Some(ticket)
    }

    fn cancel_ticket(&self, ticket: &AdmissionTicket) {
        let mut waiters = self.waiters();
        waiters.retain(|waiter| {
            waiter
                .upgrade()
                .is_some_and(|candidate| !Arc::ptr_eq(&candidate, ticket))
        });
        drop(waiters);
        self.capacity_released();
    }
}

struct MobileHubRuntime {
    hub: crate::SessionHub,
    connection: crate::HubConnection,
    responses: Arc<StdMutex<HashMap<RequestId, oneshot::Sender<ResponseBody>>>>,
    events: broadcast::Sender<HubNotice>,
    lag_epoch: Arc<AtomicU64>,
    recovered_lag_epoch: u64,
    pump: JoinHandle<()>,
    session: Option<MobileSession>,
    next_request: u64,
}

impl Drop for MobileHubRuntime {
    fn drop(&mut self) {
        self.pump.abort();
    }
}

#[derive(Clone)]
struct MobileSession {
    session_id: haider_protocol::ids::SessionId,
    worker_generation: u64,
    _attachment_id: haider_rpc::AttachmentId,
    last_seq: u64,
}

struct MobileResponse {
    body: ResponseBody,
}

#[derive(Clone)]
enum HubNotice {
    Event(RawEnvelope),
    Lagged,
    Failed(String),
}

struct ProviderCatalog {
    providers: Vec<ProviderSummaryWire>,
    revision: u64,
    available: bool,
    unavailable_reason: Option<String>,
}

impl MobileHubRuntime {
    fn session(&self) -> Result<&MobileSession, MobileChatError> {
        self.session
            .as_ref()
            .ok_or_else(|| MobileChatError::internal("mobile session is not attached"))
    }

    fn session_mut(&mut self) -> Result<&mut MobileSession, MobileChatError> {
        self.session
            .as_mut()
            .ok_or_else(|| MobileChatError::internal("mobile session is not attached"))
    }

    fn allocate_request_id(&mut self) -> RequestId {
        let id = self.next_request;
        self.next_request = self.next_request.saturating_add(1);
        RequestId::new(format!("mobile-hub-{id}"))
    }

    async fn request(&mut self, body: RequestBody) -> Result<MobileResponse, MobileChatError> {
        let request_id = self.allocate_request_id();
        let (reply, receiver) = oneshot::channel();
        let pending =
            PendingHubResponse::register(request_id.clone(), reply, Arc::clone(&self.responses));
        self.connection
            .request(request_id, body)
            .await
            .map_err(MobileChatError::daemon)?;
        let body = receiver
            .await
            .map_err(|_| MobileChatError::internal("mobile hub response router closed"))?;
        drop(pending);
        if let ResponseBody::Error {
            code,
            message,
            retryable,
            ..
        } = body
        {
            return Err(MobileChatError::new(code, message, retryable));
        }
        Ok(MobileResponse { body })
    }

    async fn provider_catalog(&mut self) -> Result<ProviderCatalog, MobileChatError> {
        let response = self
            .request(RequestBody::ProviderList { provider: None })
            .await?;
        match response.body {
            ResponseBody::ProviderList {
                providers,
                revision,
                availability,
            } => {
                let (available, unavailable_reason) = match availability {
                    Some(SnapshotAvailabilityWire::Available) | None => (true, None),
                    Some(SnapshotAvailabilityWire::Unavailable { reason }) => (false, Some(reason)),
                    Some(_) => (false, Some("provider catalog state is unknown".into())),
                };
                Ok(ProviderCatalog {
                    providers,
                    revision,
                    available,
                    unavailable_reason,
                })
            }
            body => Err(unexpected_response("provider.list", body)),
        }
    }

    async fn active_account_provider(&mut self) -> Result<Option<String>, MobileChatError> {
        let response = self
            .request(RequestBody::AccountList { provider: None })
            .await?;
        match response.body {
            ResponseBody::AccountList {
                descriptors,
                provider_active,
                ..
            } => Ok(descriptors
                .into_iter()
                .find(|descriptor| descriptor.active)
                .map(|descriptor| descriptor.provider)
                .or_else(|| {
                    provider_active
                        .into_iter()
                        .next()
                        .map(|active| active.provider)
                })),
            body => Err(unexpected_response("account.list", body)),
        }
    }

    async fn session_config(&mut self) -> Result<MobileSessionConfig, MobileChatError> {
        let catalog = self.provider_catalog().await?;
        let session_id = self.session()?.session_id.clone();
        let metadata = self
            .hub
            .session_metadata(&session_id)
            .await
            .map_err(|error| MobileChatError::internal(error.message))?
            .ok_or_else(|| MobileChatError::internal("mobile session metadata is missing"))?;
        Ok(MobileSessionConfig {
            catalog_revision: catalog.revision,
            catalog_available: catalog.available,
            unavailable_reason: catalog.unavailable_reason,
            current: MobileSelection {
                session_id: session_id.to_string(),
                provider: metadata.provider,
                model: metadata.model,
                effort: metadata.effort,
            },
            providers: catalog.providers.into_iter().map(mobile_provider).collect(),
        })
    }

    async fn reattach(&mut self) -> Result<(), MobileChatError> {
        let session = self.session()?.clone();
        let attached = self
            .request(RequestBody::SessionAttach {
                session_id: session.session_id,
                after_seq: session.last_seq,
                mode: AttachMode::Control,
                sealed_replay: false,
            })
            .await?;
        let attachment_id = match attached.body {
            ResponseBody::SessionAttach { attachment_id, .. } => attachment_id,
            body => return Err(unexpected_response("session.reattach", body)),
        };
        self.session_mut()?._attachment_id = attachment_id;
        Ok(())
    }

    async fn recover_attachment_if_lagged(&mut self) -> Result<(), MobileChatError> {
        let observed = self.lag_epoch.load(Ordering::Acquire);
        if observed <= self.recovered_lag_epoch {
            return Ok(());
        }
        self.reattach().await?;
        self.recovered_lag_epoch = observed;
        Ok(())
    }

    async fn cancel_run(
        &mut self,
        command_id: CommandId,
        session_id: haider_protocol::ids::SessionId,
        worker_generation: u64,
        run_id: haider_protocol::ids::RunId,
    ) -> Result<(), MobileChatError> {
        let cancellation = self
            .request(RequestBody::TurnCancel {
                command_id,
                session_id,
                worker_generation,
                run_id,
            })
            .await;
        match cancellation {
            Ok(MobileResponse {
                body: ResponseBody::TurnCancel { .. },
            }) => Ok(()),
            Ok(response) => Err(unexpected_response("turn.cancel", response.body)),
            Err(error) if error.code == "run_not_active" => Ok(()),
            Err(error) => Err(error),
        }
    }

    async fn submit_turn(
        &mut self,
        text: String,
        command_id: CommandId,
        parked_cancel_command_id: CommandId,
        responder: &ChatResponder,
    ) -> Result<(), MobileChatError> {
        let session = self.session()?.clone();
        let session_id = session.session_id.clone();
        let mut events = self.events.subscribe();
        let accepted = self
            .request(RequestBody::TurnSubmit {
                command_id,
                session_id: session_id.clone(),
                worker_generation: session.worker_generation,
                text,
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            })
            .await?;
        let (run_id, worker_generation) = match accepted.body {
            ResponseBody::TurnSubmit {
                run_id,
                worker_generation,
                ..
            }
            | ResponseBody::TurnSubmitOnBranch {
                run_id,
                worker_generation,
                ..
            } => (run_id, worker_generation),
            body => return Err(unexpected_response("turn.submit", body)),
        };
        self.session_mut()?.worker_generation = worker_generation;
        if responder.is_closed() {
            self.cancel_run(
                parked_cancel_command_id,
                session_id,
                worker_generation,
                run_id,
            )
            .await?;
            return Err(connection_closed_error());
        }
        let mut projection = TurnProjection::default();
        loop {
            let notice = tokio::select! {
                () = responder.wait_closed() => {
                    self.cancel_run(
                        parked_cancel_command_id.clone(),
                        session_id.clone(),
                        worker_generation,
                        run_id.clone(),
                    )
                    .await?;
                    return Err(connection_closed_error());
                }
                notice = events.recv() => notice,
            };
            let envelope = match notice {
                Ok(HubNotice::Event(envelope)) => envelope,
                Ok(HubNotice::Lagged) => {
                    self.recover_attachment_if_lagged().await?;
                    self.cancel_run(
                        parked_cancel_command_id.clone(),
                        session_id.clone(),
                        worker_generation,
                        run_id.clone(),
                    )
                    .await?;
                    return Err(MobileChatError::new(
                        "stream_lagged",
                        "mobile session attachment lagged; retry the turn after reconnecting",
                        true,
                    ));
                }
                Ok(HubNotice::Failed(message)) => {
                    return Err(MobileChatError::internal(message));
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    self.cancel_run(
                        parked_cancel_command_id.clone(),
                        session_id.clone(),
                        worker_generation,
                        run_id.clone(),
                    )
                    .await?;
                    return Err(MobileChatError::new(
                        "stream_lagged",
                        format!("mobile turn event router lagged by {skipped} frames"),
                        true,
                    ));
                }
                Err(broadcast::error::RecvError::Closed) => {
                    return Err(MobileChatError::internal("mobile turn event router closed"));
                }
            };
            let last_seq = self.session()?.last_seq.max(envelope.seq);
            self.session_mut()?.last_seq = last_seq;
            if envelope.run_id.as_ref() != Some(&run_id) {
                continue;
            }
            let terminal = match projection.apply(envelope, responder).await {
                Ok(terminal) => terminal,
                Err(response_error) => {
                    self.cancel_run(
                        parked_cancel_command_id.clone(),
                        session_id.clone(),
                        worker_generation,
                        run_id.clone(),
                    )
                    .await?;
                    return Err(response_error);
                }
            };
            if terminal {
                if projection.parked {
                    self.cancel_run(
                        parked_cancel_command_id,
                        session_id,
                        worker_generation,
                        run_id,
                    )
                    .await?;
                }
                return projection.terminal_result();
            }
        }
    }
}

async fn pump_hub_frames(
    mut frames: mpsc::Receiver<WireFrame>,
    sink: Arc<MobileHubSink>,
    responses: Arc<StdMutex<HashMap<RequestId, oneshot::Sender<ResponseBody>>>>,
    events: broadcast::Sender<HubNotice>,
    lag_epoch: Arc<AtomicU64>,
) {
    while let Some(frame) = frames.recv().await {
        sink.capacity_released();
        match frame {
            WireFrame::Response { request_id, body } => {
                if let Some(reply) = response_map(&responses).remove(&request_id) {
                    let _ = reply.send(body);
                }
            }
            WireFrame::Event { envelope, .. } => {
                let _ = events.send(HubNotice::Event(envelope));
            }
            WireFrame::Lagged { .. } => {
                lag_epoch.fetch_add(1, Ordering::AcqRel);
                let _ = events.send(HubNotice::Lagged);
            }
            WireFrame::ProtocolError(error) => {
                let _ = events.send(HubNotice::Failed(error.to_string()));
            }
            WireFrame::ServerDraining { reason, .. } => {
                let _ = events.send(HubNotice::Failed(format!("daemon is draining: {reason}")));
            }
            _ => {}
        }
    }
    response_map(&responses).clear();
}

struct PendingHubResponse {
    request_id: RequestId,
    responses: Arc<StdMutex<HashMap<RequestId, oneshot::Sender<ResponseBody>>>>,
}

impl PendingHubResponse {
    fn register(
        request_id: RequestId,
        reply: oneshot::Sender<ResponseBody>,
        responses: Arc<StdMutex<HashMap<RequestId, oneshot::Sender<ResponseBody>>>>,
    ) -> Self {
        response_map(&responses).insert(request_id.clone(), reply);
        Self {
            request_id,
            responses,
        }
    }
}

impl Drop for PendingHubResponse {
    fn drop(&mut self) {
        response_map(&self.responses).remove(&self.request_id);
    }
}

fn response_map(
    responses: &StdMutex<HashMap<RequestId, oneshot::Sender<ResponseBody>>>,
) -> std::sync::MutexGuard<'_, HashMap<RequestId, oneshot::Sender<ResponseBody>>> {
    responses
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Default)]
struct TurnProjection {
    item_text: HashMap<String, haider_protocol::reply::ReplyText>,
    tools: HashMap<String, ToolProjection>,
    failure: Option<MobileChatError>,
    terminal: Option<RunState>,
    parked: bool,
}

#[derive(Clone)]
struct ToolProjection {
    name: String,
    summary: String,
    status: &'static str,
    result: Option<String>,
}

impl TurnProjection {
    async fn apply(
        &mut self,
        envelope: RawEnvelope,
        responder: &ChatResponder,
    ) -> Result<bool, MobileChatError> {
        let Ok(payload) = envelope.payload.decode_event() else {
            return Ok(false);
        };
        match payload {
            EventPayload::Item(item) => self.apply_item(item, responder).await?,
            EventPayload::ToolResult { call_id, result } => {
                let tool = self.tools.entry(call_id.clone()).or_insert(ToolProjection {
                    name: "tool".into(),
                    summary: String::new(),
                    status: "unknown",
                    result: None,
                });
                tool.status = result_status(result.status);
                tool.result = Some(truncate_chars(&result.preview, TOOL_PREVIEW_CHARS));
                responder
                    .send(ChatEvent::Tool {
                        call_id,
                        name: tool.name.clone(),
                        summary: tool.summary.clone(),
                        status: tool.status,
                        result: tool.result.clone(),
                    })
                    .await?;
            }
            EventPayload::RunFailed {
                code,
                message,
                retryable,
                ..
            } => {
                self.failure = Some(MobileChatError::new(code.as_str(), message, retryable));
            }
            EventPayload::RunState(state) => {
                if let Some(status) = run_status(&state) {
                    responder.send(ChatEvent::Status { text: status }).await?;
                }
                match &state {
                    RunState::InputRequired { .. } => {
                        self.failure = Some(MobileChatError::new(
                            "input_required",
                            "This turn needs interactive input that the mobile chat cannot provide",
                            false,
                        ));
                        self.parked = true;
                        return Ok(true);
                    }
                    RunState::PermissionRequired { .. } => {
                        self.failure = Some(MobileChatError::new(
                            "permission_required",
                            "This turn requested a permission outside the mobile session grant",
                            false,
                        ));
                        self.parked = true;
                        return Ok(true);
                    }
                    _ => {}
                }
                if state.is_terminal() {
                    self.terminal = Some(state);
                    return Ok(true);
                }
            }
            _ => {}
        }
        Ok(false)
    }

    async fn apply_item(
        &mut self,
        event: ItemEvent,
        responder: &ChatResponder,
    ) -> Result<(), MobileChatError> {
        match event {
            ItemEvent::Started { item_id, item } => {
                self.apply_item_value(item_id.as_str(), item, false, responder)
                    .await
            }
            ItemEvent::Delta { item_id, delta } => {
                let key = item_id.as_str().to_owned();
                match delta {
                    ItemDelta::Text { text } => {
                        self.note_item_delta(key, &text);
                        responder
                            .send(ChatEvent::Delta {
                                text: text.to_owned_string(),
                                segment: "answer",
                            })
                            .await
                    }
                    ItemDelta::Reasoning { text } => {
                        self.note_item_delta(key, &text);
                        responder
                            .send(ChatEvent::Delta {
                                text: text.to_owned_string(),
                                segment: "thinking",
                            })
                            .await
                    }
                    ItemDelta::ToolArgs { fragment } => {
                        if let Some(tool) = self.tools.get_mut(item_id.as_str()) {
                            tool.summary = truncate_chars(
                                &format!("{}{}", tool.summary, fragment),
                                TOOL_SUMMARY_CHARS,
                            );
                        }
                        Ok(())
                    }
                    ItemDelta::CommandOutput { .. } => Ok(()),
                }
            }
            ItemEvent::Completed { item_id, item } => {
                self.apply_item_value(item_id.as_str(), item, true, responder)
                    .await
            }
        }
    }

    async fn apply_item_value(
        &mut self,
        item_id: &str,
        item: TurnItem,
        completed: bool,
        responder: &ChatResponder,
    ) -> Result<(), MobileChatError> {
        match item {
            TurnItem::AgentMessage { text } => {
                self.send_unseen_text(item_id, text, "answer", responder)
                    .await
            }
            TurnItem::IncompleteAgentMessage { text, .. } => {
                self.send_unseen_text(item_id, text, "answer", responder)
                    .await
            }
            TurnItem::Reasoning { summary } => {
                self.send_unseen_text(item_id, summary, "thinking", responder)
                    .await
            }
            TurnItem::ToolCall {
                call_id,
                name,
                args,
                status,
            } => {
                let status = if completed {
                    tool_status(status)
                } else {
                    "running"
                };
                let tool = ToolProjection {
                    name,
                    summary: truncate_chars(&compact_json(&args), TOOL_SUMMARY_CHARS),
                    status,
                    result: self
                        .tools
                        .get(&call_id)
                        .and_then(|tool| tool.result.clone()),
                };
                self.tools.insert(call_id.clone(), tool.clone());
                self.tools.insert(item_id.to_owned(), tool.clone());
                responder
                    .send(ChatEvent::Tool {
                        call_id,
                        name: tool.name,
                        summary: tool.summary,
                        status: tool.status,
                        result: tool.result,
                    })
                    .await
            }
            TurnItem::CommandExecution {
                call_id,
                command,
                status,
                ..
            } => {
                let status = if completed {
                    tool_status(status)
                } else {
                    "running"
                };
                let tool = ToolProjection {
                    name: "shell".into(),
                    summary: truncate_chars(&command, TOOL_SUMMARY_CHARS),
                    status,
                    result: self
                        .tools
                        .get(&call_id)
                        .and_then(|tool| tool.result.clone()),
                };
                self.tools.insert(call_id.clone(), tool.clone());
                responder
                    .send(ChatEvent::Tool {
                        call_id,
                        name: tool.name,
                        summary: tool.summary,
                        status: tool.status,
                        result: tool.result,
                    })
                    .await
            }
            TurnItem::Refusal { reason } => {
                self.send_unseen_text(item_id, reason.into(), "answer", responder)
                    .await
            }
            _ => Ok(()),
        }
    }

    async fn send_unseen_text(
        &mut self,
        item_id: &str,
        complete: haider_protocol::reply::ReplyText,
        segment: &'static str,
        responder: &ChatResponder,
    ) -> Result<(), MobileChatError> {
        let seen = self.item_text.get(item_id);
        let offset = seen
            .filter(|seen| seen.is_prefix_of(&complete))
            .map_or(0, |seen| seen.len());
        let delta = complete
            .slice(offset..complete.len())
            .unwrap_or_else(|| complete.clone())
            .to_owned_string();
        self.item_text.insert(item_id.to_owned(), complete);
        if delta.is_empty() {
            Ok(())
        } else {
            responder
                .send(ChatEvent::Delta {
                    text: delta,
                    segment,
                })
                .await
        }
    }

    fn note_item_delta(&mut self, item_id: String, delta: &haider_protocol::reply::ReplyText) {
        let Some(previous) = self.item_text.remove(&item_id) else {
            self.item_text.insert(item_id, delta.clone());
            return;
        };
        if let Some(joined) = previous.try_join(delta) {
            self.item_text.insert(item_id, joined);
            return;
        }
        let mut writer = haider_protocol::reply::ReplyArenaWriter::new();
        let _ = writer.append_shared(&previous);
        let _ = writer.append_shared(delta);
        self.item_text.insert(item_id, writer.seal());
    }

    fn terminal_result(&mut self) -> Result<(), MobileChatError> {
        if self.parked {
            return Err(self.failure.take().unwrap_or_else(|| {
                MobileChatError::new("turn_parked", "The daemon turn is waiting for input", false)
            }));
        }
        match self.terminal.take() {
            Some(RunState::Done) => Ok(()),
            Some(RunState::Errored) => Err(self.failure.take().unwrap_or_else(|| {
                MobileChatError::new("turn_failed", "The daemon turn failed", false)
            })),
            Some(RunState::Cancelled) => Err(MobileChatError::new(
                "turn_cancelled",
                "The daemon turn was cancelled",
                false,
            )),
            _ => Err(MobileChatError::internal(
                "mobile turn ended without a recognized terminal state",
            )),
        }
    }
}

fn initial_pair(
    providers: &[ProviderSummaryWire],
    active_provider: Option<&str>,
    configured_model: &str,
) -> (String, String) {
    let usable = |provider: &&ProviderSummaryWire| {
        provider.enabled && matches!(provider.availability, ProviderAvailabilityWire::Available)
    };
    let selected = providers
        .iter()
        .filter(usable)
        .find(|provider| active_provider == Some(provider.provider.as_str()))
        .or_else(|| {
            providers.iter().filter(usable).find(|provider| {
                provider.default_model.as_deref() == Some(configured_model)
                    || provider_has_model(provider, configured_model)
            })
        })
        .or_else(|| {
            providers
                .iter()
                .filter(usable)
                .find(|provider| provider.provider == haider_client::DEFAULT_PROVIDER)
        })
        .or_else(|| {
            providers
                .iter()
                .filter(usable)
                .find(|provider| provider.default_model.is_some())
        })
        .or_else(|| providers.iter().find(usable));
    selected.map_or_else(
        || {
            (
                haider_client::DEFAULT_PROVIDER.to_owned(),
                configured_model.to_owned(),
            )
        },
        |provider| {
            let model = provider
                .default_model
                .clone()
                .or_else(|| {
                    provider_has_model(provider, configured_model)
                        .then(|| configured_model.to_owned())
                })
                .or_else(|| provider.models.first().cloned())
                .or_else(|| {
                    provider
                        .model_details
                        .first()
                        .map(|detail| detail.name.clone())
                })
                .unwrap_or_else(|| configured_model.to_owned());
            (provider.provider.clone(), model)
        },
    )
}

fn provider_has_model(provider: &ProviderSummaryWire, model: &str) -> bool {
    provider.models.iter().any(|candidate| candidate == model)
        || provider
            .model_details
            .iter()
            .any(|detail| detail.name == model)
}

fn mobile_provider(provider: ProviderSummaryWire) -> MobileProvider {
    let mut seen = HashSet::new();
    let mut models = Vec::new();
    for name in provider
        .models
        .iter()
        .chain(provider.model_details.iter().map(|detail| &detail.name))
    {
        if !seen.insert(name.clone()) {
            continue;
        }
        let detail = provider
            .model_details
            .iter()
            .find(|detail| detail.name == *name);
        models.push(mobile_model(name.clone(), detail));
    }
    let (availability, availability_reason) = match provider.availability {
        ProviderAvailabilityWire::Available => ("available", provider.availability_reason),
        ProviderAvailabilityWire::Unavailable => ("unavailable", provider.availability_reason),
        _ => ("unknown", provider.availability_reason),
    };
    MobileProvider {
        id: provider.provider,
        enabled: provider.enabled,
        availability,
        availability_reason,
        default_model: provider.default_model,
        models,
    }
}

fn mobile_model(name: String, detail: Option<&ModelDetailWire>) -> MobileModel {
    MobileModel {
        id: name,
        context_window: detail.and_then(|detail| detail.context_window),
        supported_efforts: detail
            .map(|detail| detail.supported_efforts.clone())
            .unwrap_or_default(),
        default_effort: detail.and_then(|detail| detail.default_effort.clone()),
    }
}

fn activate_mobile_use(text: &str) -> String {
    if crate::worker::explicit_mobile_use_intent(text) {
        text.to_owned()
    } else {
        format!("mobile-use\n{text}")
    }
}

fn compact_json(value: &serde_json::Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "{…}".into())
}

fn truncate_chars(text: &str, maximum: usize) -> String {
    let mut chars = text.chars();
    let mut truncated = chars.by_ref().take(maximum).collect::<String>();
    if chars.next().is_some() {
        truncated.push('…');
    }
    truncated
}

fn tool_status(status: ToolStatus) -> &'static str {
    match status {
        ToolStatus::Pending | ToolStatus::InProgress => "running",
        ToolStatus::Completed => "completed",
        ToolStatus::Failed => "failed",
        ToolStatus::Cancelled => "cancelled",
        ToolStatus::Rejected => "rejected",
        ToolStatus::Conflict => "conflict",
        ToolStatus::Unknown => "unknown",
    }
}

fn result_status(status: ToolResultStatus) -> &'static str {
    match status {
        ToolResultStatus::Completed => "completed",
        ToolResultStatus::Rejected => "rejected",
        ToolResultStatus::Conflict => "conflict",
        ToolResultStatus::Failed => "failed",
        ToolResultStatus::Cancelled => "cancelled",
        ToolResultStatus::Unknown => "unknown",
    }
}

fn run_status(state: &RunState) -> Option<String> {
    match state {
        RunState::Queued => Some("queued…".into()),
        RunState::Thinking => Some("thinking…".into()),
        RunState::Streaming => Some("responding…".into()),
        RunState::RunningTool => Some("using tools…".into()),
        RunState::Waiting { .. } => Some("waiting…".into()),
        RunState::Retrying { attempt, max, .. } => {
            Some(format!("retrying provider · {attempt}/{max}…"))
        }
        RunState::InputRequired { .. } | RunState::PermissionRequired { .. } => {
            Some("needs input…".into())
        }
        RunState::Compacting => Some("compacting context…".into()),
        RunState::Verifying { .. } => Some("verifying…".into()),
        RunState::Concluding => Some("concluding…".into()),
        RunState::EffectOutcomeUnknown => Some("checking an uncertain tool outcome…".into()),
        RunState::Cancelling => Some("cancelling…".into()),
        RunState::Done | RunState::Errored | RunState::Cancelled => None,
    }
}

fn unexpected_response(operation: &str, body: ResponseBody) -> MobileChatError {
    MobileChatError::internal(format!(
        "daemon returned an unexpected response to {operation}: {body:?}"
    ))
}

fn runtime_reset_required(error: &MobileChatError) -> bool {
    matches!(
        error.code.as_str(),
        "internal" | "session_not_found" | "stale_generation"
    )
}

fn connection_closed_error() -> MobileChatError {
    MobileChatError::new(
        "connection_closed",
        "The mobile connection closed before the request could complete",
        true,
    )
}

#[cfg(test)]
#[path = "chat_bridge_tests.rs"]
mod chat_bridge_tests;
