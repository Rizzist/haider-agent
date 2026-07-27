//! W3c1 primary gate: production daemon/runtime over a real UnixStream.
//!
//! All thirteen numbered report-§6.1 scenarios live here, each headed
//! `Scenario N` and driven by an injected fake provider factory; no test in
//! this file may use a live API. File order: scenario 1, scenarios 3-8, the
//! worker-aware-drain satellite, scenarios 9-12, scenario 2 (the M2 prefix)
//! with its two session-create satellites, then scenario 13 — the
//! mutation-seam sweep manifest that names every load-bearing seam and the
//! focused test observing it.

#![allow(clippy::expect_used)]

mod support;

use async_trait::async_trait;
use haider_core::{CancelToken, StoreHandle, ToolDispatcher};
use haider_daemon::{
    DaemonConfig, DaemonDependencies, ProviderFactory, ResolvedTurnProvider, TurnToolFactory,
    WorkerToolContext,
};
use haider_protocol::DeliveryMode;
use haider_protocol::EventPayload;
use haider_protocol::effect::{
    AuthorizationVerdict, EffectClass, EffectIntent, EffectOutcome, EffectPhase,
};
use haider_protocol::envelope::{
    EventEnvelope, PromptRender, RawEnvelope, RenderTargets, SCHEMA_VERSION,
};
use haider_protocol::error::{ErrorCode, HaiderError};
use haider_protocol::ids::{DeviceId, EffectId, EventId, RunId, SessionId};
use haider_protocol::provider::{CapabilityDoc, FinishReason, Usage, UsageSource};
use haider_protocol::session::SessionMetadataV1;
use haider_protocol::state::{RunState, SessionState};
use haider_protocol::tool::BoundedResult;
use haider_provider::{
    FakeInputKind, FakeInputOption, FakeProvider, FakeStep, Provider, ProviderError,
    ProviderStream, ToolDefinition, TurnRequest,
};
use haider_rpc::{
    AttachMode, CancelStatus, Capability, CapabilitySet, ClientKind, CommandId,
    ERROR_CODE_ALREADY_RESOLVED, ERROR_CODE_CAPABILITY_DENIED, ERROR_CODE_INVALID_ARGUMENT,
    FEATURE_SESSION_MUTATION_V1, FEATURE_TURN_CONTROL_V1, RequestBody, RequestId, ResponseBody,
    SeqRange, SessionSummary, WireFrame,
};
use haider_store::{EventStore, Store};
use std::fs;
use std::future;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use support::{UdsClient, ready, ready_with_dependencies, test_root};
use tokio::sync::Semaphore;

#[derive(Clone)]
struct FakeFactory {
    fake: Arc<FakeProvider>,
}

#[async_trait]
impl ProviderFactory for FakeFactory {
    async fn resolve_for_turn(
        &self,
        metadata: &SessionMetadataV1,
    ) -> Result<ResolvedTurnProvider, haider_protocol::error::HaiderError> {
        Ok(ResolvedTurnProvider {
            provider: self.fake.clone(),
            provider_name: "fake".into(),
            model: metadata.model.clone(),
            account_alias: None,
        })
    }
}

fn fake_dependencies(script: Vec<FakeStep>) -> (DaemonDependencies, Arc<FakeProvider>) {
    let fake = Arc::new(FakeProvider::new(script));
    let dependencies = DaemonDependencies {
        provider_factory: Arc::new(FakeFactory { fake: fake.clone() }),
        ..DaemonDependencies::default()
    };
    (dependencies, fake)
}

#[derive(Clone)]
struct DurableEntryFactory {
    fake: Arc<FakeProvider>,
    database_path: std::path::PathBuf,
    inspections: Arc<AtomicUsize>,
}

#[async_trait]
impl ProviderFactory for DurableEntryFactory {
    async fn resolve_for_turn(
        &self,
        metadata: &SessionMetadataV1,
    ) -> Result<ResolvedTurnProvider, HaiderError> {
        Ok(ResolvedTurnProvider {
            provider: Arc::new(DurableEntryProvider {
                fake: self.fake.clone(),
                database_path: self.database_path.clone(),
                inspections: self.inspections.clone(),
            }),
            provider_name: "fake".into(),
            model: metadata.model.clone(),
            account_alias: None,
        })
    }
}

struct DurableEntryProvider {
    fake: Arc<FakeProvider>,
    database_path: std::path::PathBuf,
    inspections: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for DurableEntryProvider {
    async fn capabilities(&self) -> CapabilityDoc {
        self.fake.capabilities().await
    }

    async fn stream_turn(&self, request: TurnRequest) -> Result<ProviderStream, ProviderError> {
        {
            let connection = rusqlite::Connection::open_with_flags(
                &self.database_path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .expect("provider-entry store opens read-only");
            let mut statement = connection
                .prepare("SELECT envelope_json FROM events ORDER BY seq ASC")
                .expect("provider-entry query");
            let envelopes = statement
                .query_map([], |row| row.get::<_, String>(0))
                .expect("provider-entry rows")
                .map(|row| {
                    serde_json::from_str::<RawEnvelope>(&row.expect("stored envelope"))
                        .expect("typed stored envelope")
                })
                .collect::<Vec<_>>();
            let run = envelopes.iter().find_map(|envelope| {
                let payload =
                    serde_json::from_value::<EventPayload>(envelope.payload.clone()).ok()?;
                matches!(
                    payload,
                    EventPayload::UserMessage { ref text, .. } if text == "say hello"
                )
                .then(|| envelope.run_id.clone())
                .flatten()
            });
            let run = run.expect("UserMessage is durable before provider entry");
            assert!(envelopes.iter().any(|envelope| {
                envelope.run_id.as_ref() == Some(&run)
                    && serde_json::from_value::<EventPayload>(envelope.payload.clone())
                        .is_ok_and(|payload| payload == EventPayload::RunState(RunState::Queued))
            }));
            self.inspections.fetch_add(1, Ordering::SeqCst);
        }
        self.fake.stream_turn(request).await
    }
}

fn recovery_fixture_envelope(
    session_id: &SessionId,
    run_id: &RunId,
    generation: u64,
    event_id: &str,
    payload: EventPayload,
    prompt: PromptRender,
) -> RawEnvelope {
    EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(event_id),
        seq: 0,
        session_id: session_id.clone(),
        branch_id: None,
        run_id: Some(run_id.clone()),
        agent_id: None,
        device_id: DeviceId::new("recovery-fixture"),
        authority_epoch: 0,
        worker_generation: generation,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt,
        },
        payload: serde_json::to_value(payload).expect("recovery payload"),
    }
}

fn create_body(command_id: &str, cwd: String) -> RequestBody {
    RequestBody::SessionCreate {
        command_id: CommandId::new(command_id),
        cwd,
        provider: "fake".into(),
        model: "fake-v1".into(),
        max_tokens: 4096,
    }
}

async fn send_request(
    client: &mut UdsClient,
    config: &DaemonConfig,
    request_id: &str,
    body: RequestBody,
) {
    client
        .send(
            &WireFrame::Request {
                request_id: RequestId::new(request_id),
                body,
            },
            config.frame_limit,
        )
        .await;
}

fn created_response(frame: WireFrame) -> (haider_protocol::ids::SessionId, SessionMetadataV1) {
    match frame {
        WireFrame::Response {
            body:
                ResponseBody::SessionCreate {
                    session_id,
                    created_seq,
                    metadata,
                    ..
                },
            ..
        } => {
            assert_eq!(created_seq, 1);
            (session_id, metadata)
        }
        other => panic!("expected session.create response, got {other:?}"),
    }
}

async fn create_and_attach(
    client: &mut UdsClient,
    config: &DaemonConfig,
    workspace: &std::path::Path,
) -> (haider_protocol::ids::SessionId, u64) {
    send_request(
        client,
        config,
        "create",
        create_body("create-command", workspace.to_string_lossy().into_owned()),
    )
    .await;
    let (session_id, generation) = match client.next().await {
        WireFrame::Response {
            body:
                ResponseBody::SessionCreate {
                    session_id,
                    worker_generation,
                    ..
                },
            ..
        } => (session_id, worker_generation),
        other => panic!("expected create response, got {other:?}"),
    };
    send_request(
        client,
        config,
        "attach",
        RequestBody::SessionAttach {
            session_id: session_id.clone(),
            after_seq: 0,
            mode: AttachMode::Control,
        },
    )
    .await;
    assert!(matches!(
        client.next().await,
        WireFrame::Response {
            body: ResponseBody::SessionAttach { .. },
            ..
        }
    ));
    loop {
        if matches!(client.next().await, WireFrame::AttachCaughtUp { .. }) {
            break;
        }
    }
    (session_id, generation)
}

fn submit_body(
    command_id: &str,
    session_id: haider_protocol::ids::SessionId,
    generation: u64,
    text: &str,
) -> RequestBody {
    RequestBody::TurnSubmit {
        command_id: CommandId::new(command_id),
        session_id,
        worker_generation: generation,
        text: text.into(),
        attachments: Vec::new(),
        mode: DeliveryMode::Queue,
    }
}

async fn next_submit_response(client: &mut UdsClient) -> (haider_protocol::ids::RunId, u64) {
    loop {
        if let WireFrame::Response {
            body:
                ResponseBody::TurnSubmit {
                    run_id,
                    accepted_seq,
                    ..
                },
            ..
        } = client.next().await
        {
            return (run_id, accepted_seq);
        }
    }
}

async fn next_response(client: &mut UdsClient) -> WireFrame {
    loop {
        let frame = client.next().await;
        if matches!(frame, WireFrame::Response { .. }) {
            return frame;
        }
    }
}

async fn attach_existing(
    client: &mut UdsClient,
    config: &DaemonConfig,
    session_id: haider_protocol::ids::SessionId,
    after_seq: u64,
    request_id: &str,
) -> Vec<RawEnvelope> {
    send_request(
        client,
        config,
        request_id,
        RequestBody::SessionAttach {
            session_id: session_id.clone(),
            after_seq,
            mode: AttachMode::Control,
        },
    )
    .await;
    let mut response = false;
    let mut caught_up = false;
    let mut replay = Vec::new();
    while !(response && caught_up) {
        match client.next().await {
            WireFrame::Response {
                body: ResponseBody::SessionAttach { .. },
                ..
            } => response = true,
            WireFrame::AttachCaughtUp { .. } => caught_up = true,
            WireFrame::Event { envelope, .. } => replay.push(envelope),
            _ => {}
        }
    }
    replay
}

async fn events_until_terminal(
    client: &mut UdsClient,
    run_id: &haider_protocol::ids::RunId,
) -> Vec<(u64, EventPayload)> {
    let mut events = Vec::new();
    loop {
        if let WireFrame::Event { envelope, .. } = client.next().await {
            if envelope.run_id.as_ref() != Some(run_id) {
                continue;
            }
            let payload =
                serde_json::from_value::<EventPayload>(envelope.payload).expect("typed event");
            let terminal = matches!(
                payload,
                EventPayload::RunState(RunState::Done | RunState::Errored | RunState::Cancelled)
            );
            events.push((envelope.seq, payload));
            if terminal {
                return events;
            }
        }
    }
}

async fn next_idle(client: &mut UdsClient) -> bool {
    loop {
        if let WireFrame::Event { envelope, .. } = client.next().await
            && let Ok(EventPayload::SessionState(SessionState::Idle { interrupted })) =
                serde_json::from_value::<EventPayload>(envelope.payload)
        {
            return interrupted;
        }
    }
}

async fn read_session(
    client: &mut UdsClient,
    config: &DaemonConfig,
    session_id: SessionId,
    request_id: &str,
) -> Vec<RawEnvelope> {
    send_request(
        client,
        config,
        request_id,
        RequestBody::SessionRead {
            session_id,
            range: SeqRange {
                start_seq: 1,
                end_seq: 1_024,
            },
        },
    )
    .await;
    loop {
        if let WireFrame::Response {
            body: ResponseBody::SessionRead { result },
            ..
        } = client.next().await
        {
            return result.envelopes;
        }
    }
}

