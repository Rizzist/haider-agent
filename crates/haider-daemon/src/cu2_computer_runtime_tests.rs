//! CU-2 fake-backend runtime coverage. Tests in this module never touch real
//! screen or input APIs.

#![allow(clippy::expect_used)]

use crate::session_hub::{SessionHub, SessionHubConfig};
use crate::worker::{
    BrokerToolFactory, ProviderFactory, ResolvedTurnProvider, TurnToolFactory, WorkerDependencies,
    WorkerManager,
};
use async_trait::async_trait;
use base64::Engine as _;
use haider_core::{SqliteStoreHandle, StoreHandle, TurnAdmissionDisposition};
use haider_protocol::DeliveryMode;
use haider_protocol::EventPayload;
use haider_protocol::computer::ComputerAction;
use haider_protocol::effect::{
    AuthorizationVerdict, EffectClass, EffectIntent, EffectOutcome, EffectPhase,
};
use haider_protocol::envelope::{
    EventEnvelope, PromptRender, RawEnvelope, RenderTargets, SCHEMA_VERSION,
};
use haider_protocol::ids::{DeviceId, EffectId, EventId, MenuId, RunId, SessionId};
use haider_protocol::item::{ItemEvent, ToolStatus, TurnItem};
use haider_protocol::menu::{
    AnswerVia, DecisionKind, Menu, MenuAnswer, MenuKind, MenuOption, MenuScope,
};
use haider_protocol::provider::{Block, FinishReason};
use haider_protocol::state::RunState;
use haider_provider::{FakeProvider, FakeStep, Provider, TurnRequest};
use haider_store::{SessionCreateCommand, TurnAcceptCommand, TurnCancelCommand};
use haider_tools::{
    ComputerBackend, ComputerCancelToken, ComputerInspection, ComputerInspectionBounds,
    ComputerOutput, ComputerResult,
};
use image::{DynamicImage, ImageFormat};
use std::io::Cursor;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;
use tokio::time::{Duration, timeout};

struct FixedProviderFactory {
    provider: Arc<FakeProvider>,
}

#[async_trait]
impl ProviderFactory for FixedProviderFactory {
    async fn resolve_for_turn(
        &self,
        metadata: &haider_protocol::session::SessionMetadataV1,
    ) -> Result<ResolvedTurnProvider, haider_protocol::error::HaiderError> {
        Ok(ResolvedTurnProvider {
            provider: Arc::clone(&self.provider) as Arc<dyn Provider>,
            provider_name: metadata.provider.clone(),
            model: metadata.model.clone(),
            context_window: None,
            account_alias: None,
            initial_rotation: None,
            rotation_budget_consumed: false,
            attempt_resolver: None,
        })
    }
}

struct FakeComputerBackend {
    screenshot: Vec<u8>,
    actions: Mutex<Vec<ComputerAction>>,
    viewports: Mutex<Vec<(u32, u32)>>,
    block_wait: bool,
    entered: Notify,
    active_cancel: Mutex<Option<ComputerCancelToken>>,
    emergency_stopped: AtomicBool,
}

impl FakeComputerBackend {
    fn new(screenshot: Vec<u8>) -> Self {
        Self {
            screenshot,
            actions: Mutex::new(Vec::new()),
            viewports: Mutex::new(Vec::new()),
            block_wait: false,
            entered: Notify::new(),
            active_cancel: Mutex::new(None),
            emergency_stopped: AtomicBool::new(false),
        }
    }

    fn blocking(screenshot: Vec<u8>) -> Self {
        Self {
            block_wait: true,
            ..Self::new(screenshot)
        }
    }
}

#[async_trait]
impl ComputerBackend for FakeComputerBackend {
    async fn execute(
        &self,
        action: &ComputerAction,
        cancel: &ComputerCancelToken,
    ) -> ComputerResult<ComputerOutput> {
        cancel.check()?;
        self.actions
            .lock()
            .expect("actions lock")
            .push(action.clone());
        match action {
            ComputerAction::Screenshot => {
                Ok(ComputerOutput::ScreenshotPng(self.screenshot.clone()))
            }
            ComputerAction::CursorPosition => Ok(ComputerOutput::CursorPosition { x: 5, y: 7 }),
            ComputerAction::Inspect { .. } => Ok(ComputerOutput::Inspection(ComputerInspection {
                role: Some("button".into()),
                label: Some("Send message".into()),
                title: Some("Send".into()),
                bounds: Some(ComputerInspectionBounds {
                    x: 980,
                    y: 320,
                    width: 88,
                    height: 42,
                }),
                value: None,
            })),
            ComputerAction::Wait { .. } if self.block_wait => {
                *self.active_cancel.lock().expect("cancel lock") = Some(cancel.clone());
                self.entered.notify_one();
                std::future::pending::<ComputerResult<ComputerOutput>>().await
            }
            _ => Ok(ComputerOutput::Confirmed {
                action: action_name(action).into(),
            }),
        }
    }

