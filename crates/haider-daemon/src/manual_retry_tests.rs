//! End-to-end daemon laws for receipt-backed `run.retry`.

#![allow(clippy::expect_used)]

use crate::accounts::ConnectionTransport;
use crate::session_hub::{FrameSendError, FrameSink, HubConnection, SessionHub, SessionHubConfig};
use crate::turn_recovery::{RecoveredWork, recover_interrupted_turns};
use crate::worker::{
    BrokerToolFactory, ProviderFactory, ResolvedTurnProvider, WorkerDependencies, WorkerManager,
};
use haider_core::{SessionCreateCommand, SqliteStoreHandle, StoreHandle, TurnAcceptCommand};
use haider_protocol::DeliveryMode;
use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
use haider_protocol::error::{ErrorAction, ErrorCode, ErrorPresentation, ErrorScope, HaiderError};
use haider_protocol::ids::{DeviceId, EventId, RunId, SessionId};
use haider_protocol::retry::RunRetryEventPayload;
use haider_protocol::session::SessionMetadataV1;
use haider_protocol::state::{RunState, SessionState, WaitReason};
use haider_protocol::{EventPayload, provider::FinishReason};
use haider_provider::{Block, FakeProvider, FakeStep, Message, ProviderErrorKind};
use haider_rpc::{
    AttachMode, Capability, CommandId, RequestBody, RequestId, ResponseBody, SubmitDisposition,
    WireFrame,
};
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use tokio::time::{Duration, timeout};

struct StaticProviderFactory {
    provider: Arc<FakeProvider>,
}

#[async_trait::async_trait]
impl ProviderFactory for StaticProviderFactory {
    async fn resolve_for_turn(
        &self,
        metadata: &SessionMetadataV1,
    ) -> Result<ResolvedTurnProvider, HaiderError> {
        Ok(ResolvedTurnProvider {
            provider: Arc::clone(&self.provider) as Arc<dyn haider_provider::Provider>,
            provider_name: metadata.provider.clone(),
            model: metadata.model.clone(),
            context_window: None,
            account_alias: None,
            initial_rotation: None,
            rotation_budget_consumed: false,
            attempt_resolver: None,
            compaction_promotion: None,
        })
    }
}

#[derive(Default)]
struct CapturingSink(Mutex<Vec<WireFrame>>);

impl FrameSink for CapturingSink {
    fn try_send(&self, frame: WireFrame) -> Result<(), FrameSendError> {
        self.0.lock().expect("sink lock").push(frame);
        Ok(())
    }
}

struct RetryWorld {
    _root: tempfile::TempDir,
    store: SqliteStoreHandle,
    hub: SessionHub,
    manager: Option<WorkerManager>,
    session_id: SessionId,
    failed_run_id: RunId,
    user_seq: u64,
}

fn assert_retry_provider_messages(messages: &[Message]) {
    assert_eq!(
        messages.first(),
        Some(&Message::user_text("retry this exact user turn")),
        "the fresh run compiles the failed run's committed user ancestry"
    );
    assert_eq!(
        messages.len(),
        2,
        "the immutable user ancestry is followed only by daemon session context"
    );
    assert!(messages[1].blocks.iter().any(|block| {
        matches!(block, Block::Text { text } if text.starts_with("[DAEMON-BOUND SESSION CONTEXT]"))
    }));
}