fn payloads_for_run<'a>(
    envelopes: &'a [RawEnvelope],
    run_id: &'a RunId,
) -> impl Iterator<Item = EventPayload> + 'a {
    envelopes
        .iter()
        .filter(move |envelope| envelope.run_id.as_ref() == Some(run_id))
        .filter_map(|envelope| serde_json::from_value(envelope.payload.clone()).ok())
}

/// Scenario 1: the production runtime is constructed with an injected,
/// deterministic provider factory; no live provider is reachable.
///
/// MUTATION CHECK: make `spawn_with_dependencies` (haider-daemon
/// `runtime.rs`) ignore its `dependencies` argument and construct
/// `DaemonDependencies::default()`. Expected failure: this boot still
/// passes (the pinned law here is only that injection is accepted), but
/// every turn scenario in this file fails — scenario 3 first, with a
/// `credential_missing` RunFailed instead of a streamed turn — which is why
/// the scenario-13 manifest lists scenario 3 as this seam's observer.
#[tokio::test]
async fn scenario_1_production_runtime_accepts_an_injected_fake_provider_factory() {
    let root = test_root("w3c-live-");
    let config = DaemonConfig::new(
        "injected-factory",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let (dependencies, fake) = fake_dependencies(Vec::new());
    let task = ready_with_dependencies(&config, dependencies).await;
    assert!(fake.requests().is_empty());
    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// Scenario 3.
///
/// MUTATION CHECK (three seams, one per revert):
/// - move `worker_manager()?.submit(..)` before `hub.accept_turn(..)` in
///   `turn_submit` (session_hub/rpc.rs) — expected failure: the provider
///   request races the durable prefix and the Queued-before-UserMessage-
///   before-Thinking position assertions below fail;
/// - hand the worker the raw store instead of its lease-fenced
///   `HubStoreHandle` in `start_turn` (worker.rs) — expected failure: worker
///   envelopes bypass the actor and the contiguous-sequence window check
///   fails on interleaved publication;
/// - publish before append in the actor's `WorkerAppend` arm (actor.rs) —
///   expected failure: a delivered event precedes its durable seq and the
///   contiguity/durable-read checks disagree.
#[tokio::test]
async fn scenario_3_submit_streams_one_contiguous_durable_turn_over_real_uds() {
    let root = test_root("w3c-live-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let config = DaemonConfig::new(
        "live-turn",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let usage = Usage {
        input: 11,
        output: 7,
        reasoning: 0,
        cached: 3,
        source: UsageSource::ProviderReported,
        account: None,
    };
    let fake = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitText {
            text: "hello".into(),
        },
        FakeStep::EmitUsage {
            usage: usage.clone(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let inspections = Arc::new(AtomicUsize::new(0));
    let dependencies = DaemonDependencies {
        provider_factory: Arc::new(DurableEntryFactory {
            fake: fake.clone(),
            database_path: config.store_dir.join("store.sqlite"),
            inspections: inspections.clone(),
        }),
        ..DaemonDependencies::default()
    };
    let task = ready_with_dependencies(&config, dependencies.clone()).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "turn-client",
        ClientKind::Headless,
    )
    .await;
    let (session_id, generation) = create_and_attach(&mut client, &config, &workspace).await;
    send_request(
        &mut client,
        &config,
        "submit",
        submit_body(
            "submit-command",
            session_id.clone(),
            generation,
            "say hello",
        ),
    )
    .await;

    let mut events = Vec::new();
    let mut accepted = None;
    loop {
        match client.next().await {
            WireFrame::Response {
                body:
                    ResponseBody::TurnSubmit {
                        run_id,
                        accepted_seq,
                        ..
                    },
                ..
            } => accepted = Some((run_id, accepted_seq)),
            WireFrame::Event { envelope, .. } => {
                let seq = envelope.seq;
                let payload =
                    serde_json::from_value::<EventPayload>(envelope.payload).expect("typed event");
                let terminal = payload == EventPayload::RunState(RunState::Done);
                events.push((seq, payload));
                if terminal {
                    break;
                }
            }
            _ => {}
        }
    }
    let (run_id, accepted_seq) = accepted.expect("correlated submit response");
    assert_eq!(accepted_seq, 3);
    assert_eq!(fake.requests().len(), 1);
    assert_eq!(
        inspections.load(Ordering::SeqCst),
        1,
        "provider entry inspected the already-durable acceptance prefix"
    );
    assert_eq!(
        fake.requests()[0]
            .system_prompt
            .as_deref()
            .map(|prompt| { prompt.starts_with(haider_daemon::SystemPromptBuilder::VERSION) }),
        Some(true)
    );
    for pair in events.windows(2) {
        assert_eq!(
            pair[1].0,
            pair[0].0 + 1,
            "event sequence must be contiguous"
        );
    }
    let payloads = events
        .iter()
        .map(|(_, payload)| payload)
        .collect::<Vec<_>>();
    // Position, not just presence: the acceptance transaction commits Queued
    // then UserMessage, and only then may the worker commit Thinking and
    // stream (R3's durable-before-provider order).
    let position = |predicate: &dyn Fn(&EventPayload) -> bool| {
        payloads
            .iter()
            .position(|payload| predicate(payload))
            .expect("expected payload present")
    };
    let queued = position(&|payload| *payload == EventPayload::RunState(RunState::Queued));
    let user = position(
        &|payload| matches!(payload, EventPayload::UserMessage { text, .. } if text == "say hello"),
    );
    let thinking = position(&|payload| *payload == EventPayload::RunState(RunState::Thinking));
    let streaming = position(&|payload| *payload == EventPayload::RunState(RunState::Streaming));
    assert!(queued < user, "Queued must precede UserMessage");
    assert!(user < thinking, "UserMessage must precede Thinking");
    assert!(thinking < streaming, "Thinking must precede Streaming");
    assert!(payloads.iter().any(|payload| matches!(
        payload,
        EventPayload::Item(haider_protocol::item::ItemEvent::Started { .. })
    )));
    assert!(payloads.iter().any(|payload| matches!(
        payload,
        EventPayload::Item(haider_protocol::item::ItemEvent::Delta { .. })
    )));
    assert!(payloads.iter().any(|payload| matches!(
        payload,
        EventPayload::Item(haider_protocol::item::ItemEvent::Completed { .. })
    )));
    assert!(payloads.contains(&&EventPayload::Usage(usage)));
    assert!(payloads.contains(&&EventPayload::RunState(RunState::Done)));
    assert!(!run_id.as_str().is_empty());
    assert!(
        !next_idle(&mut client).await,
        "natural completion settles non-interrupted Idle"
    );
    let durable = read_session(&mut client, &config, session_id, "full-turn-read").await;
    assert!(durable.iter().enumerate().all(|(index, envelope)| {
        envelope.seq == u64::try_from(index).expect("test index") + 1
    }));
    assert_eq!(
        serde_json::from_value::<EventPayload>(durable[0].payload.clone())
            .expect("created payload"),
        EventPayload::SessionState(SessionState::Created)
    );

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// Scenario 4.
///
/// MUTATION CHECK: skip the `turn_accept_receipt` preflight in `turn_submit`
/// (session_hub/rpc.rs), or remove `admit_pending`'s active-run compare and
/// in-queue run-id scan (worker.rs). Expected failure: the same-command
/// retry takes the provider slot reserved for the positive fence turn or
/// commits a second user message.
#[tokio::test]
async fn scenario_4_lost_submit_response_replays_one_run_and_one_provider_request() {
    let root = test_root("w3c-live-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let config = DaemonConfig::new(
        "submit-idempotency",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let (dependencies, fake) = fake_dependencies(vec![
        FakeStep::EmitText {
            text: "only once".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
        FakeStep::EmitText {
            text: "fence".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);
    let task = ready_with_dependencies(&config, dependencies.clone()).await;
    let mut first = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "lost-submit",
        ClientKind::Headless,
    )
    .await;
    let (session_id, generation) = create_and_attach(&mut first, &config, &workspace).await;
    send_request(
        &mut first,
        &config,
        "lost",
        submit_body(
            "same-submit-command",
            session_id.clone(),
            generation,
            "one turn",
        ),
    )
    .await;
    tokio::time::timeout(support::DEADLINE, async {
        while fake.requests().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("provider request begins");
    drop(first);

    let mut retry = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "retry-submit",
        ClientKind::Headless,
    )
    .await;
    attach_existing(&mut retry, &config, session_id.clone(), 0, "retry-attach").await;
    let session_for_read = session_id.clone();
    send_request(
        &mut retry,
        &config,
        "retry",
        submit_body(
            "same-submit-command",
            session_id.clone(),
            generation,
            "one turn",
        ),
    )
    .await;
    let (run_id, _) = next_submit_response(&mut retry).await;

    let events = tokio::time::timeout(support::DEADLINE, async {
        'until_terminal: loop {
            send_request(
                &mut retry,
                &config,
                "read-until-terminal",
                RequestBody::SessionRead {
                    session_id: session_for_read.clone(),
                    range: SeqRange {
                        start_seq: 1,
                        end_seq: 64,
                    },
                },
            )
            .await;
            loop {
                if let WireFrame::Response {
                    body: ResponseBody::SessionRead { result },
                    ..
                } = retry.next().await
                {
                    if result.envelopes.iter().any(|envelope| {
                        envelope.run_id.as_ref() == Some(&run_id)
                            && serde_json::from_value::<EventPayload>(envelope.payload.clone())
                                .is_ok_and(|payload| {
                                    matches!(payload, EventPayload::RunState(ref state) if state.is_terminal())
                                })
                    }) {
                        break 'until_terminal result.envelopes;
                    }
                    break;
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("original run becomes durably terminal");
    assert_eq!(
        events
            .iter()
            .filter(|envelope| {
                envelope.run_id.as_ref() == Some(&run_id)
                    && serde_json::from_value::<EventPayload>(envelope.payload.clone())
                        .is_ok_and(|payload| matches!(payload, EventPayload::UserMessage { .. }))
            })
            .count(),
        1
    );

    // Positive quiescence fence: this distinct turn entered the manager
    // after the replay hint. Supervisor FIFO plus serial execution means the
    // second provider request must be this turn; a duplicate queued from the
    // replay would necessarily take that slot first.
    send_request(
        &mut retry,
        &config,
        "fence",
        submit_body("fence-command", session_id, generation, "fence turn"),
    )
    .await;
    let _ = next_submit_response(&mut retry).await;
    tokio::time::timeout(support::DEADLINE, async {
        while fake.requests().len() < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("fence provider request begins");
    let requests = fake.requests();
    assert!(requests[1].messages.iter().any(|message| {
        message.blocks.iter().any(
            |block| matches!(block, haider_protocol::provider::Block::Text { text } if text == "fence turn"),
        )
    }));

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
    assert_eq!(fake.requests().len(), 2);

    let restarted = ready_with_dependencies(&config, dependencies).await;
    let mut after_restart = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "retry-submit-after-restart",
        ClientKind::Headless,
    )
    .await;
    attach_existing(
        &mut after_restart,
        &config,
        session_for_read.clone(),
        0,
        "retry-restart-attach",
    )
    .await;
    send_request(
        &mut after_restart,
        &config,
        "retry-after-restart",
        submit_body(
            "same-submit-command",
            session_for_read,
            generation,
            "one turn",
        ),
    )
    .await;
    let (replayed_run, _) = next_submit_response(&mut after_restart).await;
    assert_eq!(replayed_run, run_id);
    restarted.shutdown_handle().request("test complete");
    restarted.join().await.expect("restarted daemon joins");
    assert_eq!(
        fake.requests().len(),
        2,
        "old-generation receipt replay is response-only"
    );
}

/// Scenario 5.
///
/// MUTATION CHECK: make `PromptHistoryCompiler::compile`
/// (haider-core/src/prompt_history.rs) return only the current user message,
/// or drop its Done-runs-only terminal filter. Expected failure: request two
/// lacks the completed first exchange, or includes non-terminal content.
/// This live scenario runs one head-turn identity; the negative
/// branch/agent and nonterminal exclusions are pinned separately in the
/// `haider-core` MemoryStore prompt-history test.
#[tokio::test]
async fn scenario_5_second_turn_contains_prior_completed_conversation() {
    let root = test_root("w3c-live-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let config = DaemonConfig::new(
        "conversation",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let (dependencies, fake) = fake_dependencies(vec![
        FakeStep::EmitText {
            text: "first answer".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
        FakeStep::EmitText {
            text: "second answer".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);
    let task = ready_with_dependencies(&config, dependencies).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "history-client",
        ClientKind::Headless,
    )
    .await;
    let (session_id, generation) = create_and_attach(&mut client, &config, &workspace).await;
    send_request(
        &mut client,
        &config,
        "first",
        submit_body(
            "first-command",
            session_id.clone(),
            generation,
            "first question",
        ),
    )
    .await;
    let (first_run, _) = next_submit_response(&mut client).await;
    let _ = events_until_terminal(&mut client, &first_run).await;

    send_request(
        &mut client,
        &config,
        "second",
        submit_body("second-command", session_id, generation, "second question"),
    )
    .await;
    let (second_run, _) = next_submit_response(&mut client).await;
    let _ = events_until_terminal(&mut client, &second_run).await;
    let requests = fake.requests();
    assert_eq!(requests.len(), 2);
    let second = &requests[1].messages;
    assert!(second.iter().any(|message| {
        message.blocks.iter().any(|block| {
            matches!(
                block,
                haider_protocol::provider::Block::Text { text } if text == "first question"
            )
        })
    }));
    assert!(second.iter().any(|message| {
        message.blocks.iter().any(|block| {
            matches!(
                block,
                haider_protocol::provider::Block::Text { text } if text == "first answer"
            )
        })
    }));
    assert!(second.iter().any(|message| {
        message.blocks.iter().any(|block| {
            matches!(
                block,
                haider_protocol::provider::Block::Text { text } if text == "second question"
            )
        })
    }));

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// Scenario 6.
///
/// MUTATION CHECK: wake the harness before the menu CAS commits, append the
/// answer again in core, or issue the next provider request without the tool
/// result. Expected failure: duplicate MenuAnswered/ToolResult or request two
/// lacks the selected value.
#[tokio::test]
async fn scenario_6_request_input_round_trip_uses_second_control_attachment() {
    let root = test_root("w3c-live-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let config = DaemonConfig::new(
        "menu-round-trip",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let (dependencies, fake) = fake_dependencies(vec![
        FakeStep::EmitRequestInput {
            call_id: "choose-1".into(),
            kind: FakeInputKind::Choice,
            title: "Choose".into(),
            body: vec!["Pick one".into()],
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
            call_id: "choose-1".into(),
        },
        FakeStep::EmitText {
            text: "continued".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);
    let task = ready_with_dependencies(&config, dependencies).await;
    let mut submitter = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "menu-submitter",
        ClientKind::Headless,
    )
    .await;
    let (session_id, generation) = create_and_attach(&mut submitter, &config, &workspace).await;
    send_request(
        &mut submitter,
        &config,
        "submit",
        submit_body("menu-submit", session_id.clone(), generation, "ask me"),
    )
    .await;
    let (run_id, _) = next_submit_response(&mut submitter).await;
    let (menu_id, request_seq, opening_generation) = loop {
        if let WireFrame::Event { envelope, .. } = submitter.next().await
            && let Ok(EventPayload::MenuOpened(menu)) =
                serde_json::from_value::<EventPayload>(envelope.payload)
        {
            break (menu.id, envelope.seq, envelope.worker_generation);
        }
    };

    let mut answerer = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "menu-answerer",
        ClientKind::Headless,
    )
    .await;
    attach_existing(
        &mut answerer,
        &config,
        session_id.clone(),
        request_seq,
        "answer-attach",
    )
    .await;
    answerer
        .send(
            &WireFrame::MenuAnswer {
                request_id: Some(RequestId::new("answer")),
                command_id: CommandId::new("answer-command"),
                session_id: session_id.clone(),
                menu_id: menu_id.clone(),
                request_seq,
                worker_generation: opening_generation,
                option_key: "yes".into(),
                option_index: 0,
                input: None,
            },
            config.frame_limit,
        )
        .await;
    assert!(matches!(
        next_response(&mut answerer).await,
        WireFrame::Response {
            body: ResponseBody::MenuAnswer { .. },
            ..
        }
    ));
    let events = events_until_terminal(&mut submitter, &run_id).await;
    assert!(matches!(
        events.last(),
        Some((_, EventPayload::RunState(RunState::Done)))
    ));
    assert_eq!(
        events
            .iter()
            .filter(|(_, payload)| matches!(payload, EventPayload::MenuAnswered(_)))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|(_, payload)| matches!(payload, EventPayload::ToolResult { .. }))
            .count(),
        1
    );
    let requests = fake.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|message| message.tool_result_for("choose-1").is_some())
    );

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// Scenario 7.
///
/// MUTATION CHECK: decide the winner in memory or wake both callers before
/// SQLite's first-committed-wins CAS. Expected failure: two successful
/// responses, two durable answers, or two follow-up provider requests.
#[tokio::test]
async fn scenario_7_two_menu_answers_race_and_only_first_commit_wins() {
    let root = test_root("w3c-live-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let config = DaemonConfig::new(
        "menu-race",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let (dependencies, fake) = fake_dependencies(vec![
        FakeStep::EmitRequestInput {
            call_id: "race-1".into(),
            kind: FakeInputKind::Choice,
            title: "Race".into(),
            body: Vec::new(),
            options: vec![
                FakeInputOption {
                    key: "a".into(),
                    label: "A".into(),
                    detail: None,
                },
                FakeInputOption {
                    key: "b".into(),
                    label: "B".into(),
                    detail: None,
                },
            ],
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "race-1".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);
    let task = ready_with_dependencies(&config, dependencies).await;
    let mut submitter = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "race-submitter",
        ClientKind::Headless,
    )
    .await;
    let (session_id, generation) = create_and_attach(&mut submitter, &config, &workspace).await;
    send_request(
        &mut submitter,
        &config,
        "submit",
        submit_body("race-submit", session_id.clone(), generation, "race"),
    )
    .await;
    let (run_id, _) = next_submit_response(&mut submitter).await;
    let (menu_id, request_seq, opening_generation) = loop {
        if let WireFrame::Event { envelope, .. } = submitter.next().await
            && let Ok(EventPayload::MenuOpened(menu)) =
                serde_json::from_value::<EventPayload>(envelope.payload)
        {
            break (menu.id, envelope.seq, envelope.worker_generation);
        }
    };
    let mut first = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "race-a",
        ClientKind::Headless,
    )
    .await;
    let mut second = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "race-b",
        ClientKind::Headless,
    )
    .await;
    attach_existing(
        &mut first,
        &config,
        session_id.clone(),
        request_seq,
        "attach-a",
    )
    .await;
    attach_existing(
        &mut second,
        &config,
        session_id.clone(),
        request_seq,
        "attach-b",
    )
    .await;
    let answer_a = WireFrame::MenuAnswer {
        request_id: Some(RequestId::new("answer-a")),
        command_id: CommandId::new("answer-command-a"),
        session_id: session_id.clone(),
        menu_id: menu_id.clone(),
        request_seq,
        worker_generation: opening_generation,
        option_key: "a".into(),
        option_index: 0,
        input: None,
    };
    let answer_b = WireFrame::MenuAnswer {
        request_id: Some(RequestId::new("answer-b")),
        command_id: CommandId::new("answer-command-b"),
        session_id,
        menu_id,
        request_seq,
        worker_generation: opening_generation,
        option_key: "b".into(),
        option_index: 1,
        input: None,
    };
    tokio::join!(
        first.send(&answer_a, config.frame_limit),
        second.send(&answer_b, config.frame_limit)
    );
    let responses = [
        next_response(&mut first).await,
        next_response(&mut second).await,
    ];
    assert_eq!(
        responses
            .iter()
            .filter(|frame| matches!(
                frame,
                WireFrame::Response {
                    body: ResponseBody::MenuAnswer { .. },
                    ..
                }
            ))
            .count(),
        1
    );
    assert_eq!(
        responses
            .iter()
            .filter(|frame| matches!(
                frame,
                WireFrame::Response {
                    body: ResponseBody::Error { code, .. },
                    ..
                } if code == ERROR_CODE_ALREADY_RESOLVED
            ))
            .count(),
        1
    );
    let events = events_until_terminal(&mut submitter, &run_id).await;
    assert_eq!(
        events
            .iter()
            .filter(|(_, payload)| matches!(payload, EventPayload::MenuAnswered(_)))
            .count(),
        1
    );
    assert_eq!(fake.requests().len(), 2);

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// Scenario 8.
///
/// MUTATION CHECK: signal cancellation before committing Cancelling, let the
/// provider stream win a buffered tie, or leave an open item uncompleted.
/// Expected failure: ordering/item-lifecycle assertions fail or a run event
/// appears after Cancelled.
#[tokio::test]
async fn scenario_8_wire_cancel_closes_open_items_and_cancelled_is_run_terminal() {
    let root = test_root("w3c-live-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let config = DaemonConfig::new(
        "turn-cancel",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let (dependencies, fake) = fake_dependencies(vec![
        FakeStep::EmitText {
            text: "partial".into(),
        },
        FakeStep::Hang,
    ]);
    let task = ready_with_dependencies(&config, dependencies).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "cancel-client",
        ClientKind::Headless,
    )
    .await;
    let (session_id, generation) = create_and_attach(&mut client, &config, &workspace).await;
    send_request(
        &mut client,
        &config,
        "submit",
        submit_body("cancel-submit", session_id.clone(), generation, "hang"),
    )
    .await;
    let (run_id, _) = next_submit_response(&mut client).await;
    let mut events = Vec::new();
    loop {
        if let WireFrame::Event { envelope, .. } = client.next().await
            && envelope.run_id.as_ref() == Some(&run_id)
        {
            let payload =
                serde_json::from_value::<EventPayload>(envelope.payload).expect("typed event");
            let has_delta = matches!(
                payload,
                EventPayload::Item(haider_protocol::item::ItemEvent::Delta { .. })
            );
            events.push((envelope.seq, payload));
            if has_delta {
                break;
            }
        }
    }
    send_request(
        &mut client,
        &config,
        "cancel",
        RequestBody::TurnCancel {
            command_id: CommandId::new("cancel-command"),
            session_id: session_id.clone(),
            worker_generation: generation,
            run_id: run_id.clone(),
        },
    )
    .await;
    let mut response_seen = false;
    let cancelled_seq = loop {
        match client.next().await {
            WireFrame::Response {
                body:
                    ResponseBody::TurnCancel {
                        status: CancelStatus::Accepted,
                        ..
                    },
                ..
            } => response_seen = true,
            WireFrame::Event { envelope, .. } if envelope.run_id.as_ref() == Some(&run_id) => {
                let payload =
                    serde_json::from_value::<EventPayload>(envelope.payload).expect("typed event");
                let terminal = payload == EventPayload::RunState(RunState::Cancelled);
                let seq = envelope.seq;
                events.push((seq, payload));
                if terminal {
                    break seq;
                }
            }
            _ => {}
        }
    };
    assert!(response_seen);
    let cancelling = events
        .iter()
        .position(|(_, payload)| *payload == EventPayload::RunState(RunState::Cancelling))
        .expect("durable cancelling");
    let cancelled = events
        .iter()
        .position(|(_, payload)| *payload == EventPayload::RunState(RunState::Cancelled))
        .expect("durable cancelled");
    assert!(cancelling < cancelled);
    assert!(events.iter().any(|(_, payload)| matches!(
        payload,
        EventPayload::Item(haider_protocol::item::ItemEvent::Completed { .. })
    )));
    let durable = read_session(&mut client, &config, session_id, "cancel-durable-read").await;
    let run_envelopes = durable
        .iter()
        .filter(|envelope| envelope.run_id.as_ref() == Some(&run_id))
        .collect::<Vec<_>>();
    let cancelled_index = run_envelopes
        .iter()
        .position(|envelope| envelope.seq == cancelled_seq)
        .expect("durable Cancelled");
    assert_eq!(
        cancelled_index + 1,
        run_envelopes.len(),
        "no durable run event may follow Cancelled"
    );
    let payloads = run_envelopes
        .iter()
        .filter_map(|envelope| {
            serde_json::from_value::<EventPayload>(envelope.payload.clone()).ok()
        })
        .collect::<Vec<_>>();
    for started in payloads.iter().filter_map(|payload| match payload {
        EventPayload::Item(haider_protocol::item::ItemEvent::Started { item_id, .. }) => {
            Some(item_id)
        }
        _ => None,
    }) {
        assert!(payloads.iter().any(|payload| matches!(
            payload,
            EventPayload::Item(haider_protocol::item::ItemEvent::Completed { item_id, .. })
                if item_id == started
        )));
    }
    assert_eq!(fake.requests().len(), 1);

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

#[derive(Clone)]
struct BlockingProviderFactory {
    entered: Arc<Semaphore>,
    release: Arc<Semaphore>,
    fake: Arc<FakeProvider>,
}

#[async_trait]
impl ProviderFactory for BlockingProviderFactory {
    async fn resolve_for_turn(
        &self,
        metadata: &SessionMetadataV1,
    ) -> Result<ResolvedTurnProvider, HaiderError> {
        self.entered.add_permits(1);
        self.release
            .acquire()
            .await
            .expect("test release semaphore")
            .forget();
        Ok(ResolvedTurnProvider {
            provider: self.fake.clone(),
            provider_name: "fake".into(),
            model: metadata.model.clone(),
            account_alias: None,
        })
    }
}

/// Exact P1-2 schedule: provider resolution is blocked, wire cancellation
/// durably commits, then resolution is released. No provider request may
/// begin after that durable fence.
///
/// MUTATION CHECK: make `cancellation_fences_start` return false (its focused
/// law test fails); this controlled schedule separately proves the live call
/// site reaches `Cancelled` with zero provider requests. Verified by revert
/// on 2026-07-27.
#[tokio::test]
async fn cancelling_while_provider_factory_is_blocked_never_starts_provider() {
    let root = test_root("w3c-live-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let config = DaemonConfig::new(
        "cancel-start-fence",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let entered = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    let fake = Arc::new(FakeProvider::new(vec![FakeStep::Finish {
        reason: FinishReason::EndTurn,
    }]));
    let task = ready_with_dependencies(
        &config,
        DaemonDependencies {
            provider_factory: Arc::new(BlockingProviderFactory {
                entered: entered.clone(),
                release: release.clone(),
                fake: fake.clone(),
            }),
            ..DaemonDependencies::default()
        },
    )
    .await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "cancel-start-client",
        ClientKind::Headless,
    )
    .await;
    let (session_id, generation) = create_and_attach(&mut client, &config, &workspace).await;
    send_request(
        &mut client,
        &config,
        "start-fence-submit",
        submit_body(
            "start-fence-submit-command",
            session_id.clone(),
            generation,
            "do not start after cancel",
        ),
    )
    .await;
    let (run_id, _) = next_submit_response(&mut client).await;
    entered.acquire().await.expect("factory entry").forget();
    send_request(
        &mut client,
        &config,
        "start-fence-cancel",
        RequestBody::TurnCancel {
            command_id: CommandId::new("start-fence-cancel-command"),
            session_id,
            worker_generation: generation,
            run_id: run_id.clone(),
        },
    )
    .await;
    loop {
        if matches!(
            client.next().await,
            WireFrame::Response {
                body: ResponseBody::TurnCancel {
                    status: CancelStatus::Accepted,
                    ..
                },
                ..
            }
        ) {
            break;
        }
    }
    release.add_permits(1);
    let events = events_until_terminal(&mut client, &run_id).await;
    assert!(matches!(
        events.last(),
        Some((_, EventPayload::RunState(RunState::Cancelled)))
    ));
    assert!(
        next_idle(&mut client).await,
        "user cancellation settles interrupted Idle"
    );
    assert!(fake.requests().is_empty());
    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

#[derive(Clone)]
struct ClosingHeldEffectFactory {
    effect: EffectId,
    dispatched: Arc<Semaphore>,
}

#[async_trait]
impl TurnToolFactory for ClosingHeldEffectFactory {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "held_effect".into(),
            description: "Hold after durable dispatch until cancellation".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }]
    }

    async fn create(
        &self,
        context: WorkerToolContext,
    ) -> Result<Option<Arc<dyn ToolDispatcher>>, HaiderError> {
        Ok(Some(Arc::new(ClosingHeldEffectDispatcher {
            context,
            effect: self.effect.clone(),
            dispatched: self.dispatched.clone(),
        })))
    }
}

struct ClosingHeldEffectDispatcher {
    context: WorkerToolContext,
    effect: EffectId,
    dispatched: Arc<Semaphore>,
}

impl ClosingHeldEffectDispatcher {
    async fn append(&self, suffix: &str, payload: EventPayload) -> Result<(), HaiderError> {
        let mut envelopes = [EventEnvelope {
            schema_version: SCHEMA_VERSION,
            event_id: EventId::new(format!("live-held-{}-{suffix}", self.effect)),
            seq: 0,
            session_id: self.context.store.session_id().clone(),
            branch_id: None,
            run_id: Some(self.context.run_id.clone()),
            agent_id: None,
            device_id: self.context.device_id.clone(),
            authority_epoch: 0,
            worker_generation: self.context.store.worker_generation(),
            causation_id: None,
            correlation_id: None,
            committed_at_ms: 0,
            render: RenderTargets {
                ui: true,
                durable: true,
                prompt: PromptRender::Omit,
            },
            payload: serde_json::to_value(payload).expect("effect payload"),
        }];
        StoreHandle::append(&self.context.store, &mut envelopes).await?;
        Ok(())
    }
}

#[async_trait]
impl ToolDispatcher for ClosingHeldEffectDispatcher {
    async fn execute(
        &self,
        _run_id: &RunId,
        _call_id: &str,
        _name: &str,
        _args: serde_json::Value,
        _cancel: &CancelToken,
    ) -> Result<BoundedResult, HaiderError> {
        self.append(
            "dispatched",
            EventPayload::Effect(EffectPhase::Dispatched {
                effect: self.effect.clone(),
            }),
        )
        .await?;
        self.dispatched.add_permits(1);
        future::pending().await
    }

    async fn close(&self) -> Result<(), HaiderError> {
        // Exact close-failure schedule: the dispatcher reports failure before
        // writing an outcome. The supervisor must reduce durable truth and
        // synthesize Unknown itself before it may commit Cancelled.
        Err(HaiderError::new(
            ErrorCode::EffectUnknownOutcome,
            "injected dispatcher close failure",
            true,
        ))
    }
}

/// Exact P1-1 schedule: cancellation drops a held dispatched execution,
/// dispatcher close fails without recording an outcome, durable
/// reconciliation appends Unknown, and only then may Cancelled commit.
///
/// MUTATION CHECK: remove `reconcile_unknown_effects` or restore the terminal
/// commit before it. Expected failure: Unknown is absent or follows
/// Cancelled. Verified by revert in W3c1.1.
#[tokio::test]
async fn held_effect_reconciles_unknown_before_cancelled() {
    let root = test_root("w3c-live-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let config = DaemonConfig::new(
        "live-held-cancel",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let dispatched = Arc::new(Semaphore::new(0));
    let effect = EffectId::new("live-held-cancel-effect");
    let (dependencies, _fake) = fake_dependencies(vec![FakeStep::EmitToolCall {
        call_id: "held-call".into(),
        name: "held_effect".into(),
        args: serde_json::json!({}),
    }]);
    let task = ready_with_dependencies(
        &config,
        DaemonDependencies {
            tool_factory: Arc::new(ClosingHeldEffectFactory {
                effect: effect.clone(),
                dispatched: dispatched.clone(),
            }),
            ..dependencies
        },
    )
    .await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "held-cancel-client",
        ClientKind::Headless,
    )
    .await;
    let (session_id, generation) = create_and_attach(&mut client, &config, &workspace).await;
    send_request(
        &mut client,
        &config,
        "held-submit",
        submit_body(
            "held-submit-command",
            session_id.clone(),
            generation,
            "dispatch and hold",
        ),
    )
    .await;
    let (run_id, _) = next_submit_response(&mut client).await;
    dispatched
        .acquire()
        .await
        .expect("dispatch commits")
        .forget();
    send_request(
        &mut client,
        &config,
        "held-cancel",
        RequestBody::TurnCancel {
            command_id: CommandId::new("held-cancel-command"),
            session_id: session_id.clone(),
            worker_generation: generation,
            run_id: run_id.clone(),
        },
    )
    .await;
    let _events = events_until_terminal(&mut client, &run_id).await;
    assert!(
        next_idle(&mut client).await,
        "user cancellation settles interrupted Idle"
    );
    let durable = read_session(&mut client, &config, session_id, "held-cancel-read").await;
    let events = durable
        .into_iter()
        .filter(|envelope| envelope.run_id.as_ref() == Some(&run_id))
        .filter_map(|envelope| {
            serde_json::from_value::<EventPayload>(envelope.payload)
                .ok()
                .map(|payload| (envelope.seq, payload))
        })
        .collect::<Vec<_>>();
    let position = |predicate: &dyn Fn(&EventPayload) -> bool| {
        events
            .iter()
            .position(|(_, payload)| predicate(payload))
            .expect("expected event")
    };
    let dispatched = position(&|payload| {
        matches!(
            payload,
            EventPayload::Effect(EffectPhase::Dispatched { effect: candidate })
                if *candidate == effect
        )
    });
    let unknown = position(&|payload| {
        matches!(
            payload,
            EventPayload::Effect(EffectPhase::Outcome {
                effect: candidate,
                outcome: EffectOutcome::Unknown,
            }) if *candidate == effect
        )
    });
    let cancelled = position(&|payload| *payload == EventPayload::RunState(RunState::Cancelled));
    assert!(dispatched < unknown && unknown < cancelled);
    assert!(matches!(
        events.last(),
        Some((_, EventPayload::RunState(RunState::Cancelled)))
    ));
    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// Worker-aware drain satellite (report §6.1 implementation bullet
/// "external admission gate and worker-aware drain", R9): a queued run that
/// never started must reach a durable terminal state during the drain grace,
/// not evaporate with the in-memory queue.
///
/// MUTATION CHECK: replace the `cancel_durable_queued_turns(..)` call with a
/// bare queue drop in either `run_supervisor` shutdown arm (worker.rs).
/// Expected failure: the accepted queued run has no terminal state after the
/// worker-aware drain completes.
#[tokio::test]
async fn worker_aware_drain_terminalizes_durable_queued_turns_before_store_close() {
    let root = test_root("w3c-live-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let config = DaemonConfig::new(
        "worker-aware-drain",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let (dependencies, fake) = fake_dependencies(vec![
        FakeStep::EmitText {
            text: "active".into(),
        },
        FakeStep::Hang,
    ]);
    let task = ready_with_dependencies(&config, dependencies).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "drain-client",
        ClientKind::Headless,
    )
    .await;
    let (session_id, generation) = create_and_attach(&mut client, &config, &workspace).await;
    send_request(
        &mut client,
        &config,
        "drain-active",
        submit_body(
            "drain-active-command",
            session_id.clone(),
            generation,
            "active turn",
        ),
    )
    .await;
    let (_active_run, _) = next_submit_response(&mut client).await;
    tokio::time::timeout(support::DEADLINE, async {
        while fake.requests().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("active provider request");
    send_request(
        &mut client,
        &config,
        "drain-queued",
        submit_body(
            "drain-queued-command",
            session_id.clone(),
            generation,
            "queued turn",
        ),
    )
    .await;
    let (queued_run, _) = next_submit_response(&mut client).await;
    drop(client);
    task.shutdown_handle().request("test drain");
    task.join().await.expect("daemon joins");

    let store = Store::open(&config.store_dir).expect("inspect drained store");
    let envelopes = store.journal_replay(&session_id).expect("drained replay");
    assert!(
        payloads_for_run(&envelopes, &queued_run)
            .any(|payload| payload == EventPayload::RunState(RunState::Cancelled))
    );
    assert_eq!(fake.requests().len(), 1);
}

/// Scenario 9.
///
/// MUTATION CHECK: classify a prior-generation Streaming run as resumable,
/// or fail to rediscover a prior-generation Queued run. Expected failure:
/// the interrupted prompt is sent twice, the queued prompt is never sent, or
/// the interrupted run lacks its durable recovery failure/terminal state.
#[tokio::test]
async fn scenario_9_restart_resumes_only_queued_and_terminalizes_streaming() {
    let root = test_root("w3c-live-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let config = DaemonConfig::new(
        "restart-queued-only",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let (dependencies, fake) = fake_dependencies(vec![
        FakeStep::EmitText {
            text: "partial before crash".into(),
        },
        FakeStep::Hang,
        FakeStep::EmitText {
            text: "queued resumed".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);
    let first_task = ready_with_dependencies(&config, dependencies.clone()).await;
    let mut first = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "restart-before",
        ClientKind::Headless,
    )
    .await;
    let (session_id, generation) = create_and_attach(&mut first, &config, &workspace).await;
    send_request(
        &mut first,
        &config,
        "streaming-submit",
        submit_body(
            "streaming-command",
            session_id.clone(),
            generation,
            "interrupted prompt",
        ),
    )
    .await;
    let (streaming_run, _) = next_submit_response(&mut first).await;
    loop {
        if let WireFrame::Event { envelope, .. } = first.next().await
            && envelope.run_id.as_ref() == Some(&streaming_run)
            && serde_json::from_value::<EventPayload>(envelope.payload).is_ok_and(|payload| {
                matches!(
                    payload,
                    EventPayload::Item(haider_protocol::item::ItemEvent::Delta { .. })
                )
            })
        {
            break;
        }
    }
    send_request(
        &mut first,
        &config,
        "queued-submit",
        submit_body(
            "queued-command",
            session_id.clone(),
            generation,
            "queued prompt",
        ),
    )
    .await;
    let (queued_run, _) = next_submit_response(&mut first).await;
    assert_eq!(fake.requests().len(), 1, "queued turn has not started");

    drop(first);
    first_task.crash().await;

    let second_task = ready_with_dependencies(&config, dependencies).await;
    let mut second = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "restart-after",
        ClientKind::Headless,
    )
    .await;
    attach_existing(
        &mut second,
        &config,
        session_id.clone(),
        0,
        "restart-attach",
    )
    .await;
    let _ = events_until_terminal(&mut second, &queued_run).await;
    let envelopes = read_session(&mut second, &config, session_id, "restart-read-terminal").await;
    let streaming = payloads_for_run(&envelopes, &streaming_run).collect::<Vec<_>>();
    let queued = payloads_for_run(&envelopes, &queued_run).collect::<Vec<_>>();
    assert!(streaming.iter().any(|payload| matches!(
        payload,
        EventPayload::RunFailed { message, .. } if message.contains("interrupted")
    )));
    assert!(streaming.contains(&EventPayload::RunState(RunState::Errored)));
    assert!(queued.contains(&EventPayload::RunState(RunState::Done)));
    let requests = fake.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].messages.iter().any(|message| {
        message.blocks.iter().any(|block| {
            matches!(
                block,
                haider_protocol::provider::Block::Text { text } if text == "interrupted prompt"
            )
        })
    }));
    assert!(requests[1].messages.iter().any(|message| {
        message.blocks.iter().any(|block| {
            matches!(
                block,
                haider_protocol::provider::Block::Text { text } if text == "queued prompt"
            )
        })
    }));

    second_task.shutdown_handle().request("test complete");
    second_task.join().await.expect("daemon joins");
}

/// Startup poison fixture (B2).
///
/// MUTATION CHECK: return metadata-less prior-generation Queued work from
/// `recover_startup` instead of terminalizing it. Expected failure: the
/// daemon stops before Ready. Verified by revert in W3c1.1.
#[tokio::test]
async fn metadata_less_prior_generation_queued_run_terminalizes_and_reaches_ready() {
    let root = test_root("w3c-live-");
    let config = DaemonConfig::new(
        "poison-session",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let session_id = SessionId::new("poison-session");
    let run_id = RunId::new("poison-run");
    {
        let store = Store::open(&config.store_dir).expect("seed store");
        let generation = store.worker_generation();
        let mut events = vec![
            recovery_fixture_envelope(
                &session_id,
                &run_id,
                generation,
                "poison-queued",
                EventPayload::RunState(RunState::Queued),
                PromptRender::Omit,
            ),
            recovery_fixture_envelope(
                &session_id,
                &run_id,
                generation,
                "poison-user",
                EventPayload::UserMessage {
                    text: "cannot resume without metadata".into(),
                    attachments: Vec::new(),
                    mode: DeliveryMode::Queue,
                },
                PromptRender::Verbatim,
            ),
        ];
        store.append(&mut events).expect("poison prefix");
    }

    let (dependencies, fake) = fake_dependencies(vec![FakeStep::Hang]);
    let task = ready_with_dependencies(&config, dependencies).await;
    task.shutdown_handle().request("fixture inspected");
    task.join().await.expect("daemon joins");
    assert!(fake.requests().is_empty());

    let store = Store::open(&config.store_dir).expect("inspect recovery");
    let events = store.journal_replay(&session_id).expect("history");
    let payloads = payloads_for_run(&events, &run_id).collect::<Vec<_>>();
    assert!(payloads.contains(&EventPayload::RunState(RunState::Errored)));
    assert!(payloads.iter().any(|payload| matches!(
        payload,
        EventPayload::RunFailed {
            code: ErrorCode::Internal,
            ..
        }
    )));
    assert!(events.iter().any(|envelope| {
        matches!(
            serde_json::from_value::<EventPayload>(envelope.payload.clone()),
            Ok(EventPayload::SessionState(SessionState::Idle {
                interrupted: true
            }))
        )
    }));
}

/// B1 wire pin: a legacy session without typed metadata is rejected with the
/// caller's request correlation before any Queued acceptance can commit.
#[tokio::test]
async fn metadata_less_live_submit_is_correlated_invalid_argument_without_acceptance() {
    let root = test_root("w3c-live-");
    let config = DaemonConfig::new(
        "legacy-live-submit",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let session_id = SessionId::new("legacy-live-session");
    {
        let store = Store::open(&config.store_dir).expect("seed store");
        let mut events = [EventEnvelope {
            schema_version: SCHEMA_VERSION,
            event_id: EventId::new("legacy-idle"),
            seq: 0,
            session_id: session_id.clone(),
            branch_id: None,
            run_id: None,
            agent_id: None,
            device_id: DeviceId::new("legacy-fixture"),
            authority_epoch: 0,
            worker_generation: store.worker_generation(),
            causation_id: None,
            correlation_id: None,
            committed_at_ms: 0,
            render: RenderTargets {
                ui: true,
                durable: true,
                prompt: PromptRender::Omit,
            },
            payload: serde_json::to_value(EventPayload::SessionState(SessionState::Idle {
                interrupted: false,
            }))
            .expect("idle payload"),
        }];
        store.append(&mut events).expect("legacy session row");
    }

    let task = ready(&config).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "legacy-submit-client",
        ClientKind::Headless,
    )
    .await;
    send_request(
        &mut client,
        &config,
        "legacy-list",
        RequestBody::SessionList {
            cursor: None,
            limit: 10,
        },
    )
    .await;
    let generation = match client.next().await {
        WireFrame::Response {
            body: ResponseBody::SessionList { sessions, .. },
            ..
        } => {
            sessions
                .into_iter()
                .find(|summary| summary.session_id == session_id)
                .expect("legacy summary")
                .worker_generation
        }
        other => panic!("expected session list, got {other:?}"),
    };
    let _ = attach_existing(&mut client, &config, session_id.clone(), 0, "legacy-attach").await;
    send_request(
        &mut client,
        &config,
        "legacy-submit-request",
        submit_body(
            "legacy-submit-command",
            session_id.clone(),
            generation,
            "must not commit",
        ),
    )
    .await;
    let rejection = client.next().await;
    assert!(
        matches!(
        rejection,
        WireFrame::Response {
            ref request_id,
            body: ResponseBody::Error { ref code, .. },
        } if *request_id == RequestId::new("legacy-submit-request")
            && code == ERROR_CODE_INVALID_ARGUMENT
        ),
        "unexpected legacy-submit response: {rejection:?}"
    );
    task.shutdown_handle().request("fixture inspected");
    task.join().await.expect("daemon joins");

    let store = Store::open(&config.store_dir).expect("inspect store");
    let events = store.journal_replay(&session_id).expect("history");
    assert!(!events.iter().any(|envelope| {
        serde_json::from_value::<EventPayload>(envelope.payload.clone())
            .is_ok_and(|payload| payload == EventPayload::RunState(RunState::Queued))
    }));
}

#[derive(Clone)]
struct RevokedCredentialFactory;

#[async_trait]
impl ProviderFactory for RevokedCredentialFactory {
    async fn resolve_for_turn(
        &self,
        _metadata: &SessionMetadataV1,
    ) -> Result<ResolvedTurnProvider, HaiderError> {
        Err(HaiderError::new(
            ErrorCode::CredentialMissing,
            "test credential was revoked",
            true,
        ))
    }
}

#[derive(Clone)]
struct PanicOnceFactory {
    calls: Arc<AtomicUsize>,
    fake: Arc<FakeProvider>,
}

#[async_trait]
impl ProviderFactory for PanicOnceFactory {
    async fn resolve_for_turn(
        &self,
        metadata: &SessionMetadataV1,
    ) -> Result<ResolvedTurnProvider, HaiderError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            panic!("injected provider factory panic");
        }
        Ok(ResolvedTurnProvider {
            provider: self.fake.clone(),
            provider_name: "fake".into(),
            model: metadata.model.clone(),
            account_alias: None,
        })
    }
}

#[tokio::test]
async fn panicked_supervisor_terminalizes_run_and_fresh_incarnation_is_usable() {
    let root = test_root("w3c-live-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let config = DaemonConfig::new(
        "panic-eviction",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let fake = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitText {
            text: "fresh supervisor".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let dependencies = DaemonDependencies {
        provider_factory: Arc::new(PanicOnceFactory {
            calls: Arc::new(AtomicUsize::new(0)),
            fake: fake.clone(),
        }),
        ..DaemonDependencies::default()
    };
    let task = ready_with_dependencies(&config, dependencies).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "panic-client",
        ClientKind::Headless,
    )
    .await;
    let (session_id, generation) = create_and_attach(&mut client, &config, &workspace).await;
    send_request(
        &mut client,
        &config,
        "panic-submit",
        submit_body(
            "panic-command",
            session_id.clone(),
            generation,
            "panic once",
        ),
    )
    .await;
    let (panicked_run, _) = next_submit_response(&mut client).await;
    let panicked = events_until_terminal(&mut client, &panicked_run).await;
    assert!(
        panicked
            .iter()
            .any(|(_, payload)| matches!(payload, EventPayload::RunFailed { .. }))
    );
    assert!(matches!(
        panicked.last(),
        Some((_, EventPayload::RunState(RunState::Errored)))
    ));

    send_request(
        &mut client,
        &config,
        "fresh-submit",
        submit_body(
            "fresh-command",
            session_id,
            generation,
            "use fresh supervisor",
        ),
    )
    .await;
    let (fresh_run, _) = next_submit_response(&mut client).await;
    let fresh = events_until_terminal(&mut client, &fresh_run).await;
    assert!(matches!(
        fresh.last(),
        Some((_, EventPayload::RunState(RunState::Done)))
    ));
    assert_eq!(fake.requests().len(), 1);
    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// Revoked-credential checkpoint fixture (B2).
///
/// MUTATION CHECK: propagate a recovered supervisor start error through the
/// Ready barrier. Expected failure: startup stops instead of closing the
/// menu and terminalizing the run. Verified by revert in W3c1.1.
#[tokio::test]
async fn revoked_credential_checkpoint_terminalizes_menu_and_reaches_ready() {
    let root = test_root("w3c-live-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let config = DaemonConfig::new(
        "revoked-checkpoint",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let (first_dependencies, fake) = fake_dependencies(vec![
        FakeStep::EmitRequestInput {
            call_id: "revoked-choice".into(),
            kind: FakeInputKind::Choice,
            title: "Credential-dependent choice".into(),
            body: Vec::new(),
            options: vec![FakeInputOption {
                key: "continue".into(),
                label: "Continue".into(),
                detail: None,
            }],
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
    ]);
    let first_task = ready_with_dependencies(&config, first_dependencies).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "revoked-before",
        ClientKind::Headless,
    )
    .await;
    let (session_id, generation) = create_and_attach(&mut client, &config, &workspace).await;
    send_request(
        &mut client,
        &config,
        "revoked-submit",
        submit_body(
            "revoked-command",
            session_id.clone(),
            generation,
            "park then revoke",
        ),
    )
    .await;
    let (run_id, _) = next_submit_response(&mut client).await;
    let menu_id = loop {
        if let WireFrame::Event { envelope, .. } = client.next().await
            && envelope.run_id.as_ref() == Some(&run_id)
            && let Ok(EventPayload::MenuOpened(menu)) =
                serde_json::from_value::<EventPayload>(envelope.payload)
        {
            break menu.id;
        }
    };
    assert_eq!(fake.requests().len(), 1);
    drop(client);
    first_task.crash().await;

    let dependencies = DaemonDependencies {
        provider_factory: Arc::new(RevokedCredentialFactory),
        ..DaemonDependencies::default()
    };
    let second_task = ready_with_dependencies(&config, dependencies).await;
    second_task.shutdown_handle().request("fixture inspected");
    second_task.join().await.expect("daemon joins");

    let store = Store::open(&config.store_dir).expect("inspect recovery");
    let events = store.journal_replay(&session_id).expect("history");
    let payloads = payloads_for_run(&events, &run_id).collect::<Vec<_>>();
    assert!(payloads.contains(&EventPayload::RunState(RunState::Errored)));
    assert!(payloads.iter().any(|payload| matches!(
        payload,
        EventPayload::RunFailed {
            code: ErrorCode::CredentialMissing,
            ..
        }
    )));
    assert!(payloads.iter().any(|payload| matches!(
        payload,
        EventPayload::MenuClosed { menu, .. } if *menu == menu_id
    )));
    assert!(events.iter().any(|envelope| {
        matches!(
            serde_json::from_value::<EventPayload>(envelope.payload.clone()),
            Ok(EventPayload::SessionState(SessionState::Idle {
                interrupted: true
            }))
        )
    }));
}

/// B2 mixed fixture: an earlier recovered checkpoint may park indefinitely,
/// while a later recovered Queued item is acknowledged at safe supervisor
/// handoff so startup still reaches Ready.
///
/// MUTATION CHECK: acknowledge queued recovery only from `start_turn`.
/// Expected failure: Ready waits forever behind the unanswered checkpoint.
#[tokio::test]
async fn checkpoint_then_later_queued_recovery_reaches_ready_without_starting_queued() {
    let root = test_root("w3c-live-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let config = DaemonConfig::new(
        "mixed-recovery",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let (first_dependencies, first_fake) = fake_dependencies(vec![
        FakeStep::EmitRequestInput {
            call_id: "mixed-choice".into(),
            kind: FakeInputKind::Choice,
            title: "Park recovery".into(),
            body: Vec::new(),
            options: vec![FakeInputOption {
                key: "continue".into(),
                label: "Continue".into(),
                detail: None,
            }],
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
    ]);
    let first_task = ready_with_dependencies(&config, first_dependencies).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "mixed-before",
        ClientKind::Headless,
    )
    .await;
    let (session_id, generation) = create_and_attach(&mut client, &config, &workspace).await;
    send_request(
        &mut client,
        &config,
        "mixed-checkpoint-submit",
        submit_body(
            "mixed-checkpoint-command",
            session_id.clone(),
            generation,
            "park first",
        ),
    )
    .await;
    let (checkpoint_run, _) = next_submit_response(&mut client).await;
    let menu_id = loop {
        if let WireFrame::Event { envelope, .. } = client.next().await
            && envelope.run_id.as_ref() == Some(&checkpoint_run)
            && let Ok(EventPayload::MenuOpened(menu)) =
                serde_json::from_value::<EventPayload>(envelope.payload)
        {
            break menu.id;
        }
    };
    send_request(
        &mut client,
        &config,
        "mixed-queued-submit",
        submit_body(
            "mixed-queued-command",
            session_id.clone(),
            generation,
            "wait behind checkpoint",
        ),
    )
    .await;
    let (queued_run, _) = next_submit_response(&mut client).await;
    assert_eq!(first_fake.requests().len(), 1);
    drop(client);
    first_task.crash().await;

    let (second_dependencies, second_fake) = fake_dependencies(vec![FakeStep::Hang]);
    let second_task = ready_with_dependencies(&config, second_dependencies).await;
    assert!(
        second_fake.requests().is_empty(),
        "checkpoint recovery does not replay provider work and queued stays behind it"
    );
    second_task
        .shutdown_handle()
        .request("mixed fixture inspected");
    second_task.join().await.expect("daemon joins");

    let store = Store::open(&config.store_dir).expect("inspect recovery");
    let events = store.journal_replay(&session_id).expect("history");
    let checkpoint = payloads_for_run(&events, &checkpoint_run).collect::<Vec<_>>();
    let queued = payloads_for_run(&events, &queued_run).collect::<Vec<_>>();
    assert!(checkpoint.contains(&EventPayload::RunState(RunState::Cancelled)));
    assert!(checkpoint.iter().any(|payload| matches!(
        payload,
        EventPayload::MenuClosed { menu, .. } if *menu == menu_id
    )));
    assert!(queued.contains(&EventPayload::RunState(RunState::Cancelled)));
}

/// Scenario 10.
///
/// MUTATION CHECK: rerun the provider request that created request_input,
/// require the answer to carry the new generation instead of the durable
/// opening coordinates, or omit the post-registration committed-answer scan.
/// Expected failure: request count exceeds two, the replayed answer is
/// rejected, or the recovered harness remains parked forever.
#[tokio::test]
async fn scenario_10_restart_replays_request_input_without_reexecuting_prior_request() {
    let root = test_root("w3c-live-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let config = DaemonConfig::new(
        "restart-request-input",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let (dependencies, fake) = fake_dependencies(vec![
        FakeStep::EmitRequestInput {
            call_id: "restart-choice".into(),
            kind: FakeInputKind::Choice,
            title: "Resume me".into(),
            body: vec!["This menu survives a restart".into()],
            options: vec![FakeInputOption {
                key: "continue".into(),
                label: "Continue".into(),
                detail: None,
            }],
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "restart-choice".into(),
        },
        FakeStep::EmitText {
            text: "resumed after answer".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);
    let first_task = ready_with_dependencies(&config, dependencies.clone()).await;
    let mut first = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "checkpoint-before",
        ClientKind::Headless,
    )
    .await;
    let (session_id, generation) = create_and_attach(&mut first, &config, &workspace).await;
    send_request(
        &mut first,
        &config,
        "checkpoint-submit",
        submit_body(
            "checkpoint-command",
            session_id.clone(),
            generation,
            "ask across restart",
        ),
    )
    .await;
    let (run_id, _) = next_submit_response(&mut first).await;
    let (menu_id, request_seq, opening_generation) = loop {
        if let WireFrame::Event { envelope, .. } = first.next().await
            && envelope.run_id.as_ref() == Some(&run_id)
            && let Ok(EventPayload::MenuOpened(menu)) =
                serde_json::from_value::<EventPayload>(envelope.payload)
        {
            break (menu.id, envelope.seq, envelope.worker_generation);
        }
    };
    assert_eq!(fake.requests().len(), 1);
    drop(first);
    first_task.crash().await;

    let second_task = ready_with_dependencies(&config, dependencies).await;
    assert_eq!(
        fake.requests().len(),
        1,
        "startup reconstructs the checkpoint without a provider request"
    );
    let mut second = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "checkpoint-after",
        ClientKind::Headless,
    )
    .await;
    let replay_frames = attach_existing(
        &mut second,
        &config,
        session_id.clone(),
        0,
        "checkpoint-replay",
    )
    .await;
    assert!(replay_frames.iter().any(|envelope| {
        envelope.run_id.as_ref() == Some(&run_id)
            && matches!(
                serde_json::from_value::<EventPayload>(envelope.payload.clone()),
                Ok(EventPayload::MenuOpened(ref menu)) if menu.id == menu_id
            )
    }));
    second
        .send(
            &WireFrame::MenuAnswer {
                request_id: Some(RequestId::new("checkpoint-answer")),
                command_id: CommandId::new("checkpoint-answer-command"),
                session_id: session_id.clone(),
                menu_id: menu_id.clone(),
                request_seq,
                worker_generation: opening_generation,
                option_key: "continue".into(),
                option_index: 0,
                input: None,
            },
            config.frame_limit,
        )
        .await;
    assert!(matches!(
        next_response(&mut second).await,
        WireFrame::Response {
            body: ResponseBody::MenuAnswer { .. },
            ..
        }
    ));
    second
        .send(
            &WireFrame::MenuAnswer {
                request_id: Some(RequestId::new("checkpoint-loser")),
                command_id: CommandId::new("checkpoint-loser-command"),
                session_id: session_id.clone(),
                menu_id: menu_id.clone(),
                request_seq,
                worker_generation: opening_generation,
                option_key: "continue".into(),
                option_index: 0,
                input: None,
            },
            config.frame_limit,
        )
        .await;
    assert!(matches!(
        next_response(&mut second).await,
        WireFrame::Response {
            body: ResponseBody::Error { code, .. },
            ..
        } if code == ERROR_CODE_ALREADY_RESOLVED
    ));
    let events = events_until_terminal(&mut second, &run_id).await;
    assert!(
        events
            .iter()
            .any(|(_, payload)| *payload == EventPayload::RunState(RunState::Done))
    );
    let durable = read_session(&mut second, &config, session_id, "checkpoint-durable-read").await;
    let resolution = durable
        .iter()
        .find(|envelope| {
            matches!(
                serde_json::from_value::<EventPayload>(envelope.payload.clone()),
                Ok(EventPayload::MenuAnswered(answer)) if answer.menu == menu_id
            )
        })
        .expect("durable menu resolution");
    assert!(
        resolution.worker_generation > opening_generation,
        "post-restart resolution is stamped with the current generation"
    );
    assert_eq!(
        payloads_for_run(&durable, &run_id)
            .filter(|payload| matches!(payload, EventPayload::ToolResult { .. }))
            .count(),
        1
    );
    assert_eq!(
        payloads_for_run(&durable, &run_id)
            .filter(|payload| matches!(payload, EventPayload::MenuOpened(_)))
            .count(),
        1,
        "request_input executes once; restart only replays its durable menu"
    );
    let requests = fake.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|message| message.tool_result_for("restart-choice").is_some())
    );

    second_task.shutdown_handle().request("test complete");
    second_task.join().await.expect("daemon joins");
}

#[derive(Clone)]
struct HoldingEffectFactory {
    calls: Arc<AtomicUsize>,
    effect: EffectId,
}

#[async_trait]
impl TurnToolFactory for HoldingEffectFactory {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "hold_effect".into(),
            description: "Journals dispatch, then holds the result boundary".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        }]
    }

    async fn create(
        &self,
        context: WorkerToolContext,
    ) -> Result<Option<Arc<dyn ToolDispatcher>>, HaiderError> {
        Ok(Some(Arc::new(HoldingEffectDispatcher {
            context,
            calls: self.calls.clone(),
            effect: self.effect.clone(),
        })))
    }
}

struct HoldingEffectDispatcher {
    context: WorkerToolContext,
    calls: Arc<AtomicUsize>,
    effect: EffectId,
}

#[async_trait]
impl ToolDispatcher for HoldingEffectDispatcher {
    async fn execute(
        &self,
        _run_id: &RunId,
        _call_id: &str,
        name: &str,
        _args: serde_json::Value,
        _cancel: &CancelToken,
    ) -> Result<BoundedResult, HaiderError> {
        if name != "hold_effect" {
            return Err(HaiderError::new(
                ErrorCode::InvalidArgument,
                format!("unexpected test tool {name}"),
                false,
            ));
        }
        let payloads = [
            EventPayload::Effect(EffectPhase::Intent(EffectIntent {
                effect: self.effect.clone(),
                class: EffectClass::Network {
                    host: "example.invalid".into(),
                },
                summary: "ambiguous non-idempotent test effect".into(),
                args_digest: "blake3:w3c-scenario-11".into(),
                workspace_revision: None,
            })),
            EventPayload::Effect(EffectPhase::Authorized {
                effect: self.effect.clone(),
                verdict: AuthorizationVerdict::Allow,
            }),
            EventPayload::Effect(EffectPhase::Dispatched {
                effect: self.effect.clone(),
            }),
        ];
        let mut envelopes = payloads
            .into_iter()
            .enumerate()
            .map(|(index, payload)| EventEnvelope {
                schema_version: SCHEMA_VERSION,
                event_id: EventId::new(format!("scenario-11-effect-{}", index + 1)),
                seq: 0,
                session_id: self.context.store.session_id().clone(),
                branch_id: None,
                run_id: Some(self.context.run_id.clone()),
                agent_id: None,
                device_id: self.context.device_id.clone(),
                authority_epoch: 0,
                worker_generation: self.context.store.worker_generation(),
                causation_id: None,
                correlation_id: None,
                committed_at_ms: 0,
                render: RenderTargets {
                    ui: true,
                    durable: true,
                    prompt: PromptRender::Omit,
                },
                payload: serde_json::to_value(payload).expect("effect payload"),
            })
            .collect::<Vec<_>>();
        StoreHandle::append(&self.context.store, &mut envelopes).await?;
        self.calls.fetch_add(1, Ordering::SeqCst);
        future::pending::<Result<BoundedResult, HaiderError>>().await
    }
}

/// Scenario 11.
///
/// MUTATION CHECK: omit pre-Ready effect reconciliation, classify RunningTool
/// as resumable, or dispatch a prior-generation effect from recovery.
/// Expected failure: no Unknown outcome is committed or dispatcher/provider
/// call counts exceed one.
#[tokio::test]
async fn scenario_11_held_effect_becomes_unknown_after_restart_and_never_redispatches() {
    let root = test_root("w3c-live-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let config = DaemonConfig::new(
        "restart-held-effect",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let (mut dependencies, fake) = fake_dependencies(vec![
        FakeStep::EmitToolCall {
            call_id: "held-call".into(),
            name: "hold_effect".into(),
            args: serde_json::json!({}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
    ]);
    let calls = Arc::new(AtomicUsize::new(0));
    let effect = EffectId::new("held-effect");
    dependencies.tool_factory = Arc::new(HoldingEffectFactory {
        calls: calls.clone(),
        effect: effect.clone(),
    });
    let first_task = ready_with_dependencies(&config, dependencies.clone()).await;
    let mut first = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "effect-before",
        ClientKind::Headless,
    )
    .await;
    let (session_id, generation) = create_and_attach(&mut first, &config, &workspace).await;
    send_request(
        &mut first,
        &config,
        "effect-submit",
        submit_body(
            "effect-command",
            session_id.clone(),
            generation,
            "dispatch once",
        ),
    )
    .await;
    let (run_id, _) = next_submit_response(&mut first).await;
    tokio::time::timeout(support::DEADLINE, async {
        while calls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("effect crossed dispatch boundary");
    drop(first);
    first_task.crash().await;

    let second_task = ready_with_dependencies(&config, dependencies).await;
    let mut second = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "effect-after",
        ClientKind::Headless,
    )
    .await;
    attach_existing(&mut second, &config, session_id.clone(), 0, "effect-replay").await;
    let envelopes = read_session(&mut second, &config, session_id, "effect-read-terminal").await;
    assert_eq!(
        payloads_for_run(&envelopes, &run_id)
            .filter(|payload| matches!(
                payload,
                EventPayload::Effect(EffectPhase::Outcome {
                    effect: found,
                    outcome: EffectOutcome::Unknown,
                }) if *found == effect
            ))
            .count(),
        1
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(fake.requests().len(), 1);

    second_task.shutdown_handle().request("test complete");
    second_task.join().await.expect("daemon joins");
}

/// Scenario 12.
///
/// MUTATION CHECK: push normalized reasoning into assistant follow-up blocks,
/// treat request usage updates as logical-turn deltas, or omit RunFailed.
/// Expected failure: request two contains Reasoning, cumulative usage is
/// wrong, or Errored is not immediately preceded by one RunFailed.
#[tokio::test]
async fn scenario_12_reasoning_safe_follow_up_cumulative_usage_and_durable_failure() {
    let root = test_root("w3c-live-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    fs::write(workspace.join("note.txt"), "tool output").expect("fixture file");
    let config = DaemonConfig::new(
        "reasoning-usage",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let first_usage = Usage {
        input: 10,
        output: 4,
        reasoning: 3,
        cached: 2,
        source: UsageSource::ProviderReported,
        account: None,
    };
    let second_usage = Usage {
        input: 6,
        output: 2,
        reasoning: 1,
        cached: 1,
        source: UsageSource::ProviderReported,
        account: None,
    };
    let expected_cumulative = Usage {
        input: 16,
        output: 6,
        reasoning: 4,
        cached: 3,
        source: UsageSource::ProviderReported,
        account: None,
    };
    let (dependencies, fake) = fake_dependencies(vec![
        FakeStep::EmitReasoning {
            text: "private normalized thought".into(),
        },
        FakeStep::EmitToolCall {
            call_id: "read-1".into(),
            name: "fs_read".into(),
            args: serde_json::json!({"path": "note.txt"}),
        },
        FakeStep::EmitUsage { usage: first_usage },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "read-1".into(),
        },
        FakeStep::EmitUsage {
            usage: second_usage,
        },
        FakeStep::MalformedFrame,
    ]);
    let task = ready_with_dependencies(&config, dependencies).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "reasoning-client",
        ClientKind::Headless,
    )
    .await;
    let (session_id, generation) = create_and_attach(&mut client, &config, &workspace).await;
    send_request(
        &mut client,
        &config,
        "submit",
        submit_body("reasoning-submit", session_id, generation, "read note"),
    )
    .await;
    let (run_id, _) = next_submit_response(&mut client).await;
    let events = events_until_terminal(&mut client, &run_id).await;
    let requests = fake.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].messages.iter().all(|message| {
        message
            .blocks
            .iter()
            .all(|block| !matches!(block, haider_protocol::provider::Block::Reasoning { .. }))
    }));
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|message| message.tool_result_for("read-1").is_some())
    );
    let usages = events
        .iter()
        .filter_map(|(_, payload)| match payload {
            EventPayload::Usage(usage) => Some(usage),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(usages.last().copied(), Some(&expected_cumulative));
    let failures = events
        .iter()
        .filter_map(|(_, payload)| match payload {
            EventPayload::RunFailed {
                code,
                message,
                retryable,
            } => Some((code, message, retryable)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(failures.len(), 1);
    let (failure_code, failure_message, failure_retryable) = failures[0];
    assert_eq!(*failure_code, ErrorCode::ProviderError);
    assert!(!*failure_retryable);
    assert!(failure_message.len() <= 512);
    assert!(
        failure_message
            .chars()
            .all(|character| !character.is_control() || character == '\n')
    );
    let failed = events
        .iter()
        .position(|(_, payload)| matches!(payload, EventPayload::RunFailed { .. }))
        .expect("RunFailed");
    let errored = events
        .iter()
        .position(|(_, payload)| *payload == EventPayload::RunState(RunState::Errored))
        .expect("Errored");
    assert_eq!(errored, failed + 1);

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// Scenario 2 (M2 prefix).
///
/// MUTATION CHECK: capture the attach head outside the actor's `Register`
/// step (actor.rs). Expected failure: `AttachCaughtUp` reports a stale
/// high-water instead of 1, or the `Created` event misses the replay.
/// (Publishing `Created` before its transaction commits is NOT
/// deterministically observable over this wire ordering — the atomicity of
/// metadata + `Created` + receipt is pinned at the store seam by
/// `haider-store/tests/session_create_tests.rs`.)
#[tokio::test]
async fn scenario_2_real_uds_creates_attaches_and_replays_typed_session() {
    let root = test_root("w3c-live-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let config = DaemonConfig::new(
        "live-create",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let task = ready(&config).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "control-1",
        ClientKind::Headless,
    )
    .await;

    send_request(
        &mut client,
        &config,
        "create",
        create_body("create-command", workspace.to_string_lossy().into_owned()),
    )
    .await;
    let (session_id, metadata) = created_response(client.next().await);
    assert_eq!(
        metadata.cwd,
        fs::canonicalize(&workspace)
            .expect("canonical workspace")
            .to_str()
            .expect("UTF-8")
    );

    send_request(
        &mut client,
        &config,
        "attach",
        RequestBody::SessionAttach {
            session_id: session_id.clone(),
            after_seq: 0,
            mode: AttachMode::Control,
        },
    )
    .await;
    assert!(matches!(
        client.next().await,
        WireFrame::Response {
            body: ResponseBody::SessionAttach { .. },
            ..
        }
    ));
    let created = match client.next().await {
        WireFrame::Event { envelope, .. } => envelope,
        other => panic!("expected Created event, got {other:?}"),
    };
    assert_eq!(created.seq, 1);
    assert_eq!(
        serde_json::from_value::<EventPayload>(created.payload.clone()).expect("typed payload"),
        EventPayload::SessionState(SessionState::Created)
    );
    assert!(matches!(
        client.next().await,
        WireFrame::AttachCaughtUp {
            high_water_seq: 1,
            ..
        }
    ));

    send_request(
        &mut client,
        &config,
        "list",
        RequestBody::SessionList {
            cursor: None,
            limit: 10,
        },
    )
    .await;
    let summary = match client.next().await {
        WireFrame::Response {
            body: ResponseBody::SessionList { sessions, .. },
            ..
        } => sessions.into_iter().next().expect("one session"),
        other => panic!("expected list response, got {other:?}"),
    };
    assert_eq!(
        summary,
        SessionSummary {
            session_id: session_id.clone(),
            head_seq: 1,
            worker_generation: created.worker_generation,
            metadata: Some(metadata.clone()),
        }
    );

    send_request(
        &mut client,
        &config,
        "read",
        RequestBody::SessionRead {
            session_id,
            range: SeqRange {
                start_seq: 1,
                end_seq: 1,
            },
        },
    )
    .await;
    assert!(matches!(
        client.next().await,
        WireFrame::Response {
            body: ResponseBody::SessionRead { result },
            ..
        } if result.metadata == Some(metadata) && result.envelopes.len() == 1
    ));

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// Scenario 2 satellite — the R2 receipt-idempotency law on the wire: a
/// lost `session.create` response is recoverable by same-command retry, and
/// a same-command different-body reuse is rejected.
///
/// MUTATION CHECK: in `session_create` (session_hub/rpc.rs), move
/// `validate_workspace` before the `session_create_receipt` preflight, or
/// drop the digest comparison in the store's receipt lookup. Expected
/// failure: the retry after workspace removal fails, or the changed-body
/// reuse is accepted.
#[tokio::test]
async fn session_create_lost_response_retry_survives_removed_cwd_and_rejects_changed_body() {
    let root = test_root("w3c-live-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let alternate = root.path().join("alternate");
    fs::create_dir(&alternate).expect("alternate workspace");
    let config = DaemonConfig::new(
        "create-idempotency",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let task = ready(&config).await;
    let mut submitter = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "submitter",
        ClientKind::Headless,
    )
    .await;
    let mut observer = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "observer",
        ClientKind::Headless,
    )
    .await;
    let original = workspace.to_string_lossy().into_owned();

    send_request(
        &mut submitter,
        &config,
        "lost-response",
        create_body("same-command", original.clone()),
    )
    .await;
    // Observe durable truth from another connection, then drop the first
    // connection without ever reading its response.
    let first_session = loop {
        send_request(
            &mut observer,
            &config,
            "observe-list",
            RequestBody::SessionList {
                cursor: None,
                limit: 10,
            },
        )
        .await;
        match observer.next().await {
            WireFrame::Response {
                body: ResponseBody::SessionList { sessions, .. },
                ..
            } if !sessions.is_empty() => break sessions[0].session_id.clone(),
            WireFrame::Response {
                body: ResponseBody::SessionList { .. },
                ..
            } => tokio::task::yield_now().await,
            other => panic!("unexpected observer frame: {other:?}"),
        }
    };
    drop(submitter);
    fs::remove_dir(&workspace).expect("remove committed workspace");

    send_request(
        &mut observer,
        &config,
        "retry",
        create_body("same-command", original),
    )
    .await;
    let (retried_session, _) = created_response(observer.next().await);
    assert_eq!(retried_session, first_session);

    send_request(
        &mut observer,
        &config,
        "changed",
        create_body("same-command", alternate.to_string_lossy().into_owned()),
    )
    .await;
    assert!(matches!(
        observer.next().await,
        WireFrame::Response {
            body: ResponseBody::Error { ref code, .. },
            ..
        } if code == ERROR_CODE_INVALID_ARGUMENT
    ));

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// Scenario 2 satellite — the R7 capability and feature-advertisement law:
/// `session.create` requires Control, and the ready `Welcome` advertises
/// exactly the additive methods this daemon implements.
///
/// MUTATION CHECK: authorize `session.create` as View or advertise features
/// without implementing the receipt-backed method. Expected failure: the
/// View-only client creates a session, or the ready Welcome lacks the feature.
#[tokio::test]
async fn session_create_requires_control_and_ready_welcome_advertises_implemented_feature() {
    let root = test_root("w3c-live-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let config = DaemonConfig::new(
        "create-capability",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let task = ready(&config).await;
    let mut viewer = UdsClient::connect_with_capabilities(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "viewer",
        ClientKind::Headless,
        CapabilitySet::from([Capability::View]),
    )
    .await;
    // The shared handshake consumed Welcome, so reconnect raw to inspect the
    // advertised feature set explicitly.
    let mut feature_client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "feature-client",
        ClientKind::Headless,
    )
    .await;
    send_request(
        &mut viewer,
        &config,
        "denied-create",
        create_body("viewer-command", workspace.to_string_lossy().into_owned()),
    )
    .await;
    assert!(matches!(
        viewer.next().await,
        WireFrame::Response {
            body: ResponseBody::Error { ref code, .. },
            ..
        } if code == ERROR_CODE_CAPABILITY_DENIED
    ));

    send_request(
        &mut feature_client,
        &config,
        "list",
        RequestBody::SessionList {
            cursor: None,
            limit: 1,
        },
    )
    .await;
    assert!(matches!(
        feature_client.next().await,
        WireFrame::Response {
            body: ResponseBody::SessionList { ref sessions, .. },
            ..
        } if sessions.is_empty()
    ));
    send_request(
        &mut feature_client,
        &config,
        "control-create",
        create_body(
            "control-create-command",
            workspace.to_string_lossy().into_owned(),
        ),
    )
    .await;
    let (session_id, generation) = match feature_client.next().await {
        WireFrame::Response {
            body:
                ResponseBody::SessionCreate {
                    session_id,
                    worker_generation,
                    ..
                },
            ..
        } => (session_id, worker_generation),
        other => panic!("expected control create, got {other:?}"),
    };
    send_request(
        &mut viewer,
        &config,
        "denied-submit",
        submit_body(
            "viewer-submit-command",
            session_id.clone(),
            generation,
            "must not submit",
        ),
    )
    .await;
    assert!(matches!(
        viewer.next().await,
        WireFrame::Response {
            body: ResponseBody::Error { ref code, .. },
            ..
        } if code == ERROR_CODE_CAPABILITY_DENIED
    ));
    send_request(
        &mut viewer,
        &config,
        "denied-cancel",
        RequestBody::TurnCancel {
            command_id: CommandId::new("viewer-cancel-command"),
            session_id,
            worker_generation: generation,
            run_id: RunId::new("not-visible-to-viewer"),
        },
    )
    .await;
    assert!(matches!(
        viewer.next().await,
        WireFrame::Response {
            body: ResponseBody::Error { ref code, .. },
            ..
        } if code == ERROR_CODE_CAPABILITY_DENIED
    ));

    // Inspect a fresh raw handshake because connect_control intentionally
    // consumes Welcome.
    let mut raw = UdsClient::connect(&config.endpoint_path(), config.frame_limit)
        .await
        .expect("raw connect");
    raw.send(
        &WireFrame::Hello(haider_rpc::Hello {
            protocol_min: haider_rpc::WIRE_PROTOCOL_VERSION,
            protocol_max: haider_rpc::WIRE_PROTOCOL_VERSION,
            client_name: "feature-inspector".into(),
            client_version: "test".into(),
            client_instance_id: "feature-inspector".into(),
            client_kind: ClientKind::Headless,
            capabilities_requested: CapabilitySet::from([Capability::View]),
            max_receive_frame: u32::try_from(config.frame_limit).expect("frame limit"),
        }),
        config.frame_limit,
    )
    .await;
    assert!(matches!(
        raw.next().await,
        WireFrame::Welcome(haider_rpc::Welcome { features, .. })
            if features.contains(FEATURE_SESSION_MUTATION_V1)
                && features.contains(FEATURE_TURN_CONTROL_V1)
    ));

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// Scenario 13: the manifest for the production-seam mutation sweep.
///
/// Each entry is `(test fn, workspace-relative file, seam to revert)`: the
/// focused test that must fail when the listed seam is reverted, and where a
/// re-runner finds it (six of thirteen live outside this file). The sweep
/// itself is executed by hand — revert each seam, run the named test, record
/// the observation in the commit message; this manifest keeps that procedure
/// honest by construction: the seven in-file entries are compile-time
/// references to their test functions (a rename breaks the build), and every
/// listed file path is asserted to exist in the workspace.
///
/// MUTATION CHECK: delete an entry, point two seams at the same focused
/// test, or let a listed file move without updating its coordinate.
/// Expected failure: the completeness, uniqueness, or path-existence
/// assertions below fail (an in-file test rename fails compilation first).
#[test]
fn scenario_13_mutation_seam_sweep_manifest_covers_each_load_bearing_boundary() {
    // Compile-time linkage for the in-file entries: renaming any of these
    // seven tests without updating the manifest is a build error.
    let _in_file_sweep_links: [fn(); 7] = [
        scenario_4_lost_submit_response_replays_one_run_and_one_provider_request,
        scenario_7_two_menu_answers_race_and_only_first_commit_wins,
        scenario_8_wire_cancel_closes_open_items_and_cancelled_is_run_terminal,
        scenario_9_restart_resumes_only_queued_and_terminalizes_streaming,
        scenario_10_restart_replays_request_input_without_reexecuting_prior_request,
        scenario_11_held_effect_becomes_unknown_after_restart_and_never_redispatches,
        scenario_12_reasoning_safe_follow_up_cumulative_usage_and_durable_failure,
    ];
    const HERE: &str = "crates/haider-daemond/tests/live_turn_rpc_tests.rs";
    let sweep = [
        (
            "scenario_4_lost_submit_response_replays_one_run_and_one_provider_request",
            HERE,
            "durable receipt preflight and admit_pending's active-run/in-queue run-id dedup",
        ),
        (
            "superseded_worker_lease_is_fenced_before_store_append",
            "crates/haider-daemon/tests/session_hub_tests.rs",
            "hub WorkerAppend active lease-token validation",
        ),
        (
            "scenario_7_two_menu_answers_race_and_only_first_commit_wins",
            HERE,
            "SQLite first-committed-wins menu CAS",
        ),
        (
            "scenario_9_restart_resumes_only_queued_and_terminalizes_streaming",
            HERE,
            "interrupted-run resumability reduction (turn_recovery.rs)",
        ),
        (
            "scenario_11_held_effect_becomes_unknown_after_restart_and_never_redispatches",
            HERE,
            "pre-Ready ambiguous-effect reconciliation",
        ),
        (
            "scenario_12_reasoning_safe_follow_up_cumulative_usage_and_durable_failure",
            HERE,
            "reasoning omission, cumulative usage, and RunFailed ordering",
        ),
        (
            "replay_live_barrier_is_contiguous_at_every_forced_boundary",
            "crates/haider-daemon/tests/session_hub_tests.rs",
            "persist-before-publish and serialized register-plus-head",
        ),
        (
            "full_internal_catch_up_receiver_reregisters_and_resumes_from_store",
            "crates/haider-daemon/tests/session_hub_tests.rs",
            "bounded catch-up with store as the only lag buffer",
        ),
        (
            "scenario_10_restart_replays_request_input_without_reexecuting_prior_request",
            HERE,
            "recovery Ready barrier and recovered-menu generation authorization",
        ),
        (
            "scenario_8_wire_cancel_closes_open_items_and_cancelled_is_run_terminal",
            HERE,
            "lease-bound cancellation wake and terminal event ordering",
        ),
        (
            "aggregate_idle_is_skipped_when_a_new_run_is_durably_active",
            "crates/haider-store/tests/turn_command_tests.rs",
            "transactional aggregate SessionState ownership",
        ),
        (
            "tool_result_is_presented_after_its_completed_tool_call",
            "crates/haider-core/src/prompt_history.rs",
            "provider-valid tool history reconstruction",
        ),
        (
            "dropping_an_owned_stream_aborts_its_producer",
            "crates/haider-provider/tests/fake_provider_tests.rs",
            "owned provider producer cancellation",
        ),
    ];
    let tests = sweep
        .iter()
        .map(|(test, _, _)| *test)
        .collect::<std::collections::HashSet<_>>();
    let seams = sweep
        .iter()
        .map(|(_, _, seam)| *seam)
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(sweep.len(), 13);
    assert_eq!(tests.len(), sweep.len());
    assert_eq!(seams.len(), sweep.len());
    let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root");
    for (test, file, _) in &sweep {
        let path = workspace_root.join(file);
        assert!(
            path.is_file(),
            "manifest coordinate for `{test}` does not exist: {file}"
        );
        let source = fs::read_to_string(&path).expect("manifest coordinate is readable");
        assert!(
            source.contains(&format!("fn {test}")),
            "manifest coordinate {file} no longer defines `{test}`"
        );
    }
}