    fn set_viewport(&self, width: u32, height: u32) -> ComputerResult<()> {
        self.viewports
            .lock()
            .expect("viewport lock")
            .push((width, height));
        Ok(())
    }

    async fn emergency_stop(&self) -> ComputerResult<()> {
        self.emergency_stopped.store(true, Ordering::Release);
        if let Some(cancel) = self.active_cancel.lock().expect("cancel lock").as_ref() {
            cancel.cancel();
        }
        Ok(())
    }
}

fn action_name(action: &ComputerAction) -> &'static str {
    match action {
        ComputerAction::LeftClick { .. } => "left_click",
        ComputerAction::Inspect { .. } => "inspect",
        ComputerAction::Wait { .. } => "wait",
        _ => "computer_action",
    }
}

fn large_png_fixture() -> Vec<u8> {
    let pixels = image::RgbaImage::from_pixel(3_000, 1_000, image::Rgba([31, 61, 127, 255]));
    let mut encoded = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(pixels)
        .write_to(&mut encoded, ImageFormat::Png)
        .expect("encode PNG fixture");
    encoded.into_inner()
}

async fn create_session(hub: &SessionHub, session_id: &SessionId, device_id: &DeviceId) {
    let cwd = std::fs::canonicalize(std::env::current_dir().expect("cwd"))
        .expect("canonical cwd")
        .to_string_lossy()
        .into_owned();
    hub.create_internal_session(SessionCreateCommand {
        command_id: format!("create-{session_id}"),
        request_digest: format!("create-{session_id}-digest"),
        request_json: format!(r#"{{"session":"{session_id}"}}"#),
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
        event_id: EventId::new(format!("{session_id}-created")),
        device_id: device_id.clone(),
    })
    .await
    .expect("create CU-2 session");
}

fn grant_envelope(
    store: &SqliteStoreHandle,
    session_id: &SessionId,
    event: &str,
    payload: EventPayload,
) -> RawEnvelope {
    EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(event),
        seq: 0,
        session_id: session_id.clone(),
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: DeviceId::new("cu2-grant-device"),
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
        payload: serde_json::to_value(payload).expect("grant payload serializes"),
    }
}

async fn append_screen_control_grant(store: &SqliteStoreHandle, session_id: &SessionId) {
    let effect = EffectId::new(format!("{session_id}-control-effect"));
    let menu_id = MenuId::new(format!("{session_id}-control-menu"));
    let menu = Menu {
        id: menu_id.clone(),
        kind: MenuKind::Permission {
            effect_summary: "computer left_click".into(),
        },
        title: "Allow computer control?".into(),
        body: vec!["Effect class: ScreenControl".into()],
        options: vec![MenuOption {
            key: "approve_for_session".into(),
            label: "Approve for this session".into(),
            detail: None,
            decision: Some(DecisionKind::AllowAlways),
        }],
        blocking: true,
        scope: MenuScope::Session,
        origin: "effect_broker".into(),
        ttl_ms: None,
        timeout_option: None,
    };
    let mut events = [
        grant_envelope(
            store,
            session_id,
            "cu2-grant-intent",
            EventPayload::Effect(EffectPhase::Intent(EffectIntent {
                effect: effect.clone(),
                class: EffectClass::ScreenControl,
                summary: "computer left_click".into(),
                args_digest: "cu2-control-shape".into(),
                workspace_revision: None,
            })),
        ),
        grant_envelope(
            store,
            session_id,
            "cu2-grant-authorized",
            EventPayload::Effect(EffectPhase::Authorized {
                effect,
                verdict: AuthorizationVerdict::Ask {
                    menu: menu_id.clone(),
                },
            }),
        ),
        grant_envelope(
            store,
            session_id,
            "cu2-grant-opened",
            EventPayload::MenuOpened(menu),
        ),
        grant_envelope(
            store,
            session_id,
            "cu2-grant-answered",
            EventPayload::MenuAnswered(MenuAnswer {
                menu: menu_id,
                option_index: 0,
                option_key: Some("approve_for_session".into()),
                value: None,
                via: AnswerVia::Rpc,
            }),
        ),
    ];
    store
        .append(&mut events)
        .await
        .expect("append screen grant");
}