impl RetryWorld {
    async fn terminal_failed(prefix: &str, provider: Arc<FakeProvider>) -> Self {
        let root = tempfile::tempdir().expect("temp profile");
        let store = SqliteStoreHandle::open(root.path()).await.expect("store");
        let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
        let session_id = SessionId::new(format!("{prefix}-session"));
        let failed_run_id = RunId::new(format!("{prefix}-failed-run"));
        let device_id = DeviceId::new(format!("{prefix}-device"));
        let cwd = std::fs::canonicalize(std::env::current_dir().expect("cwd"))
            .expect("canonical cwd")
            .to_string_lossy()
            .into_owned();
        hub.create_internal_session(SessionCreateCommand {
            command_id: format!("{prefix}-create"),
            request_digest: format!("{prefix}-create-digest"),
            request_json: format!(r#"{{"session":"{prefix}"}}"#),
            session_id: session_id.clone(),
            cwd,
            provider: "fake".into(),
            model: "fake-model".into(),
            max_tokens: 4096,
            permission_overrides: None,
            effort: None,
            fast: false,
            cache_policy: Default::default(),
            system_prompt_version: crate::worker::SystemPromptBuilder::VERSION.into(),
            event_id: EventId::new(format!("{prefix}-created")),
            device_id: device_id.clone(),
        })
        .await
        .expect("create session");
        let accepted = hub
            .accept_internal_turn(TurnAcceptCommand {
                command_id: format!("{prefix}-turn"),
                request_digest: format!("{prefix}-turn-digest"),
                request_json: format!(r#"{{"turn":"{prefix}"}}"#),
                session_id: session_id.clone(),
                worker_generation: store.worker_generation(),
                run_id: failed_run_id.clone(),
                agent_id: None,
                branch_id: None,
                text: "retry this exact user turn".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
                queued_event_id: EventId::new(format!("{prefix}-queued")),
                user_event_id: EventId::new(format!("{prefix}-user")),
                active_event_id: EventId::new(format!("{prefix}-active")),
                device_id: device_id.clone(),
            })
            .await
            .expect("accept failed turn");
        let user_seq = accepted.accepted_seq;
        let mut terminal = vec![
            envelope(
                &session_id,
                &failed_run_id,
                &device_id,
                store.worker_generation(),
                format!("{prefix}-run-failed"),
                EventPayload::RunFailed {
                    code: ErrorCode::ProviderError,
                    message: "provider exhausted".into(),
                    retryable: true,
                    presentation: Some(ErrorPresentation::new(
                        "provider-exhausted",
                        "Provider failed",
                        "Retry the turn.",
                        ErrorScope::Turn,
                        [ErrorAction::Retry],
                    )),
                },
            ),
            envelope(
                &session_id,
                &failed_run_id,
                &device_id,
                store.worker_generation(),
                format!("{prefix}-errored"),
                EventPayload::RunState(RunState::Errored),
            ),
            envelope(
                &session_id,
                &failed_run_id,
                &device_id,
                store.worker_generation(),
                format!("{prefix}-idle"),
                EventPayload::SessionState(SessionState::Idle { interrupted: false }),
            ),
        ];
        hub.append(&mut terminal).await.expect("terminal failure");
        let manager = WorkerManager::start(
            hub.clone(),
            WorkerDependencies {
                diagnostics: None,
                provider_factory: Arc::new(StaticProviderFactory { provider }),
                tool_factory: Arc::new(BrokerToolFactory),
                delegation: None,
                web_search: None,
            },
            false,
        );
        hub.install_worker_manager(manager.handle())
            .expect("install manager");
        Self {
            _root: root,
            store,
            hub,
            manager: Some(manager),
            session_id,
            failed_run_id,
            user_seq,
        }
    }

    async fn idle(prefix: &str) -> Self {
        let root = tempfile::tempdir().expect("idle temp profile");
        let store = SqliteStoreHandle::open(root.path())
            .await
            .expect("idle store");
        let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("idle hub");
        let session_id = SessionId::new(format!("{prefix}-idle-session"));
        let cwd = std::fs::canonicalize(std::env::current_dir().expect("cwd"))
            .expect("canonical cwd")
            .to_string_lossy()
            .into_owned();
        hub.create_internal_session(SessionCreateCommand {
            command_id: format!("{prefix}-idle-create"),
            request_digest: format!("{prefix}-idle-create-digest"),
            request_json: format!(r#"{{"session":"{prefix}-idle"}}"#),
            session_id: session_id.clone(),
            cwd,
            provider: "fake".into(),
            model: "fake-model".into(),
            max_tokens: 4096,
            permission_overrides: None,
            effort: None,
            fast: false,
            cache_policy: Default::default(),
            system_prompt_version: crate::worker::SystemPromptBuilder::VERSION.into(),
            event_id: EventId::new(format!("{prefix}-idle-created")),
            device_id: DeviceId::new(format!("{prefix}-idle-device")),
        })
        .await
        .expect("create idle session");
        Self {
            _root: root,
            store,
            hub,
            manager: None,
            session_id,
            failed_run_id: RunId::new("none"),
            user_seq: 0,
        }
    }

    async fn control(&self) -> (HubConnection, Arc<CapturingSink>) {
        let sink = Arc::new(CapturingSink::default());
        let connection = self
            .hub
            .open_connection(
                BTreeSet::from([Capability::View, Capability::Control]),
                sink.clone(),
                ConnectionTransport::LocalSameUid,
            )
            .expect("control connection");
        connection
            .request(
                RequestId::new("retry-attach"),
                RequestBody::SessionAttach {
                    session_id: self.session_id.clone(),
                    after_seq: 0,
                    mode: AttachMode::Control,
                    sealed_replay: false,
                },
            )
            .await
            .expect("attach control");
        sink.0.lock().expect("sink").clear();
        (connection, sink)
    }

    async fn shutdown(mut self) {
        if let Some(manager) = self.manager.take() {
            manager.shutdown().await.expect("manager shutdown");
        }
        self.hub.shutdown().await.expect("hub shutdown");
        self.store.close().await.expect("store close");
    }
}

fn envelope(
    session_id: &SessionId,
    run_id: &RunId,
    device_id: &DeviceId,
    worker_generation: u64,
    event_id: String,
    payload: EventPayload,
) -> haider_protocol::envelope::RawEnvelope {
    EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(event_id),
        seq: 0,
        session_id: session_id.clone(),
        branch_id: None,
        run_id: Some(run_id.clone()),
        agent_id: None,
        device_id: device_id.clone(),
        authority_epoch: 0,
        worker_generation,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: serde_json::to_value(payload).expect("serialize test payload"),
    }
}

async fn request(
    connection: &HubConnection,
    sink: &CapturingSink,
    request_id: &str,
    body: RequestBody,
) -> ResponseBody {
    connection
        .request(RequestId::new(request_id), body)
        .await
        .expect("request routes");
    let mut frames = sink.0.lock().expect("sink frames");
    let response = frames
        .iter()
        .find_map(|frame| match frame {
            WireFrame::Response {
                request_id: found,
                body,
            } if found.as_str() == request_id => Some(body.clone()),
            _ => None,
        })
        .expect("correlated response");
    frames.clear();
    response
}

fn retry_request(world: &RetryWorld, command: &str) -> RequestBody {
    RequestBody::RunRetry {
        command_id: CommandId::new(command),
        session_id: world.session_id.clone(),
        worker_generation: world.store.worker_generation(),
    }
}

async fn wait_for_state(world: &RetryWorld, run_id: &RunId, wanted: RunState) {
    wait_for_store_state(&world.store, &world.session_id, run_id, wanted).await;
}

async fn wait_for_store_state(
    store: &SqliteStoreHandle,
    session_id: &SessionId,
    run_id: &RunId,
    wanted: RunState,
) {
    timeout(Duration::from_secs(10), async {
        loop {
            let found = store
                .read(session_id, 0, 512)
                .await
                .expect("read states")
                .into_iter()
                .any(|event| {
                    event.run_id.as_ref() == Some(run_id)
                        && serde_json::from_value::<EventPayload>(event.payload)
                            .is_ok_and(|payload| payload == EventPayload::RunState(wanted.clone()))
                });
            if found {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("state reached");
}

async fn wait_for_retrying_event(
    world: &RetryWorld,
    run_id: &RunId,
) -> haider_protocol::envelope::RawEnvelope {
    timeout(Duration::from_secs(10), async {
        loop {
            if let Some(event) = world
                .store
                .read(&world.session_id, 0, 512)
                .await
                .expect("read retrying facts")
                .into_iter()
                .find(|event| {
                    event.run_id.as_ref() == Some(run_id)
                        && matches!(
                            serde_json::from_value::<EventPayload>(event.payload.clone()),
                            Ok(EventPayload::RunState(RunState::Retrying { .. }))
                        )
                })
            {
                return event;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("Retrying fact reached on the poll grid")
}

async fn wait_for_provider_requests(provider: &FakeProvider, expected: usize) {
    timeout(Duration::from_secs(10), async {
        loop {
            if provider.requests().len() >= expected {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("provider request fact reached on the poll grid");
}

/// MUTATION CHECK: append a second `UserMessage`, compile the new run rather
/// than `failed_run_id`, or omit the atomic `RunRetried` fact. Expected
/// runtime failure: the journal count, provider prompt, or source coordinates
/// below diverge even if the retry happens to finish.
#[tokio::test]
async fn run_retry_terminal_failure_starts_one_fresh_run_on_the_same_user_turn() {
    let fake = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitText {
            text: "retry succeeded".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let world = RetryWorld::terminal_failed("retry-success", Arc::clone(&fake)).await;
    let (connection, sink) = world.control().await;
    let response = request(
        &connection,
        &sink,
        "retry-success-request",
        retry_request(&world, "retry-success-command"),
    )
    .await;
    let ResponseBody::RunRetry {
        run_id,
        failed_run_id,
        user_seq,
        ..
    } = response
    else {
        panic!("expected run.retry success")
    };
    assert_ne!(run_id, world.failed_run_id, "retry gets a fresh run id");
    assert_eq!(failed_run_id, world.failed_run_id);
    assert_eq!(user_seq, world.user_seq);
    wait_for_state(&world, &run_id, RunState::Done).await;
    let events = world
        .store
        .read(&world.session_id, 0, 512)
        .await
        .expect("retry history");
    assert_eq!(
        events
            .iter()
            .filter(
                |event| serde_json::from_value::<EventPayload>(event.payload.clone())
                    .is_ok_and(|payload| matches!(payload, EventPayload::UserMessage { .. }))
            )
            .count(),
        1,
        "manual retry never commits a second UserMessage"
    );
    assert_eq!(
        events
            .iter()
            .filter(
                |event| RunRetryEventPayload::from_payload_value(event.payload.clone()).is_ok_and(
                    |payload| matches!(payload, RunRetryEventPayload::RunRetried { .. })
                )
            )
            .count(),
        1
    );
    let requests = fake.requests();
    assert_eq!(requests.len(), 1, "exactly one fresh provider run starts");
    assert_retry_provider_messages(&requests[0].messages);
    let stale_failure_retry = request(
        &connection,
        &sink,
        "retry-after-success-request",
        retry_request(&world, "retry-after-success-command"),
    )
    .await;
    assert!(matches!(
        stale_failure_retry,
        ResponseBody::Error { ref code, ref message, .. }
            if code == haider_rpc::ERROR_CODE_INVALID_ARGUMENT
                && message.contains("terminal-failed")
    ));
    drop(connection);
    world.shutdown().await;
}

/// MUTATION CHECK: bind a retry-of-retry to the immediately failed run's
/// empty synthetic history instead of preserving the original prompt run, or
/// make the second retry ineligible because it has no new `UserMessage`.
/// Expected runtime failure: the second acceptance/source coordinate or its
/// provider prompt differs from the original committed user turn.
#[tokio::test]
async fn run_retry_of_failed_retry_reuses_the_original_user_turn() {
    let fake = Arc::new(FakeProvider::new(vec![
        FakeStep::Error {
            kind: ProviderErrorKind::QuotaExhausted,
            message: "first manual retry still failed".into(),
            retry_after_ms: None,
        },
        FakeStep::EmitText {
            text: "second manual retry succeeded".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let world = RetryWorld::terminal_failed("retry-chain", Arc::clone(&fake)).await;
    let (connection, sink) = world.control().await;
    let first = request(
        &connection,
        &sink,
        "retry-chain-first-request",
        retry_request(&world, "retry-chain-first-command"),
    )
    .await;
    let ResponseBody::RunRetry {
        run_id: first_retry_run,
        failed_run_id: first_failed_run,
        user_seq: first_user_seq,
        ..
    } = first
    else {
        panic!("first retry is accepted")
    };
    assert_eq!(first_failed_run, world.failed_run_id);
    assert_eq!(first_user_seq, world.user_seq);
    wait_for_state(&world, &first_retry_run, RunState::Errored).await;

    let second = request(
        &connection,
        &sink,
        "retry-chain-second-request",
        retry_request(&world, "retry-chain-second-command"),
    )
    .await;
    let ResponseBody::RunRetry {
        run_id: second_retry_run,
        failed_run_id: second_failed_run,
        user_seq: second_user_seq,
        ..
    } = second
    else {
        panic!("retry of the failed retry is accepted")
    };
    assert_eq!(second_failed_run, first_retry_run);
    assert_eq!(second_user_seq, world.user_seq);
    wait_for_state(&world, &second_retry_run, RunState::Done).await;

    let events = world
        .store
        .read(&world.session_id, 0, 512)
        .await
        .expect("retry-chain history");
    assert_eq!(
        events
            .iter()
            .filter(|event| {
                serde_json::from_value::<EventPayload>(event.payload.clone())
                    .is_ok_and(|payload| matches!(payload, EventPayload::UserMessage { .. }))
            })
            .count(),
        1,
        "a retry chain still has one durable user message"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| {
                RunRetryEventPayload::from_payload_value(event.payload.clone())
                    .is_ok_and(|payload| matches!(payload, RunRetryEventPayload::RunRetried { .. }))
            })
            .count(),
        2,
        "each fresh retry gets one durable retry fact"
    );
    let requests = fake.requests();
    assert_eq!(requests.len(), 2, "each manual retry starts once");
    requests
        .iter()
        .for_each(|request| assert_retry_provider_messages(&request.messages));
    drop(connection);
    world.shutdown().await;
}

/// MUTATION CHECK: omit `RunRetried` from startup reduction, recover it as an
/// ordinary queued turn, or compile the fresh retry run rather than its
/// durable prompt source. Expected runtime failure: recovery finds no retry,
/// starts zero/multiple provider requests, or loses the original user prompt.
#[tokio::test]
async fn run_retry_lost_handoff_recovers_once_after_restart() {
    let fake = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitText {
            text: "restart retry succeeded".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let mut world =
        RetryWorld::terminal_failed("retry-restart", Arc::new(FakeProvider::new(Vec::new()))).await;
    world
        .manager
        .take()
        .expect("initial manager")
        .shutdown()
        .await
        .expect("stop before handoff");
    let (connection, sink) = world.control().await;
    let response = request(
        &connection,
        &sink,
        "retry-restart-request",
        retry_request(&world, "retry-restart-command"),
    )
    .await;
    assert!(
        matches!(response, ResponseBody::Error { .. }),
        "acceptance commits but its stopped-manager handoff is reported"
    );
    let accepted_run = world
        .store
        .read(&world.session_id, 0, 512)
        .await
        .expect("accepted retry history")
        .into_iter()
        .find_map(|event| {
            RunRetryEventPayload::from_payload_value(event.payload)
                .is_ok_and(|payload| matches!(payload, RunRetryEventPayload::RunRetried { .. }))
                .then_some(event.run_id)
                .flatten()
        })
        .expect("retry acceptance is durable before handoff");
    drop(connection);

    let RetryWorld {
        _root: root,
        store,
        hub,
        manager: None,
        session_id,
        ..
    } = world
    else {
        panic!("initial manager was removed")
    };
    hub.shutdown().await.expect("initial hub shutdown");
    drop(hub);
    store.close().await.expect("initial store close");

    let restarted_store = SqliteStoreHandle::open(root.path())
        .await
        .expect("restarted store");
    let mut recovered = recover_interrupted_turns(
        &restarted_store,
        &DeviceId::new("retry-restart-recovery-device"),
    )
    .await
    .expect("reduce interrupted retry");
    assert_eq!(recovered.len(), 1, "one durable retry is recoverable");
    let RecoveredWork::Retry(accepted) = recovered.pop().expect("recovered retry") else {
        panic!("queued retry retains its retry-specific source coordinates")
    };
    assert_eq!(accepted.run_id, accepted_run);
    let restarted_hub =
        SessionHub::new(restarted_store.clone(), SessionHubConfig::default()).expect("restart hub");
    let restarted_manager = WorkerManager::start(
        restarted_hub.clone(),
        WorkerDependencies {
            diagnostics: None,
            provider_factory: Arc::new(StaticProviderFactory {
                provider: Arc::clone(&fake),
            }),
            tool_factory: Arc::new(BrokerToolFactory),
            delegation: None,
            web_search: None,
        },
        false,
    );
    let restarted_handle = restarted_manager.handle();
    restarted_hub
        .install_worker_manager(restarted_handle.clone())
        .expect("install restarted manager");
    restarted_handle
        .recover_retry(accepted)
        .await
        .expect("handoff recovered retry");
    wait_for_store_state(&restarted_store, &session_id, &accepted_run, RunState::Done).await;
    let requests = fake.requests();
    assert_eq!(requests.len(), 1, "recovery starts the retry exactly once");
    assert_retry_provider_messages(&requests[0].messages);
    restarted_manager
        .shutdown()
        .await
        .expect("restarted manager shutdown");
    restarted_hub
        .shutdown()
        .await
        .expect("restarted hub shutdown");
    drop(restarted_hub);
    restarted_store
        .close()
        .await
        .expect("restarted store close");
}

/// MUTATION CHECK: move the durable live-run gate outside its transaction or
/// bypass receipt replay. Expected runtime failure: both racing commands
/// create `RunRetried` facts/provider requests, or the accepted command's
/// replay returns different run coordinates.
#[tokio::test]
async fn run_retry_duplicate_while_live_is_refused_without_a_second_run() {
    let fake = Arc::new(FakeProvider::new(vec![FakeStep::Hang]));
    let world = RetryWorld::terminal_failed("retry-duplicate", Arc::clone(&fake)).await;
    let (connection_one, sink_one) = world.control().await;
    let (connection_two, sink_two) = world.control().await;
    let (outcome_one, outcome_two) = tokio::join!(
        request(
            &connection_one,
            &sink_one,
            "retry-race-one",
            retry_request(&world, "retry-command-one"),
        ),
        request(
            &connection_two,
            &sink_two,
            "retry-race-two",
            retry_request(&world, "retry-command-two"),
        ),
    );
    let outcomes = [
        (outcome_one, "retry-command-one"),
        (outcome_two, "retry-command-two"),
    ];
    let accepted = outcomes
        .iter()
        .find_map(|(response, command)| match response {
            ResponseBody::RunRetry { run_id, .. } => Some((run_id.clone(), *command)),
            _ => None,
        })
        .expect("exactly one racing retry is accepted");
    assert_eq!(
        outcomes
            .iter()
            .filter(|(response, _)| matches!(response, ResponseBody::RunRetry { .. }))
            .count(),
        1,
        "the failed turn admits one fresh retry run"
    );
    let duplicate = outcomes
        .iter()
        .find_map(|(response, _)| {
            matches!(response, ResponseBody::Error { .. }).then_some(response)
        })
        .expect("the other racing retry is refused");
    assert!(matches!(
        duplicate,
        ResponseBody::Error { code, message, retryable: false, .. }
            if code == haider_rpc::ERROR_CODE_INVALID_ARGUMENT
                && message.contains("still live")
    ));
    wait_for_state(&world, &accepted.0, RunState::Thinking).await;
    let replay = request(
        &connection_one,
        &sink_one,
        "retry-same-command-replay",
        retry_request(&world, accepted.1),
    )
    .await;
    let ResponseBody::RunRetry {
        run_id: replay_run, ..
    } = replay
    else {
        panic!("receipt replay succeeds")
    };
    assert_eq!(replay_run, accepted.0, "receipt replays the same run");
    let events = world
        .store
        .read(&world.session_id, 0, 512)
        .await
        .expect("retry history");
    assert_eq!(
        events
            .iter()
            .filter(
                |event| RunRetryEventPayload::from_payload_value(event.payload.clone()).is_ok_and(
                    |payload| matches!(payload, RunRetryEventPayload::RunRetried { .. })
                )
            )
            .count(),
        1,
        "only one retry run is durably accepted"
    );
    wait_for_provider_requests(&fake, 1).await;
    assert_eq!(fake.requests().len(), 1, "only one provider run starts");
    drop(connection_one);
    drop(connection_two);
    world.shutdown().await;
}

/// Workflow selection and fresh retry admission have one total order. If the
/// retry wins, the now-live run rejects the switch; if the switch wins, its
/// pin is durable before the retry is accepted and handed to the provider.
#[tokio::test]
async fn run_retry_and_graph_switch_cannot_cross_the_provider_authority_boundary() {
    let fake = Arc::new(FakeProvider::new(vec![FakeStep::Hang]));
    let world = RetryWorld::terminal_failed("retry-graph-switch", Arc::clone(&fake)).await;
    let (retry_connection, retry_sink) = world.control().await;
    let (switch_connection, switch_sink) = world.control().await;

    let initial = request(
        &switch_connection,
        &switch_sink,
        "retry-graph-initial-pin",
        RequestBody::GraphPin {
            command_id: CommandId::new("retry-graph-initial-pin-command"),
            session_id: world.session_id.clone(),
            worker_generation: world.store.worker_generation(),
            template: haider_protocol::graph::SHIP_LOOP_TEMPLATE.into(),
            expected_digest: None,
        },
    )
    .await;
    let ResponseBody::GraphPin {
        graph_id: old_graph_id,
        ..
    } = initial
    else {
        panic!("initial graph pin succeeds while the failed session is idle")
    };

    let (retry, switch) = tokio::join!(
        request(
            &retry_connection,
            &retry_sink,
            "retry-graph-retry",
            retry_request(&world, "retry-graph-retry-command"),
        ),
        request(
            &switch_connection,
            &switch_sink,
            "retry-graph-switch",
            RequestBody::GraphSwitch {
                command_id: CommandId::new("retry-graph-switch-command"),
                session_id: world.session_id.clone(),
                worker_generation: world.store.worker_generation(),
                old_graph_id,
                template: haider_protocol::graph::STAGGERED_TEMPLATE.into(),
                expected_digest: None,
            },
        ),
    );
    let ResponseBody::RunRetry { accepted_seq, .. } = retry else {
        panic!("the terminal-failure retry remains admissible")
    };
    match switch {
        ResponseBody::Error { code, .. } => {
            assert_eq!(
                code,
                haider_rpc::ERROR_CODE_BUSY,
                "a retry-first ordering makes the session nonterminal"
            );
        }
        ResponseBody::GraphSwitch { pinned_seq, .. } => {
            assert!(
                pinned_seq < accepted_seq,
                "a switch-first ordering must commit the new workflow before retry admission"
            );
        }
        other => panic!("unexpected graph-switch race response: {other:?}"),
    }

    drop(retry_connection);
    drop(switch_connection);
    world.shutdown().await;
}

/// MUTATION CHECK: accept an idle session merely because all runs are
/// terminal (or because there are no runs). Expected runtime failure: this
/// request returns `run.retry` success instead of typed `invalid_argument`.
#[tokio::test]
async fn run_retry_idle_never_failed_session_is_refused() {
    let world = RetryWorld::idle("retry-idle").await;
    let (connection, sink) = world.control().await;
    let response = request(
        &connection,
        &sink,
        "retry-idle-request",
        retry_request(&world, "retry-idle-command"),
    )
    .await;
    assert!(matches!(
        response,
        ResponseBody::Error { ref code, ref message, retryable: false, .. }
            if code == haider_rpc::ERROR_CODE_INVALID_ARGUMENT
                && message.contains("terminal-failed")
    ));
    drop(connection);
    world.shutdown().await;
}

/// MUTATION CHECK: retain the old mid-backoff refusal, mint a fresh run, lose
/// the exact `Retrying` event coordinate, or replay the receipt as a second
/// attempt. Expected runtime failure: the wake is refused/times out, response
/// coordinates diverge, a second `RunRetried` appears, or request count is 3.
#[tokio::test]
async fn run_retry_mid_backoff_wakes_exact_attempt_and_receipt_replay_is_noop() {
    let fake = Arc::new(FakeProvider::new(vec![
        FakeStep::Error {
            kind: ProviderErrorKind::Overloaded,
            message: "automatic reconnect backoff".into(),
            retry_after_ms: Some(60_000),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let world = RetryWorld::terminal_failed("retry-backoff", Arc::clone(&fake)).await;
    let (connection, sink) = world.control().await;

    // Start the unchanged terminal-failure retry path, whose provider then
    // enters a real actor-owned 60s automatic backoff.
    let started = request(
        &connection,
        &sink,
        "retry-backoff-start-request",
        retry_request(&world, "retry-backoff-start-command"),
    )
    .await;
    let ResponseBody::RunRetry {
        run_id: retrying_run,
        user_seq,
        ..
    } = started
    else {
        panic!("terminal-failure retry starts")
    };
    assert_eq!(user_seq, world.user_seq);
    let retrying_event = wait_for_retrying_event(&world, &retrying_run).await;
    assert_eq!(
        fake.requests().len(),
        1,
        "the natural wait is still pending"
    );
    assert!(matches!(
        serde_json::from_value::<EventPayload>(retrying_event.payload.clone()),
        Ok(EventPayload::RunState(RunState::Retrying {
            attempt: 2,
            max: 10,
            delay_ms: 60_000,
            reason: WaitReason::ProviderBackoff,
        }))
    ));

    let wake = request(
        &connection,
        &sink,
        "retry-backoff-wake-request",
        retry_request(&world, "retry-backoff-wake-command"),
    )
    .await;
    let ResponseBody::RunRetry {
        run_id: wake_run,
        failed_run_id: wake_failed_run,
        user_seq: wake_user_seq,
        accepted_seq: wake_seq,
        ..
    } = wake
    else {
        panic!("mid-backoff retry is accepted")
    };
    assert_eq!(wake_run, retrying_run, "the existing run is woken");
    assert_eq!(wake_failed_run, retrying_run, "no fresh run is minted");
    assert_eq!(wake_user_seq, world.user_seq);
    assert_eq!(
        wake_seq, retrying_event.seq,
        "receipt names the exact backoff fact"
    );
    wait_for_provider_requests(&fake, 2).await;
    wait_for_state(&world, &retrying_run, RunState::Done).await;

    // Replay after the attempt has already fired/finished. Receipt lookup
    // precedes state validation and worker delivery is a fulfilled no-op.
    let replay = request(
        &connection,
        &sink,
        "retry-backoff-replay-request",
        retry_request(&world, "retry-backoff-wake-command"),
    )
    .await;
    assert!(matches!(
        replay,
        ResponseBody::RunRetry {
            run_id,
            failed_run_id,
            accepted_seq,
            ..
        } if run_id == retrying_run
            && failed_run_id == retrying_run
            && accepted_seq == retrying_event.seq
    ));
    let events = world
        .store
        .read(&world.session_id, 0, 512)
        .await
        .expect("retry history");
    assert_eq!(
        events
            .iter()
            .filter(|event| {
                RunRetryEventPayload::from_payload_value(event.payload.clone())
                    .is_ok_and(|payload| matches!(payload, RunRetryEventPayload::RunRetried { .. }))
            })
            .count(),
        1,
        "only terminal failure creates a fresh-run retry fact"
    );
    assert_eq!(fake.requests().len(), 2, "receipt replay cannot run again");
    drop(connection);
    world.shutdown().await;
}

/// MUTATION CHECK: choose only the newest nonterminal run before looking for
/// a main-timeline backoff. Expected runtime failure: the newer queued turn
/// shadows the active `Retrying` run and `run.retry` returns a live-Queued
/// refusal instead of naming and waking the exact retry event.
#[tokio::test]
async fn run_retry_queued_turn_does_not_shadow_active_backoff() {
    let fake = Arc::new(FakeProvider::new(vec![
        FakeStep::Error {
            kind: ProviderErrorKind::Overloaded,
            message: "backoff with queued work".into(),
            retry_after_ms: Some(60_000),
        },
        FakeStep::Hang,
    ]));
    let world = RetryWorld::terminal_failed("retry-queued-shadow", Arc::clone(&fake)).await;
    let (connection, sink) = world.control().await;
    let started = request(
        &connection,
        &sink,
        "retry-queued-start-request",
        retry_request(&world, "retry-queued-start-command"),
    )
    .await;
    let ResponseBody::RunRetry {
        run_id: retrying_run,
        ..
    } = started
    else {
        panic!("terminal-failure retry starts")
    };
    let retrying_event = wait_for_retrying_event(&world, &retrying_run).await;

    let queued = request(
        &connection,
        &sink,
        "retry-queued-turn-request",
        RequestBody::TurnSubmit {
            command_id: CommandId::new("retry-queued-turn-command"),
            session_id: world.session_id.clone(),
            worker_generation: world.store.worker_generation(),
            text: "wait behind the reconnect".into(),
            attachments: Vec::new(),
            mode: DeliveryMode::Queue,
        },
    )
    .await;
    let ResponseBody::TurnSubmit {
        run_id: queued_run,
        disposition,
        ..
    } = queued
    else {
        panic!("turn is queued behind active backoff")
    };
    assert_ne!(queued_run, retrying_run);
    assert_eq!(disposition, SubmitDisposition::Queued);

    let wake = request(
        &connection,
        &sink,
        "retry-queued-wake-request",
        retry_request(&world, "retry-queued-wake-command"),
    )
    .await;
    assert!(matches!(
        wake,
        ResponseBody::RunRetry {
            run_id,
            failed_run_id,
            accepted_seq,
            ..
        } if run_id == retrying_run
            && failed_run_id == retrying_run
            && accepted_seq == retrying_event.seq
    ));
    wait_for_provider_requests(&fake, 2).await;
    assert_eq!(
        fake.requests().len(),
        2,
        "the queued turn remains behind the one woken next attempt"
    );
    drop(connection);
    world.shutdown().await;
}
