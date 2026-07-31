//! Private session-hub accounting tests.

#![allow(clippy::expect_used)]

use super::*;
use haider_protocol::EventPayload;
use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
use haider_protocol::ids::{AgentId, BranchId, EventId, ItemId, MenuId, RunId};
use haider_protocol::item::{ItemEvent, ToolStatus, TurnItem};
use haider_protocol::menu::{Menu, MenuCloseReason, MenuKind, MenuOption, MenuScope};
use haider_protocol::state::RunState;
use haider_store::{SessionCreateCommand, TurnAcceptCommand};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use tokio::sync::{Notify, mpsc, oneshot, watch};

fn provider_summary(provider: &str) -> haider_rpc::ProviderSummaryWire {
    haider_rpc::ProviderSummaryWire {
        provider: provider.to_owned(),
        api_family: haider_rpc::ProviderApiFamilyWire::Unknown,
        endpoint: None,
        models: Vec::new(),
        model_details: Vec::new(),
        auth_methods: Vec::new(),
        availability: haider_rpc::ProviderAvailabilityWire::Unknown,
        availability_reason: None,
        default_model: None,
        enabled: true,
    }
}

/// The optional `provider.list` coordinate filters the production snapshot
/// projection without probing or rebuilding provider data.
///
/// MUTATION CHECK: delete the predicate from
/// `rpc::filter_provider_summaries`. Expected runtime failure: both fixture
/// providers are returned instead of only `openai`.
#[test]
fn provider_list_filter_is_applied_to_the_owned_snapshot_projection() {
    let providers = vec![provider_summary("anthropic"), provider_summary("openai")];
    let filtered = rpc::filter_provider_summaries(providers, Some("openai"));
    assert_eq!(
        filtered
            .iter()
            .map(|summary| summary.provider.as_str())
            .collect::<Vec<_>>(),
        vec!["openai"]
    );
}

#[derive(Default)]
struct CapturingFrameSink(Mutex<Vec<WireFrame>>);

impl FrameSink for CapturingFrameSink {
    fn try_send(&self, frame: WireFrame) -> Result<(), FrameSendError> {
        self.0.lock().expect("capturing sink").push(frame);
        Ok(())
    }
}

/// The new refresh method requires Control and hands a correlation-owned job
/// to the bounded account actor mailbox.
///
/// MUTATION CHECK: authorize `RequestBody::ProviderModelsRefresh` with
/// `Operation::View` instead of `Operation::Control`. Expected runtime
/// failure: the view-only request enters the actor mailbox and its sink has
/// no `capability_denied` response.
/// Verified by revert on 2026-07-30.
#[tokio::test]
async fn provider_models_refresh_requires_control_and_hands_off_correlation() {
    let root = tempfile::tempdir().expect("temp store");
    let store = SqliteStoreHandle::open(root.path())
        .await
        .expect("store opens");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub opens");
    let (commands, mut actor_mailbox) = mpsc::channel(2);
    hub.install_accounts(crate::accounts::AccountsFacade {
        login: Some(commands),
        oauth: None,
        snapshot: Arc::new(Mutex::new(Vec::new())),
        management: crate::accounts::ManagementSnapshot::new(0, Vec::new(), Vec::new()),
        vault_supported: true,
    })
    .expect("install accounts");

    let view_sink = Arc::new(CapturingFrameSink::default());
    let view = hub
        .open_connection(
            std::collections::BTreeSet::from([haider_rpc::Capability::View]),
            view_sink.clone(),
            crate::accounts::ConnectionTransport::LocalSameUid,
        )
        .expect("view connection");
    view.request(
        haider_rpc::RequestId::new("view-refresh"),
        haider_rpc::RequestBody::ProviderModelsRefresh {
            provider: "openai-oauth".to_owned(),
        },
    )
    .await
    .expect("view rejection");
    assert!(actor_mailbox.try_recv().is_err());
    assert!(matches!(
        view_sink.0.lock().expect("view frames").as_slice(),
        [WireFrame::Response {
            body: haider_rpc::ResponseBody::Error { code, .. },
            ..
        }] if code == haider_rpc::ERROR_CODE_CAPABILITY_DENIED
    ));

    let control_sink = Arc::new(CapturingFrameSink::default());
    let control = hub
        .open_connection(
            std::collections::BTreeSet::from([
                haider_rpc::Capability::View,
                haider_rpc::Capability::Control,
            ]),
            control_sink.clone(),
            crate::accounts::ConnectionTransport::LocalSameUid,
        )
        .expect("control connection");
    control
        .request(
            haider_rpc::RequestId::new("control-refresh"),
            haider_rpc::RequestBody::ProviderModelsRefresh {
                provider: "openai-oauth".to_owned(),
            },
        )
        .await
        .expect("control handoff");
    let command = actor_mailbox.recv().await.expect("owned actor job");
    let crate::accounts::AccountCommand::RefreshProviderModels {
        provider,
        completed,
    } = command
    else {
        panic!("unexpected actor command");
    };
    assert_eq!(provider, "openai-oauth");
    completed
        .sink
        .try_send(WireFrame::Response {
            request_id: completed.request_id,
            body: haider_rpc::ResponseBody::ProviderModelsRefresh {
                provider: provider_summary("openai-oauth"),
                revision: 4,
            },
        })
        .expect("correlated response");
    assert!(matches!(
        control_sink.0.lock().expect("control frames").as_slice(),
        [WireFrame::Response {
            request_id,
            body: haider_rpc::ResponseBody::ProviderModelsRefresh { revision: 4, .. },
        }] if request_id.as_str() == "control-refresh"
    ));

    drop(view);
    drop(control);
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

fn run_state_envelope(
    session_id: &SessionId,
    run_id: &RunId,
    generation: u64,
    event_id: &str,
    state: RunState,
) -> RawEnvelope {
    EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(event_id),
        seq: 0,
        session_id: session_id.clone(),
        branch_id: None,
        run_id: Some(run_id.clone()),
        agent_id: None,
        device_id: DeviceId::new("terminal-truth-test"),
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
        payload: serde_json::to_value(EventPayload::RunState(state)).expect("state serializes"),
    }
}