async fn submit_turn(
    hub: &SessionHub,
    manager: &crate::worker::WorkerManagerHandle,
    store: &SqliteStoreHandle,
    session_id: &SessionId,
    run_id: &RunId,
    device_id: DeviceId,
) {
    let accepted = hub
        .accept_internal_turn(TurnAcceptCommand {
            command_id: format!("submit-{run_id}"),
            request_digest: format!("submit-{run_id}-digest"),
            request_json: format!(r#"{{"run":"{run_id}"}}"#),
            session_id: session_id.clone(),
            worker_generation: store.worker_generation(),
            run_id: run_id.clone(),
            agent_id: None,
            branch_id: None,
            text: "use the computer".into(),
            attachments: Vec::new(),
            mode: DeliveryMode::Steer,
            queued_event_id: EventId::new(format!("{run_id}-queued")),
            user_event_id: EventId::new(format!("{run_id}-user")),
            active_event_id: EventId::new(format!("{run_id}-active")),
            device_id,
        })
        .await
        .expect("accept CU-2 turn");
    assert_eq!(accepted.disposition, TurnAdmissionDisposition::Started);
    manager.submit(accepted).await.expect("submit CU-2 turn");
}

async fn wait_for_run_state(
    store: &SqliteStoreHandle,
    session_id: &SessionId,
    run_id: &RunId,
    expected: RunState,
) -> Vec<haider_protocol::envelope::RawEnvelope> {
    timeout(Duration::from_secs(12), async {
        loop {
            let events = store.read(session_id, 0, 4096).await.expect("journal");
            if events.iter().any(|event| {
                event.run_id.as_ref() == Some(run_id)
                    && serde_json::from_value::<EventPayload>(event.payload.clone())
                        .is_ok_and(|payload| payload == EventPayload::RunState(expected.clone()))
            }) {
                break events;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("run reaches expected state")
}

#[tokio::test]
async fn screenshot_reaches_provider_click_journals_control_and_viewport_is_post_cu1() {
    let root = tempfile::tempdir().expect("profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let provider = Arc::new(
        FakeProvider::new(vec![
            FakeStep::EmitToolCall {
                call_id: "cu2-screenshot".into(),
                name: "computer".into(),
                args: serde_json::json!({"action": "screenshot"}),
            },
            FakeStep::Finish {
                reason: FinishReason::ToolUse,
            },
            FakeStep::ExpectToolResult {
                call_id: "cu2-screenshot".into(),
            },
            FakeStep::EmitToolCall {
                call_id: "cu2-inspect".into(),
                name: "computer".into(),
                args: serde_json::json!({"action": "inspect", "x": 1024, "y": 341}),
            },
            FakeStep::Finish {
                reason: FinishReason::ToolUse,
            },
            FakeStep::ExpectToolResult {
                call_id: "cu2-inspect".into(),
            },
            FakeStep::EmitToolCall {
                call_id: "cu2-click".into(),
                name: "computer".into(),
                args: serde_json::json!({"action": "left_click", "x": 1024, "y": 341}),
            },
            FakeStep::Finish {
                reason: FinishReason::ToolUse,
            },
            FakeStep::ExpectToolResult {
                call_id: "cu2-click".into(),
            },
            FakeStep::EmitText {
                text: "computer complete".into(),
            },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
        ])
        .with_vision_native(),
    );
    let backend = Arc::new(FakeComputerBackend::new(large_png_fixture()));
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let factory: Arc<dyn TurnToolFactory> = Arc::new(BrokerToolFactory::with_computer_backend(
        Arc::clone(&backend) as Arc<dyn ComputerBackend>,
    ));
    let manager = WorkerManager::start(
        hub.clone(),
        WorkerDependencies {
            provider_factory: Arc::new(FixedProviderFactory {
                provider: Arc::clone(&provider),
            }),
            tool_factory: factory,
            delegation: None,
            web_search: None,
        },
        false,
    );
    hub.install_worker_manager(manager.handle())
        .expect("install manager");
    let session_id = SessionId::new("cu2-roundtrip-session");
    let run_id = RunId::new("cu2-roundtrip-run");
    let device_id = DeviceId::new("cu2-roundtrip-device");
    create_session(&hub, &session_id, &device_id).await;
    append_screen_control_grant(&store, &session_id).await;
    submit_turn(
        &hub,
        &manager.handle(),
        &store,
        &session_id,
        &run_id,
        device_id,
    )
    .await;

    let events = wait_for_run_state(&store, &session_id, &run_id, RunState::Done).await;
    let (journal_json, image) = events
        .iter()
        .find_map(|event| {
            let payload = serde_json::from_value::<EventPayload>(event.payload.clone()).ok()?;
            match payload {
                EventPayload::ToolResult { result, .. } if !result.images.is_empty() => {
                    Some((event.payload.to_string(), result.images[0].clone()))
                }
                _ => None,
            }
        })
        .expect("computer screenshot result");
    assert_eq!(image.width, 2048);
    assert!(image.height < 1_000);
    assert_eq!(
        backend.viewports.lock().expect("viewport lock").as_slice(),
        &[(image.width, image.height)]
    );
    assert!(!journal_json.contains("data_base64"));
    let cas_bytes = store.get(&image.artifact).await.expect("screenshot CAS");
    assert!(!journal_json.contains(&base64::engine::general_purpose::STANDARD.encode(&cas_bytes)));

    let requests: Vec<TurnRequest> = provider.requests();
    assert_eq!(requests.len(), 4);
    assert!(requests[1].messages.iter().any(|message| {
        matches!(message.blocks.as_slice(), [Block::ToolResult { call_id, images, .. }]
            if call_id == "cu2-screenshot" && images == std::slice::from_ref(&image))
    }));
    let provider_attachment = requests[1]
        .attachments
        .iter()
        .find(|attachment| attachment.artifact == image.artifact)
        .expect("provider screenshot attachment");
    let provider_bytes = base64::engine::general_purpose::STANDARD
        .decode(&provider_attachment.data_base64)
        .expect("provider attachment base64");
    assert_eq!(provider_bytes, cas_bytes);
    assert!(requests[2].messages.iter().any(|message| {
        matches!(message.blocks.as_slice(), [Block::ToolResult { call_id, preview, images, .. }]
            if call_id == "cu2-inspect"
                && preview == "{\"role\":\"button\",\"label\":\"Send message\",\"title\":\"Send\",\"bounds\":{\"x\":980,\"y\":320,\"width\":88,\"height\":42},\"value\":null}"
                && images.is_empty())
    }));
    assert!(requests[3].messages.iter().any(|message| {
        matches!(message.blocks.as_slice(), [Block::ToolResult { call_id, preview, images, .. }]
            if call_id == "cu2-click" && preview == "left_click completed" && images.is_empty())
    }));

    let mut control_effect = None;
    let mut inspect_effect = None;
    let mut phases = Vec::new();
    for event in &events {
        if let Ok(EventPayload::Effect(phase)) =
            serde_json::from_value::<EventPayload>(event.payload.clone())
        {
            if let EffectPhase::Intent(intent) = &phase
                && intent.class == EffectClass::ScreenControl
                && intent.summary == "computer left_click"
            {
                control_effect = Some(intent.effect.clone());
            }
            if let EffectPhase::Intent(intent) = &phase
                && intent.class == EffectClass::ScreenObserve
                && intent.summary == "computer inspect"
            {
                inspect_effect = Some(intent.effect.clone());
            }
            phases.push(phase);
        }
    }
    let inspect_effect = inspect_effect.expect("inspect ScreenObserve intent");
    assert!(phases.iter().any(|phase| matches!(phase,
        EffectPhase::Outcome { effect, outcome: EffectOutcome::Ok, .. }
            if effect == &inspect_effect)));
    let control_effect = control_effect.expect("click ScreenControl intent");
    assert!(phases.iter().any(|phase| matches!(phase,
        EffectPhase::Authorized { effect, verdict: AuthorizationVerdict::Allow }
            if effect == &control_effect)));
    assert!(phases.iter().any(|phase| matches!(phase,
        EffectPhase::Dispatched { effect } if effect == &control_effect)));
    assert!(phases.iter().any(|phase| matches!(phase,
        EffectPhase::Outcome { effect, outcome: EffectOutcome::Ok, .. }
            if effect == &control_effect)));
}

#[tokio::test]
async fn turn_cancel_emergency_stops_backend_and_journals_computer_cancelled() {
    let root = tempfile::tempdir().expect("profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitToolCall {
            call_id: "cu2-wait".into(),
            name: "computer".into(),
            args: serde_json::json!({"action": "wait", "ms": 60_000}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
    ]));
    let backend = Arc::new(FakeComputerBackend::blocking(large_png_fixture()));
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let factory: Arc<dyn TurnToolFactory> = Arc::new(BrokerToolFactory::with_computer_backend(
        Arc::clone(&backend) as Arc<dyn ComputerBackend>,
    ));
    let manager = WorkerManager::start(
        hub.clone(),
        WorkerDependencies {
            provider_factory: Arc::new(FixedProviderFactory {
                provider: Arc::clone(&provider),
            }),
            tool_factory: factory,
            delegation: None,
            web_search: None,
        },
        false,
    );
    hub.install_worker_manager(manager.handle())
        .expect("install manager");
    let session_id = SessionId::new("cu2-cancel-session");
    let run_id = RunId::new("cu2-cancel-run");
    let device_id = DeviceId::new("cu2-cancel-device");
    create_session(&hub, &session_id, &device_id).await;
    append_screen_control_grant(&store, &session_id).await;
    submit_turn(
        &hub,
        &manager.handle(),
        &store,
        &session_id,
        &run_id,
        device_id.clone(),
    )
    .await;

    timeout(Duration::from_secs(8), backend.entered.notified())
        .await
        .expect("fake backend enters in-flight wait");
    hub.cancel_internal_turn(TurnCancelCommand {
        command_id: "cancel-cu2-computer".into(),
        request_digest: "cancel-cu2-computer-digest".into(),
        request_json: r#"{"run":"cu2-cancel-run"}"#.into(),
        session_id: session_id.clone(),
        worker_generation: store.worker_generation(),
        run_id: run_id.clone(),
        cancelling_event_id: EventId::new("cu2-cancelling"),
        device_id,
    })
    .await
    .expect("cancel CU-2 turn");

    let events = wait_for_run_state(&store, &session_id, &run_id, RunState::Cancelled).await;
    assert!(backend.emergency_stopped.load(Ordering::Acquire));
    assert!(
        backend
            .active_cancel
            .lock()
            .expect("cancel lock")
            .as_ref()
            .is_some_and(ComputerCancelToken::is_cancelled)
    );
    let mut computer_effect = None;
    for event in &events {
        if let Ok(EventPayload::Effect(EffectPhase::Intent(intent))) =
            serde_json::from_value::<EventPayload>(event.payload.clone())
            && intent.class == EffectClass::ScreenControl
            && intent.summary == "computer wait"
        {
            computer_effect = Some(intent.effect);
        }
    }
    let computer_effect = computer_effect.expect("computer wait effect");
    let effect_phases = events
        .iter()
        .filter_map(|event| {
            let EventPayload::Effect(phase) =
                serde_json::from_value::<EventPayload>(event.payload.clone()).ok()?
            else {
                return None;
            };
            match &phase {
                EffectPhase::Intent(intent) if intent.effect == computer_effect => Some(phase),
                EffectPhase::Authorized { effect, .. }
                | EffectPhase::Dispatched { effect }
                | EffectPhase::Outcome { effect, .. }
                    if effect == &computer_effect =>
                {
                    Some(phase)
                }
                _ => None,
            }
        })
        .collect::<Vec<_>>();
    assert!(matches!(
        effect_phases.as_slice(),
        [
            EffectPhase::Intent(_),
            EffectPhase::Authorized {
                verdict: AuthorizationVerdict::Allow,
                ..
            },
            EffectPhase::Dispatched { .. },
            EffectPhase::Outcome {
                outcome: EffectOutcome::Cancelled,
                ..
            }
        ]
    ));
    let outcome_seq = events
        .iter()
        .find_map(|event| {
            matches!(
                serde_json::from_value::<EventPayload>(event.payload.clone()),
                Ok(EventPayload::Effect(EffectPhase::Outcome {
                    ref effect,
                    outcome: EffectOutcome::Cancelled,
                    ..
                })) if effect == &computer_effect
            )
            .then_some(event.seq)
        })
        .expect("cancelled effect outcome seq");
    let item_seq = events
        .iter()
        .find_map(|event| {
            matches!(
                serde_json::from_value::<EventPayload>(event.payload.clone()),
                Ok(EventPayload::Item(ItemEvent::Completed {
                    item: TurnItem::ToolCall {
                        ref call_id,
                        status: ToolStatus::Cancelled,
                        ..
                    },
                    ..
                })) if call_id == "cu2-wait"
            )
            .then_some(event.seq)
        })
        .expect("cancelled tool item seq");
    assert!(
        outcome_seq < item_seq,
        "effect cancellation must be durable before tool item cancellation"
    );
    assert_eq!(
        backend.actions.lock().expect("actions lock").as_slice(),
        &[ComputerAction::Wait { ms: 60_000 }]
    );
}