fn run_payload_envelope(
    session_id: &SessionId,
    run_id: &RunId,
    generation: u64,
    event_id: &str,
    payload: EventPayload,
) -> RawEnvelope {
    let mut envelope =
        run_state_envelope(session_id, run_id, generation, event_id, RunState::Queued);
    envelope.payload = serde_json::to_value(payload).expect("payload serializes");
    envelope
}

fn create_command(session_id: &SessionId, suffix: &str) -> SessionCreateCommand {
    SessionCreateCommand {
        command_id: format!("create-{suffix}"),
        request_digest: format!("create-digest-{suffix}"),
        request_json: format!(r#"{{"fixture":"{suffix}"}}"#),
        session_id: session_id.clone(),
        cwd: "/tmp".into(),
        provider: "fake".into(),
        model: "fake-v1".into(),
        max_tokens: 4_096,
        system_prompt_version: "test-system-v1".into(),
        event_id: EventId::new(format!("created-{suffix}")),
        device_id: DeviceId::new("worker-law-test"),
    }
}

fn accept_command(
    session_id: &SessionId,
    run_id: &RunId,
    generation: u64,
    suffix: &str,
) -> TurnAcceptCommand {
    TurnAcceptCommand {
        command_id: format!("accept-{suffix}"),
        request_digest: format!("accept-digest-{suffix}"),
        request_json: format!(r#"{{"fixture":"{suffix}"}}"#),
        session_id: session_id.clone(),
        worker_generation: generation,
        run_id: run_id.clone(),
        text: "fixture turn".into(),
        attachments: Vec::new(),
        mode: haider_protocol::DeliveryMode::Queue,
        queued_event_id: EventId::new(format!("queued-{suffix}")),
        user_event_id: EventId::new(format!("user-{suffix}")),
        active_event_id: EventId::new(format!("active-{suffix}")),
        device_id: DeviceId::new("worker-law-test"),
    }
}

/// Exact P1-3 schedule: actor FIFO commits CancelTurn first, then receives an
/// already-queued worker Done. The durable transition gate must reject Done
/// and allow only the cancellation terminal.
///
/// MUTATION CHECK: route `ActorCommand::WorkerAppend` through ordinary
/// `store.append` instead of `append_worker`. Expected failure: Done commits.
/// Verified by revert in W3c1.1.
#[tokio::test]
async fn cancelling_committed_before_worker_done_rejects_done() {
    let root = tempfile::tempdir().expect("temp store");
    let store = SqliteStoreHandle::open(root.path())
        .await
        .expect("store opens");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub opens");
    let session_id = SessionId::new("cancel-before-done");
    let run_id = RunId::new("cancel-before-done-run");
    let generation = store.worker_generation();
    let mut queued = [run_state_envelope(
        &session_id,
        &run_id,
        generation,
        "cancel-before-done-queued",
        RunState::Queued,
    )];
    hub.append(&mut queued).await.expect("queued prefix");
    let lease = hub
        .acquire_worker_lease(session_id.clone())
        .await
        .expect("lease");
    let actor = hub
        .existing_actor(&session_id)
        .expect("actor lookup")
        .expect("actor exists");
    let (cancel_completed, cancel_response) = oneshot::channel();
    actor
        .commands
        .send(ActorCommand::CancelTurn {
            command: TurnCancelCommand {
                command_id: "cancel-before-done-command".into(),
                request_digest: "cancel-before-done-digest".into(),
                request_json: "{}".into(),
                session_id: session_id.clone(),
                worker_generation: generation,
                run_id: run_id.clone(),
                cancelling_event_id: EventId::new("cancel-before-done-cancelling"),
                device_id: DeviceId::new("terminal-truth-test"),
            },
            completed: cancel_completed,
        })
        .await
        .expect("cancel queues");
    let (done_completed, done_response) = oneshot::channel();
    actor
        .commands
        .send(ActorCommand::WorkerAppend {
            lease_id: lease.lease_id.clone(),
            envelopes: vec![run_state_envelope(
                &session_id,
                &run_id,
                generation,
                "cancel-before-done-done",
                RunState::Done,
            )],
            completed: done_completed,
        })
        .await
        .expect("done queues behind cancel");

    assert!(matches!(
        cancel_response.await.expect("cancel response"),
        Ok(TurnCancelOutcome::Committed {
            envelope: Some(_),
            ..
        })
    ));
    let error = done_response
        .await
        .expect("done response")
        .expect_err("Done is rejected");
    assert_eq!(error.code, ErrorCode::RunNotActive);

    let mut cancelled = [run_state_envelope(
        &session_id,
        &run_id,
        generation,
        "cancel-before-done-cancelled",
        RunState::Cancelled,
    )];
    StoreHandle::append(&lease, &mut cancelled)
        .await
        .expect("Cancelled commits");
    let history = store.read(&session_id, 0, 16).await.expect("history");
    let states = history
        .into_iter()
        .filter_map(|envelope| serde_json::from_value::<EventPayload>(envelope.payload).ok())
        .filter_map(|payload| match payload {
            EventPayload::RunState(state) => Some(state),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        states,
        vec![RunState::Queued, RunState::Cancelling, RunState::Cancelled]
    );
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// Exact D2g handoff schedule: acceptance is durably committed, the external
/// manager gate closes and Shutdown is enqueued before the post-commit hint
/// can hand off. The hint receives typed Busy, while the drain sweep still
/// terminalizes the accepted run in this generation.
///
/// MUTATION CHECK: remove the post-supervisor durable drain sweep. Expected
/// failure: the run remains Queued after manager shutdown.
#[tokio::test]
async fn accepted_commit_then_shutdown_before_handoff_is_swept_terminal() {
    let root = tempfile::tempdir().expect("temp store");
    let store = SqliteStoreHandle::open(root.path())
        .await
        .expect("store opens");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub opens");
    let session_id = SessionId::new("drain-before-handoff");
    let run_id = RunId::new("drain-before-handoff-run");
    hub.create_session(create_command(&session_id, "drain-before-handoff"))
        .await
        .expect("typed session commits");
    let accepted = match hub
        .accept_turn(accept_command(
            &session_id,
            &run_id,
            store.worker_generation(),
            "drain-before-handoff",
        ))
        .await
        .expect("acceptance commits")
    {
        haider_store::TurnAcceptOutcome::Committed { accepted, .. }
        | haider_store::TurnAcceptOutcome::IdempotentReplay { accepted } => accepted,
    };
    let manager = crate::worker::WorkerManager::start(
        hub.clone(),
        crate::worker::WorkerDependencies::unconfigured_for_tests(),
        false,
    );
    let handle = manager.handle();
    handle.begin_draining();
    let shutdown = tokio::spawn(manager.shutdown());
    tokio::task::yield_now().await;
    let error = handle
        .submit(accepted)
        .await
        .expect_err("post-gate hint is rejected");
    assert_eq!(error.code, ErrorCode::Busy);
    shutdown
        .await
        .expect("manager task joins")
        .expect("drain sweep succeeds");

    let history = haider_core::StoreHandle::read(&store, &session_id, 0, 64)
        .await
        .expect("history reads");
    assert!(history.iter().any(|envelope| {
        envelope.run_id.as_ref() == Some(&run_id)
            && serde_json::from_value::<EventPayload>(envelope.payload.clone())
                .is_ok_and(|payload| payload == EventPayload::RunState(RunState::Cancelled))
    }));
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// Exact D1g/D3-4 interlock: a supervisor panic is observed only after
/// durable Cancelling, with one open tool item and menu. Exit terminalization
/// must use cancellation-shaped closure before Cancelled, then eviction may
/// safely permit a fresh incarnation.
///
/// MUTATION CHECK: restore the Cancelling fast-path that appends only
/// Cancelled. Expected failure: the item and menu remain open.
#[tokio::test]
async fn panic_exit_after_cancelling_closes_item_and_menu_before_cancelled() {
    let root = tempfile::tempdir().expect("temp store");
    let store = SqliteStoreHandle::open(root.path())
        .await
        .expect("store opens");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub opens");
    let session_id = SessionId::new("panic-cancelling");
    let run_id = RunId::new("panic-cancelling-run");
    let item_id = ItemId::new("panic-open-item");
    let menu_id = MenuId::new("panic-open-menu");
    let generation = store.worker_generation();
    hub.create_session(create_command(&session_id, "panic-cancelling"))
        .await
        .expect("typed session commits");
    hub.accept_turn(accept_command(
        &session_id,
        &run_id,
        generation,
        "panic-cancelling",
    ))
    .await
    .expect("acceptance commits");
    let mut lifecycle = vec![
        run_payload_envelope(
            &session_id,
            &run_id,
            generation,
            "panic-item-started",
            EventPayload::Item(ItemEvent::Started {
                item_id: item_id.clone(),
                item: TurnItem::ToolCall {
                    call_id: "panic-call".into(),
                    name: "request_input".into(),
                    args: serde_json::json!({}),
                    status: ToolStatus::InProgress,
                },
            }),
        ),
        run_payload_envelope(
            &session_id,
            &run_id,
            generation,
            "panic-menu-opened",
            EventPayload::MenuOpened(Menu {
                id: menu_id.clone(),
                kind: MenuKind::Choice,
                title: "Continue?".into(),
                body: Vec::new(),
                options: vec![MenuOption {
                    key: "yes".into(),
                    label: "Yes".into(),
                    detail: None,
                    decision: None,
                }],
                blocking: true,
                scope: MenuScope::Session,
                origin: "request_input".into(),
                ttl_ms: None,
                timeout_option: None,
            }),
        ),
    ];
    hub.append(&mut lifecycle)
        .await
        .expect("open lifecycle commits");
    hub.cancel_turn(haider_store::TurnCancelCommand {
        command_id: "panic-cancel-command".into(),
        request_digest: "panic-cancel-digest".into(),
        request_json: "{}".into(),
        session_id: session_id.clone(),
        worker_generation: generation,
        run_id: run_id.clone(),
        cancelling_event_id: EventId::new("panic-cancelling-state"),
        device_id: DeviceId::new("worker-law-test"),
    })
    .await
    .expect("Cancelling commits");

    crate::worker::terminalize_supervisor_exit(&hub, &session_id, 1)
        .await
        .expect("panic exit terminalizes");
    let history = haider_core::StoreHandle::read(&store, &session_id, 0, 128)
        .await
        .expect("history reads");
    let payloads = history
        .into_iter()
        .filter(|envelope| envelope.run_id.as_ref() == Some(&run_id))
        .filter_map(|envelope| {
            serde_json::from_value::<EventPayload>(envelope.payload)
                .ok()
                .map(|payload| (envelope.seq, payload))
        })
        .collect::<Vec<_>>();
    let completed = payloads
        .iter()
        .position(|(_, payload)| {
            matches!(
                payload,
                EventPayload::Item(ItemEvent::Completed {
                    item_id: candidate,
                    item: TurnItem::ToolCall {
                        status: ToolStatus::Cancelled,
                        ..
                    },
                }) if *candidate == item_id
            )
        })
        .expect("item closes cancelled");
    let menu_closed = payloads
        .iter()
        .position(|(_, payload)| {
            matches!(
                payload,
                EventPayload::MenuClosed {
                    menu,
                    reason: MenuCloseReason::Cancelled,
                } if *menu == menu_id
            )
        })
        .expect("menu closes cancelled");
    let cancelled = payloads
        .iter()
        .position(|(_, payload)| *payload == EventPayload::RunState(RunState::Cancelled))
        .expect("run cancels");
    assert!(completed < cancelled && menu_closed < cancelled);
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// A supervisor panic with an orphaned dispatch but no cancellation must
/// reconcile Unknown before failure-shaped terminalization.
///
/// MUTATION CHECK: reconcile only the Cancelling branch. Expected failure:
/// Errored commits without an Unknown predecessor.
#[tokio::test]
async fn panic_exit_reconciles_dispatched_before_errored() {
    let root = tempfile::tempdir().expect("temp store");
    let store = SqliteStoreHandle::open(root.path())
        .await
        .expect("store opens");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub opens");
    let session_id = SessionId::new("panic-dispatched");
    let run_id = RunId::new("panic-dispatched-run");
    let effect_id = haider_protocol::ids::EffectId::new("panic-dispatched-effect");
    let generation = store.worker_generation();
    hub.create_session(create_command(&session_id, "panic-dispatched"))
        .await
        .expect("typed session commits");
    hub.accept_turn(accept_command(
        &session_id,
        &run_id,
        generation,
        "panic-dispatched",
    ))
    .await
    .expect("acceptance commits");
    let mut dispatched = [run_payload_envelope(
        &session_id,
        &run_id,
        generation,
        "panic-effect-dispatched",
        EventPayload::Effect(haider_protocol::effect::EffectPhase::Dispatched {
            effect: effect_id.clone(),
        }),
    )];
    hub.append(&mut dispatched).await.expect("dispatch commits");

    crate::worker::terminalize_supervisor_exit(&hub, &session_id, 1)
        .await
        .expect("panic exit terminalizes");
    let history = haider_core::StoreHandle::read(&store, &session_id, 0, 128)
        .await
        .expect("history reads");
    let payloads = history
        .into_iter()
        .filter(|envelope| envelope.run_id.as_ref() == Some(&run_id))
        .filter_map(|envelope| {
            serde_json::from_value::<EventPayload>(envelope.payload)
                .ok()
                .map(|payload| (envelope.seq, payload))
        })
        .collect::<Vec<_>>();
    let unknown = payloads
        .iter()
        .position(|(_, payload)| {
            matches!(
                payload,
                EventPayload::Effect(haider_protocol::effect::EffectPhase::Outcome {
                    effect,
                    outcome: haider_protocol::effect::EffectOutcome::Unknown,
                }) if *effect == effect_id
            )
        })
        .expect("Unknown reconciles");
    let errored = payloads
        .iter()
        .position(|(_, payload)| *payload == EventPayload::RunState(RunState::Errored))
        .expect("run errors");
    assert!(unknown < errored);
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// MUTATION CHECK: remove any owned ID charge from
/// `envelope_weight_bytes` (for example `branch_id`). Expected failure: the
/// estimator falls below the explicit fixed-value-plus-owned-strings size.
#[test]
fn envelope_weight_charges_every_large_owned_id_string() {
    let large = |label: &str| format!("{label}-{}", "x".repeat(16 * 1024));
    let envelope = EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(large("event")),
        seq: 1,
        session_id: SessionId::new(large("session")),
        branch_id: Some(BranchId::new(large("branch"))),
        run_id: Some(RunId::new(large("run"))),
        agent_id: Some(AgentId::new(large("agent"))),
        device_id: DeviceId::new(large("device")),
        authority_epoch: 2,
        worker_generation: 3,
        causation_id: Some(EventId::new(large("causation"))),
        correlation_id: Some(EventId::new(large("correlation"))),
        committed_at_ms: 4,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: serde_json::Value::Null,
    };
    let owned_string_bytes = envelope
        .event_id
        .as_str()
        .len()
        .saturating_add(envelope.session_id.as_str().len())
        .saturating_add(
            envelope
                .branch_id
                .as_ref()
                .map_or(0, |value| value.as_str().len()),
        )
        .saturating_add(
            envelope
                .run_id
                .as_ref()
                .map_or(0, |value| value.as_str().len()),
        )
        .saturating_add(
            envelope
                .agent_id
                .as_ref()
                .map_or(0, |value| value.as_str().len()),
        )
        .saturating_add(envelope.device_id.as_str().len())
        .saturating_add(
            envelope
                .causation_id
                .as_ref()
                .map_or(0, |value| value.as_str().len()),
        )
        .saturating_add(
            envelope
                .correlation_id
                .as_ref()
                .map_or(0, |value| value.as_str().len()),
        );
    let real_owned_lower_bound =
        std::mem::size_of::<RawEnvelope>().saturating_add(owned_string_bytes);

    assert!(
        envelope_weight_bytes(&envelope) >= real_owned_lower_bound,
        "every variable-length envelope field must be charged"
    );
}

struct AbortQueueSink {
    state: Mutex<AbortQueueState>,
    changed: Notify,
    pause_next_fire: AtomicBool,
    fired_reached: Notify,
    fired_release: Notify,
}

struct AbortQueueState {
    queue: VecDeque<WireFrame>,
    tickets: VecDeque<Weak<Notify>>,
}

impl AbortQueueState {
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

    fn fire_head(&mut self) {
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

impl AbortQueueSink {
    fn new() -> Self {
        Self {
            state: Mutex::new(AbortQueueState {
                queue: VecDeque::new(),
                tickets: VecDeque::new(),
            }),
            changed: Notify::new(),
            pause_next_fire: AtomicBool::new(true),
            fired_reached: Notify::new(),
            fired_release: Notify::new(),
        }
    }

    fn offer_with_ticket(
        &self,
        frame: &WireFrame,
        ticket: Option<&AdmissionTicket>,
    ) -> SendAdmission {
        let mut state = self.state.lock().expect("abort queue state");
        state.prune_dead_tickets();
        let caller_may_admit =
            state.tickets.is_empty() || ticket.is_some_and(|ticket| state.ticket_is_head(ticket));
        if !caller_may_admit || !state.queue.is_empty() {
            return SendAdmission::Busy;
        }
        if let Some(ticket) = ticket
            && state.ticket_is_head(ticket)
        {
            state.tickets.pop_front();
        }
        state.queue.push_back(frame.clone());
        SendAdmission::Sent
    }

    async fn wait_for_tickets(&self, count: usize) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let changed = self.changed.notified();
                if self.state.lock().expect("abort queue state").tickets.len() >= count {
                    return;
                }
                changed.await;
            }
        })
        .await
        .expect("waiters park deterministically");
    }

    fn pop(&self) -> WireFrame {
        let mut state = self.state.lock().expect("abort queue state");
        let frame = state.queue.pop_front().expect("queued frame");
        state.fire_head();
        frame
    }
}

impl FrameSink for AbortQueueSink {
    fn try_send(&self, _frame: WireFrame) -> Result<(), FrameSendError> {
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
        self.state
            .lock()
            .expect("abort queue state")
            .tickets
            .push_back(Arc::downgrade(&ticket));
        self.changed.notify_waiters();
        Some(ticket)
    }

    fn cancel_ticket(&self, ticket: &AdmissionTicket) {
        let mut state = self.state.lock().expect("abort queue state");
        if state.remove_ticket(ticket) {
            state.fire_head();
        }
    }

    fn ticket_fired_test_gate(
        &self,
    ) -> Option<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>>> {
        self.pause_next_fire
            .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| {
                Box::pin(async {
                    self.fired_reached.notify_one();
                    self.fired_release.notified().await;
                })
                    as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>>
            })
    }
}

fn caught_up(attachment_id: &AttachmentId, seq: u64) -> WireFrame {
    WireFrame::AttachCaughtUp {
        attachment_id: attachment_id.clone(),
        high_water_seq: seq,
    }
}

fn caught_up_seq(frame: WireFrame) -> u64 {
    let WireFrame::AttachCaughtUp { high_water_seq, .. } = frame else {
        panic!("expected caught-up frame");
    };
    high_water_seq
}

fn spawn_delivery(
    hub: SessionHub,
    sink: Arc<dyn FrameSink>,
    attachment_id: AttachmentId,
    seq: u64,
) -> tokio::task::JoinHandle<FrameDelivery> {
    tokio::spawn(async move {
        let (lag_sender, mut lagged) = watch::channel::<Option<u64>>(None);
        let (cancel_sender, mut cancel) = watch::channel(false);
        let keep_senders_alive = (lag_sender, cancel_sender);
        let result = deliver_frame(
            &hub,
            &sink,
            &attachment_id,
            &caught_up(&attachment_id, seq),
            &mut lagged,
            &mut cancel,
        )
        .await;
        drop(keep_senders_alive);
        result
    })
}

/// Capacity one: actual `deliver_frame` tasks A and B park, A's ticket fires,
/// and A is raw-aborted at the controlled fired-before-reoffer await. Fresh C
/// then joins; B must be admitted first and C after it without a wedge.
///
/// MUTATION CHECK: revert BOTH the `AdmissionTicketGuard` wiring/drop cleanup
/// and the connection outbox's dead-head successor firing. Expected failure:
/// B's timeout expires after C prunes dead A without waking the exposed head.
/// Verified by revert on 2026-07-27.
#[tokio::test]
async fn aborting_deliver_frame_before_reoffer_keeps_fifo_admission_live() {
    let root = tempfile::tempdir().expect("temp store");
    let store = SqliteStoreHandle::open(root.path())
        .await
        .expect("store opens");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub opens");
    let attachment_id = AttachmentId::new("abort-before-reoffer");
    let session_id = SessionId::new("abort-before-reoffer");
    let (commands, _command_receiver) = mpsc::channel(1);
    let (owner_cancel, _owner_cancel_receiver) = watch::channel(false);
    lock(&hub.inner.attachments)
        .expect("attachments lock")
        .insert(
            attachment_id.clone(),
            AttachmentOwner {
                connection_id: "abort-test".into(),
                session_id,
                mode: AttachMode::View,
                actor: SessionActorHandle { commands },
                cancel: owner_cancel,
            },
        );

    let sink_impl = Arc::new(AbortQueueSink::new());
    let sink: Arc<dyn FrameSink> = sink_impl.clone();
    assert!(matches!(
        sink.offer(&attachment_id, &caught_up(&attachment_id, 0)),
        SendAdmission::Sent
    ));

    let first = spawn_delivery(hub.clone(), Arc::clone(&sink), attachment_id.clone(), 1);
    sink_impl.wait_for_tickets(1).await;

    let second = spawn_delivery(hub.clone(), Arc::clone(&sink), attachment_id.clone(), 2);
    sink_impl.wait_for_tickets(2).await;

    assert_eq!(caught_up_seq(sink_impl.pop()), 0);
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        sink_impl.fired_reached.notified(),
    )
    .await
    .expect("A reaches the fired-before-reoffer await");
    first.abort();
    let abort_error = match first.await {
        Err(error) => error,
        Ok(_) => panic!("raw abort must cancel A"),
    };
    assert!(
        abort_error.is_cancelled(),
        "A must be dropped inside deliver_frame"
    );

    let fresh = spawn_delivery(hub.clone(), Arc::clone(&sink), attachment_id.clone(), 3);

    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(5), second)
            .await
            .expect("B is admitted after A abort")
            .expect("B task joins"),
        FrameDelivery::Delivered
    ));
    assert_eq!(caught_up_seq(sink_impl.pop()), 2);
    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(5), fresh)
            .await
            .expect("C is admitted after B")
            .expect("C task joins"),
        FrameDelivery::Delivered
    ));
    assert_eq!(caught_up_seq(sink_impl.pop()), 3);

    lock(&hub.inner.attachments)
        .expect("attachments lock")
        .remove(&attachment_id);
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// R3 aggregate-idle law at the terminalization sites (review r2 NF-1): a
/// per-run terminalization must never commit `SessionState::Idle` while any
/// other durable run in the session is nonterminal. Site under test: the
/// recovery-feed degradation path (`terminalize_recovery_feed_failure`),
/// driven through a legacy metadata-less session so `supervisor_for` fails.
/// The positive control proves the settle-guarded idle DOES commit once the
/// last nonterminal run terminalizes — the guard, not the call site, decides.
///
/// MUTATION CHECK: restore the unfiltered `failed_resumption_payloads`
/// append at `terminalize_recovery_feed_failure` (drop the SessionState
/// retain and the `append_session_idle` call). Expected failure: the
/// payload-embedded `Idle { interrupted: true }` commits while run A is
/// durably Queued, and the zero-idle assertion below fails.
#[tokio::test]
async fn recovery_terminalization_never_settles_idle_while_another_run_is_active() {
    let root = tempfile::tempdir().expect("temp store");
    let store = SqliteStoreHandle::open(root.path())
        .await
        .expect("store opens");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub opens");
    let session_id = SessionId::new("aggregate-idle-law");
    let run_a = RunId::new("aggregate-idle-run-a");
    let run_b = RunId::new("aggregate-idle-run-b");
    let generation = store.worker_generation();
    // Legacy session: raw appends only, so no typed live-worker metadata
    // exists and the recovery feed cannot build a supervisor for it.
    let mut queued = vec![
        run_state_envelope(
            &session_id,
            &run_a,
            generation,
            "idle-law-a-queued",
            RunState::Queued,
        ),
        run_state_envelope(
            &session_id,
            &run_b,
            generation,
            "idle-law-b-queued",
            RunState::Queued,
        ),
    ];
    hub.append(&mut queued)
        .await
        .expect("legacy queued runs commit");
    let accepted = |run_id: &RunId, seq: u64| haider_store::AcceptedTurn {
        session_id: session_id.clone(),
        run_id: run_id.clone(),
        accepted_seq: seq,
        worker_generation: generation,
        disposition: haider_store::TurnAdmissionDisposition::Queued,
    };
    let manager = crate::worker::WorkerManager::start(
        hub.clone(),
        crate::worker::WorkerDependencies::unconfigured_for_tests(),
        false,
    );
    let handle = manager.handle();

    handle
        .recover_queued(accepted(&run_b, queued[1].seq))
        .await
        .expect("feed failure degrades per-item, not fatally");

    let idle_envelopes = |history: &[RawEnvelope]| {
        history
            .iter()
            .filter_map(|envelope| {
                serde_json::from_value::<EventPayload>(envelope.payload.clone()).ok()
            })
            .filter(|payload| {
                matches!(
                    payload,
                    EventPayload::SessionState(haider_protocol::state::SessionState::Idle { .. })
                )
            })
            .count()
    };
    let latest_state = |history: &[RawEnvelope], run: &RunId| {
        history
            .iter()
            .filter(|envelope| envelope.run_id.as_ref() == Some(run))
            .filter_map(|envelope| {
                match serde_json::from_value::<EventPayload>(envelope.payload.clone()) {
                    Ok(EventPayload::RunState(state)) => Some(state),
                    _ => None,
                }
            })
            .next_back()
    };

    let history = haider_core::StoreHandle::read(&store, &session_id, 0, 128)
        .await
        .expect("history reads");
    assert!(
        latest_state(&history, &run_b).is_some_and(|state| state.is_terminal()),
        "run B terminalizes"
    );
    assert_eq!(
        latest_state(&history, &run_a),
        Some(RunState::Queued),
        "run A stays durably Queued"
    );
    assert_eq!(
        idle_envelopes(&history),
        0,
        "no aggregate Idle commits while run A is nonterminal"
    );

    // Positive control: terminalizing the last nonterminal run settles the
    // session — exactly one guarded Idle { interrupted: true } commits.
    handle
        .recover_queued(accepted(&run_a, queued[0].seq))
        .await
        .expect("second degradation succeeds");
    let history = haider_core::StoreHandle::read(&store, &session_id, 0, 128)
        .await
        .expect("history rereads");
    assert!(
        latest_state(&history, &run_a).is_some_and(|state| state.is_terminal()),
        "run A terminalizes"
    );
    assert_eq!(
        idle_envelopes(&history),
        1,
        "the settle-guarded aggregate Idle commits once the session quiesces"
    );

    handle.begin_draining();
    manager.shutdown().await.expect("manager drains");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}
