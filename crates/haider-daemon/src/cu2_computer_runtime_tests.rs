#![allow(clippy::expect_used)]
//! CU-2 fake-backend runtime coverage. Tests in this module never touch real
//! screen or input APIs.

use crate::session_hub::{SessionHub, SessionHubConfig};
use crate::turn_recovery::{RecoveredWork, recover_interrupted_turns};
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
use haider_protocol::graph::{
    ComputerObservationKind, EvidenceAuthority, GraphEvidenceSource, SHIP_LOOP_TEMPLATE,
};
use haider_protocol::ids::{DeviceId, EffectId, EventId, GraphId, MenuId, RunId, SessionId};
use haider_protocol::item::{ItemEvent, ToolStatus, TurnItem};
use haider_protocol::menu::{
    AnswerVia, DecisionKind, Menu, MenuAnswer, MenuKind, MenuOption, MenuScope,
};
use haider_protocol::permission::{
    PermissionEventPayload, PermissionGrantAction, PermissionGrantResolution, SystemPermission,
};
use haider_protocol::provider::{Block, FinishReason};
use haider_protocol::session::SessionInteractionModeV1;
use haider_protocol::state::RunState;
use haider_protocol::tool::ToolResultStatus;
use haider_provider::{FakeProvider, FakeStep, Provider, TurnRequest};
use haider_store::{
    GraphPinCommand, MenuResolutionCommand, SessionCreateCommand, TurnAcceptCommand,
    TurnCancelCommand,
};
use haider_tools::{
    ComputerBackend, ComputerCancelToken, ComputerError, ComputerInspection,
    ComputerInspectionBounds, ComputerOutput, ComputerPermissionPoll, ComputerResult,
    ExcludeRegionScreenshotRedaction, ScreenshotRedactionRegion,
};
use image::{DynamicImage, ImageFormat};
use std::io::Cursor;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
            account_incarnation: None,
            active_no_auth: false,
            initial_rotation: None,
            rotation_budget_consumed: false,
            attempt_resolver: None,
            compaction_promotion: None,
        })
    }
}

struct FakeComputerBackend {
    screenshot: Vec<u8>,
    inspect_screenshot: Vec<u8>,
    actions: Mutex<Vec<ComputerAction>>,
    viewports: Mutex<Vec<(u32, u32)>>,
    crops: Mutex<Vec<Option<haider_tools::ComputerScreenshotCrop>>>,
    block_wait: bool,
    entered: Notify,
    active_cancel: Mutex<Option<ComputerCancelToken>>,
    emergency_stopped: AtomicBool,
}

impl FakeComputerBackend {
    fn new(screenshot: Vec<u8>) -> Self {
        Self {
            screenshot,
            inspect_screenshot: inspect_png_fixture(),
            actions: Mutex::new(Vec::new()),
            viewports: Mutex::new(Vec::new()),
            crops: Mutex::new(Vec::new()),
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
            ComputerAction::Inspect { .. } => Ok(ComputerOutput::Inspection {
                inspection: ComputerInspection {
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
                },
                screenshot_png: self.inspect_screenshot.clone(),
            }),
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

    fn set_viewport_region(
        &self,
        width: u32,
        height: u32,
        crop: Option<haider_tools::ComputerScreenshotCrop>,
    ) -> ComputerResult<()> {
        self.crops.lock().expect("crop lock").push(crop);
        self.set_viewport(width, height)
    }

    async fn emergency_stop(&self) -> ComputerResult<()> {
        self.emergency_stopped.store(true, Ordering::Release);
        if let Some(cancel) = self.active_cancel.lock().expect("cancel lock").as_ref() {
            cancel.cancel();
        }
        Ok(())
    }
}

struct PermissionFlipBackend {
    screenshot: Vec<u8>,
    permission: SystemPermission,
    poll_result: ComputerPermissionPoll,
    granted: AtomicBool,
    prompts: AtomicUsize,
    attempts: AtomicUsize,
    polls: AtomicUsize,
}

#[async_trait]
impl ComputerBackend for PermissionFlipBackend {
    async fn prepare(
        &self,
        action: &ComputerAction,
        cancel: &ComputerCancelToken,
    ) -> ComputerResult<()> {
        cancel.check()?;
        assert_eq!(action, &ComputerAction::Screenshot);
        if self.granted.load(Ordering::Acquire) {
            return Ok(());
        }
        self.prompts.fetch_add(1, Ordering::AcqRel);
        let (settings_pane, settings_url) = match self.permission {
            SystemPermission::ScreenRecording => (
                "System Settings > Privacy & Security > Screen Recording",
                "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture",
            ),
            SystemPermission::Accessibility => (
                "System Settings > Privacy & Security > Accessibility",
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility",
            ),
        };
        Err(ComputerError::PermissionRequired {
            permission: self.permission,
            settings_pane: settings_pane.into(),
            settings_url: settings_url.into(),
            restart_required: false,
            message: "native prompt is waiting for Accessibility".into(),
        })
    }

    async fn execute(
        &self,
        action: &ComputerAction,
        cancel: &ComputerCancelToken,
    ) -> ComputerResult<ComputerOutput> {
        cancel.check()?;
        assert_eq!(action, &ComputerAction::Screenshot);
        assert!(self.granted.load(Ordering::Acquire));
        self.attempts.fetch_add(1, Ordering::AcqRel);
        Ok(ComputerOutput::ScreenshotPng(self.screenshot.clone()))
    }

    async fn poll_permission(
        &self,
        permission: SystemPermission,
        cancel: &ComputerCancelToken,
        _timeout: Duration,
    ) -> ComputerResult<ComputerPermissionPoll> {
        cancel.check()?;
        assert_eq!(permission, self.permission);
        self.polls.fetch_add(1, Ordering::AcqRel);
        if self.poll_result == ComputerPermissionPoll::Granted {
            self.granted.store(true, Ordering::Release);
        }
        Ok(self.poll_result)
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

fn inspect_png_fixture() -> Vec<u8> {
    let pixels = image::RgbaImage::from_pixel(1_500, 700, image::Rgba([181, 43, 79, 255]));
    let mut encoded = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(pixels)
        .write_to(&mut encoded, ImageFormat::Png)
        .expect("encode inspect PNG fixture");
    encoded.into_inner()
}

async fn create_session(hub: &SessionHub, session_id: &SessionId, device_id: &DeviceId) {
    create_session_with_interaction_mode(
        hub,
        session_id,
        device_id,
        SessionInteractionModeV1::Interactive,
    )
    .await;
}

async fn create_session_with_interaction_mode(
    hub: &SessionHub,
    session_id: &SessionId,
    device_id: &DeviceId,
    interaction_mode: SessionInteractionModeV1,
) {
    let cwd = std::fs::canonicalize(std::env::current_dir().expect("cwd"))
        .expect("canonical cwd")
        .to_string_lossy()
        .into_owned();
    hub.create_internal_session_with_interaction_mode(
        SessionCreateCommand {
            command_id: format!("create-{session_id}"),
            request_digest: format!("create-{session_id}-digest"),
            request_json: format!(r#"{{"session":"{session_id}"}}"#),
            session_id: session_id.clone(),
            cwd,
            provider: "fake".into(),
            model: "fake-model".into(),
            max_tokens: 4096,
            max_tokens_source: None,
            permission_overrides: None,
            effort: None,
            fast: false,
            cache_policy: Default::default(),
            system_prompt_version: crate::worker::SystemPromptBuilder::VERSION.into(),
            event_id: EventId::new(format!("{session_id}-created")),
            device_id: device_id.clone(),
        },
        interaction_mode,
    )
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
        payload: serde_json::to_value(payload)
            .expect("grant payload serializes")
            .into(),
    }
}

async fn append_screen_control_grant(store: &SqliteStoreHandle, session_id: &SessionId) {
    let effect = EffectId::new(format!("{session_id}-control-effect"));
    let menu_id = MenuId::new(format!("{session_id}-control-menu"));
    let menu = Menu {
        id: menu_id.clone(),
        kind: MenuKind::Permission {
            effect_summary: "computer left_click".into(),
            file_review: None,
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
            text: "computer-use my screen".into(),
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
    match timeout(Duration::from_secs(12), async {
        loop {
            let events = store.read(session_id, 0, 4096).await.expect("journal");
            if events.iter().any(|event| {
                event.run_id.as_ref() == Some(run_id)
                    && event
                        .payload
                        .decode_event()
                        .is_ok_and(|payload| payload == EventPayload::RunState(expected.clone()))
            }) {
                break events;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    {
        Ok(events) => events,
        Err(_) => {
            let events = store
                .read(session_id, 0, 4096)
                .await
                .expect("diagnostic journal");
            panic!("run did not reach {expected:?}: {events:#?}");
        }
    }
}

async fn abandon_active_graph_and_wait_for_done(
    hub: &SessionHub,
    store: &SqliteStoreHandle,
    session_id: &SessionId,
    run_id: &RunId,
    device_id: DeviceId,
) -> Vec<haider_protocol::envelope::RawEnvelope> {
    let pending_menu = timeout(Duration::from_secs(12), async {
        loop {
            let events = store.read(session_id, 0, 4096).await.expect("journal");
            if let Some(opening) = events.into_iter().find(|event| {
                event.run_id.as_ref() == Some(run_id)
                    && event.payload.decode_event().is_ok_and(|payload| {
                        matches!(
                            payload,
                            EventPayload::MenuOpened(ref menu)
                                if matches!(menu.kind, MenuKind::GraphAbandonConfirm { .. })
                        )
                    })
            }) {
                let EventPayload::MenuOpened(menu) =
                    serde_json::from_value(opening.payload.into()).expect("typed graph menu")
                else {
                    unreachable!();
                };
                break (menu, opening.seq);
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    let (menu, request_seq) = match pending_menu {
        Ok(menu) => menu,
        Err(_) => {
            let events = store
                .read(session_id, 0, 4096)
                .await
                .expect("diagnostic journal");
            let payloads = events
                .iter()
                .map(|event| (event.seq, event.run_id.clone(), event.payload.clone()))
                .collect::<Vec<_>>();
            panic!("graph finalization did not open abandonment confirmation: {payloads:#?}");
        }
    };
    hub.resolve_hook_menu(MenuResolutionCommand {
        command_id: format!("abandon-{run_id}"),
        session_id: session_id.clone(),
        request_seq,
        worker_generation: store.worker_generation(),
        allow_prior_generation: false,
        answer: MenuAnswer {
            menu: menu.id,
            option_key: Some("abandon-and-finish".into()),
            option_index: 1,
            value: None,
            via: AnswerVia::Rpc,
        },
        device_id,
        input_is_secret_reference: false,
    })
    .await
    .expect("abandon graph after computer evidence");
    wait_for_run_state(store, session_id, run_id, RunState::Done).await
}

#[tokio::test]
async fn explicit_intent_auto_grants_and_tcc_flip_retries_the_parked_action() {
    let root = tempfile::tempdir().expect("profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let provider = Arc::new(
        FakeProvider::new(vec![
            FakeStep::EmitToolCall {
                call_id: "permission-screenshot".into(),
                name: "computer".into(),
                args: serde_json::json!({"action": "screenshot"}),
            },
            FakeStep::Finish {
                reason: FinishReason::ToolUse,
            },
            FakeStep::ExpectToolResult {
                call_id: "permission-screenshot".into(),
            },
            FakeStep::EmitText {
                text: "capture complete".into(),
            },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
        ])
        .with_vision_native(),
    );
    let backend = Arc::new(PermissionFlipBackend {
        screenshot: inspect_png_fixture(),
        permission: SystemPermission::Accessibility,
        poll_result: ComputerPermissionPoll::Granted,
        granted: AtomicBool::new(false),
        prompts: AtomicUsize::new(0),
        attempts: AtomicUsize::new(0),
        polls: AtomicUsize::new(0),
    });
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let factory: Arc<dyn TurnToolFactory> = Arc::new(BrokerToolFactory::with_computer_backend(
        Arc::clone(&backend) as Arc<dyn ComputerBackend>,
    ));
    let manager = WorkerManager::start(
        hub.clone(),
        WorkerDependencies {
            diagnostics: None,
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
    let session_id = SessionId::new("permission-retry-session");
    let run_id = RunId::new("permission-retry-run");
    let device_id = DeviceId::new("permission-retry-device");
    create_session(&hub, &session_id, &device_id).await;
    // `submit_turn` commits the explicit `computer-use` opt-in. No
    // permission-menu answer or preconfigured screen grant is installed.
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
    assert_eq!(backend.prompts.load(Ordering::Acquire), 1);
    assert_eq!(backend.attempts.load(Ordering::Acquire), 1);
    assert_eq!(backend.polls.load(Ordering::Acquire), 1);

    let mut needed = None;
    let mut resolved = None;
    let mut permission_menu = None;
    let mut tool_result_seq = None;
    let mut observe_effect = None;
    let mut observe_authorized = false;
    for event in &events {
        match PermissionEventPayload::from_payload_value(event.payload.clone().into()) {
            Ok(PermissionEventPayload::PermissionGrantNeeded(event_payload)) => {
                needed = Some((event.seq, event_payload));
                continue;
            }
            Ok(PermissionEventPayload::PermissionGrantResolved(event_payload)) => {
                resolved = Some((event.seq, event_payload));
                continue;
            }
            Err(_) => {}
        }
        match event.payload.decode_event() {
            Ok(EventPayload::ToolResult { call_id, result })
                if call_id == "permission-screenshot" && !result.images.is_empty() =>
            {
                tool_result_seq = Some(event.seq);
            }
            Ok(EventPayload::MenuOpened(menu))
                if menu.origin == super::COMPUTER_PERMISSION_MENU_ORIGIN =>
            {
                permission_menu = Some((event.seq, event.worker_generation, menu));
            }
            Ok(EventPayload::Effect(EffectPhase::Intent(intent)))
                if intent.class == EffectClass::ScreenObserve =>
            {
                observe_effect = Some(intent.effect);
            }
            Ok(EventPayload::Effect(EffectPhase::Authorized {
                effect,
                verdict: AuthorizationVerdict::Allow,
            })) if observe_effect.as_ref() == Some(&effect) => observe_authorized = true,
            _ => {}
        }
    }
    let (needed_seq, needed) = needed.expect("grant-needed event");
    assert_eq!(needed.call_id, "permission-screenshot");
    assert_eq!(needed.permission, SystemPermission::Accessibility);
    assert_eq!(
        needed.actions,
        vec![
            PermissionGrantAction::OpenSettings,
            PermissionGrantAction::Retry,
            PermissionGrantAction::RestartDaemon,
        ]
    );
    assert!(!needed.auto_restart_pending);
    let (menu_seq, menu_generation, menu) = permission_menu.expect("durable OS permission menu");
    assert_eq!(needed.menu_id, menu.id);
    assert_eq!(needed.request_seq, menu_seq);
    assert_eq!(needed.opening_generation, menu_generation);
    assert_eq!(menu.origin, super::COMPUTER_PERMISSION_MENU_ORIGIN);
    let (resolved_seq, resolved) = resolved.expect("grant resolution event");
    assert_eq!(resolved.request_id, needed.request_id);
    assert_eq!(resolved.resolution, PermissionGrantResolution::Granted);
    assert!(resolved.retrying_parked_action);
    assert!(needed_seq < resolved_seq);
    assert!(resolved_seq < tool_result_seq.expect("successful screenshot result"));
    assert!(!events.iter().any(|event| {
        event.seq < resolved_seq
            && matches!(
                event.payload.decode_event(),
                Ok(EventPayload::Effect(EffectPhase::Dispatched { .. }))
            )
    }));
    assert!(
        observe_authorized,
        "explicit intent must authorize without Ask"
    );

    manager.shutdown().await.expect("manager shutdown");
    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}

#[tokio::test]
async fn autonomous_os_permission_fails_typed_without_a_menu_or_dispatch() {
    let root = tempfile::tempdir().expect("profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let provider = Arc::new(
        FakeProvider::new(vec![
            FakeStep::EmitToolCall {
                call_id: "autonomous-permission-screenshot".into(),
                name: "computer".into(),
                args: serde_json::json!({"action": "screenshot"}),
            },
            FakeStep::Finish {
                reason: FinishReason::ToolUse,
            },
            FakeStep::ExpectToolResult {
                call_id: "autonomous-permission-screenshot".into(),
            },
            FakeStep::EmitText {
                text: "permission was not granted".into(),
            },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
        ])
        .with_vision_native(),
    );
    let backend = Arc::new(PermissionFlipBackend {
        screenshot: inspect_png_fixture(),
        permission: SystemPermission::Accessibility,
        poll_result: ComputerPermissionPoll::Granted,
        granted: AtomicBool::new(false),
        prompts: AtomicUsize::new(0),
        attempts: AtomicUsize::new(0),
        polls: AtomicUsize::new(0),
    });
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let manager = WorkerManager::start(
        hub.clone(),
        WorkerDependencies {
            diagnostics: None,
            provider_factory: Arc::new(FixedProviderFactory { provider }),
            tool_factory: Arc::new(BrokerToolFactory::with_computer_backend(
                Arc::clone(&backend) as Arc<dyn ComputerBackend>,
            )),
            delegation: None,
            web_search: None,
        },
        false,
    );
    hub.install_worker_manager(manager.handle())
        .expect("install manager");
    let session_id = SessionId::new("autonomous-permission-session");
    let run_id = RunId::new("autonomous-permission-run");
    let device_id = DeviceId::new("autonomous-permission-device");
    create_session_with_interaction_mode(
        &hub,
        &session_id,
        &device_id,
        SessionInteractionModeV1::Autonomous,
    )
    .await;
    // The explicit marker is enough to grant the broker effect, so this test
    // reaches the independent OS permission gate.
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
    assert_eq!(backend.prompts.load(Ordering::Acquire), 1);
    assert_eq!(backend.attempts.load(Ordering::Acquire), 0);
    assert_eq!(backend.polls.load(Ordering::Acquire), 0);
    assert!(!events.iter().any(|event| matches!(
        event.payload.decode_event(),
        Ok(EventPayload::RunState(RunState::PermissionRequired { .. }))
    )));
    assert!(!events.iter().any(|event| matches!(
        event.payload.decode_event(),
        Ok(EventPayload::MenuOpened(ref menu))
            if menu.origin == super::COMPUTER_PERMISSION_MENU_ORIGIN
    )));
    assert!(!events.iter().any(|event| matches!(
        event.payload.decode_event(),
        Ok(EventPayload::Effect(EffectPhase::Dispatched { .. }))
    )));
    let result = events
        .iter()
        .find_map(|event| match event.payload.decode_event() {
            Ok(EventPayload::ToolResult { call_id, result })
                if call_id == "autonomous-permission-screenshot" =>
            {
                Some(result)
            }
            _ => None,
        })
        .expect("typed permission result");
    assert_eq!(result.status, ToolResultStatus::Rejected);
    assert!(result.preview.contains("permission_denied"));
    assert!(
        result
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("no_human_available"))
    );

    manager.shutdown().await.expect("manager shutdown");
    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}

#[tokio::test]
async fn permission_poll_timeout_leaves_the_card_actionable_and_undispatched() {
    let root = tempfile::tempdir().expect("profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let provider = Arc::new(
        FakeProvider::new(vec![
            FakeStep::EmitToolCall {
                call_id: "timeout-screenshot".into(),
                name: "computer".into(),
                args: serde_json::json!({"action": "screenshot"}),
            },
            FakeStep::Finish {
                reason: FinishReason::ToolUse,
            },
            FakeStep::ExpectToolResult {
                call_id: "timeout-screenshot".into(),
            },
            FakeStep::EmitText {
                text: "capture resumed after manual retry".into(),
            },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
        ])
        .with_vision_native(),
    );
    let backend = Arc::new(PermissionFlipBackend {
        screenshot: inspect_png_fixture(),
        permission: SystemPermission::Accessibility,
        poll_result: ComputerPermissionPoll::TimedOut,
        granted: AtomicBool::new(false),
        prompts: AtomicUsize::new(0),
        attempts: AtomicUsize::new(0),
        polls: AtomicUsize::new(0),
    });
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let manager = WorkerManager::start(
        hub.clone(),
        WorkerDependencies {
            diagnostics: None,
            provider_factory: Arc::new(FixedProviderFactory { provider }),
            tool_factory: Arc::new(BrokerToolFactory::with_computer_backend(
                Arc::clone(&backend) as Arc<dyn ComputerBackend>,
            )),
            delegation: None,
            web_search: None,
        },
        false,
    );
    hub.install_worker_manager(manager.handle())
        .expect("install manager");
    let session_id = SessionId::new("permission-timeout-session");
    let run_id = RunId::new("permission-timeout-run");
    let device_id = DeviceId::new("permission-timeout-device");
    create_session(&hub, &session_id, &device_id).await;
    submit_turn(
        &hub,
        &manager.handle(),
        &store,
        &session_id,
        &run_id,
        device_id.clone(),
    )
    .await;
    let events = timeout(Duration::from_secs(12), async {
        loop {
            if backend.polls.load(Ordering::Acquire) == 1 {
                break store.read(&session_id, 0, 4096).await.expect("journal");
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("bounded fake poll finishes");
    assert!(events.iter().any(|event| matches!(
        event.payload.decode_event(),
        Ok(EventPayload::RunState(RunState::PermissionRequired { .. }))
    )));
    assert!(events.iter().any(|event| matches!(
        PermissionEventPayload::from_payload_value(event.payload.clone().into()),
        Ok(PermissionEventPayload::PermissionGrantNeeded(_))
    )));
    assert!(!events.iter().any(|event| matches!(
        PermissionEventPayload::from_payload_value(event.payload.clone().into()),
        Ok(PermissionEventPayload::PermissionGrantResolved(_))
    )));
    assert!(!events.iter().any(|event| matches!(
        event.payload.decode_event(),
        Ok(EventPayload::Effect(EffectPhase::Dispatched { .. }))
    )));

    let (request_seq, worker_generation, menu) = events
        .iter()
        .find_map(|event| match event.payload.decode_event() {
            Ok(EventPayload::MenuOpened(menu))
                if menu.origin == super::COMPUTER_PERMISSION_MENU_ORIGIN =>
            {
                Some((event.seq, event.worker_generation, menu))
            }
            _ => None,
        })
        .expect("durable retry menu");
    backend.granted.store(true, Ordering::Release);
    hub.resolve_hook_menu(MenuResolutionCommand {
        command_id: "manual-computer-permission-retry".into(),
        session_id: session_id.clone(),
        request_seq,
        worker_generation,
        allow_prior_generation: false,
        answer: MenuAnswer {
            menu: menu.id,
            option_key: Some("retry".into()),
            option_index: 0,
            value: None,
            via: AnswerVia::Rpc,
        },
        device_id,
        input_is_secret_reference: false,
    })
    .await
    .expect("manual retry is accepted");
    let resumed = wait_for_run_state(&store, &session_id, &run_id, RunState::Done).await;
    assert_eq!(backend.attempts.load(Ordering::Acquire), 1);
    assert!(resumed.iter().any(|event| matches!(
        event.payload.decode_event(),
        Ok(EventPayload::ToolResult { ref call_id, .. }) if call_id == "timeout-screenshot"
    )));

    manager.shutdown().await.expect("manager shutdown");
    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}

#[tokio::test]
async fn screen_recording_flip_parks_then_fresh_daemon_resumes_the_exact_call() {
    let root = tempfile::tempdir().expect("profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let provider = Arc::new(
        FakeProvider::new(vec![
            FakeStep::EmitToolCall {
                call_id: "restart-screenshot".into(),
                name: "computer".into(),
                args: serde_json::json!({"action": "screenshot"}),
            },
            FakeStep::Finish {
                reason: FinishReason::ToolUse,
            },
            FakeStep::ExpectToolResult {
                call_id: "restart-screenshot".into(),
            },
            FakeStep::EmitText {
                text: "capture resumed".into(),
            },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
        ])
        .with_vision_native(),
    );
    let first_backend = Arc::new(PermissionFlipBackend {
        screenshot: inspect_png_fixture(),
        permission: SystemPermission::ScreenRecording,
        poll_result: ComputerPermissionPoll::RestartRequired,
        granted: AtomicBool::new(false),
        prompts: AtomicUsize::new(0),
        attempts: AtomicUsize::new(0),
        polls: AtomicUsize::new(0),
    });
    let first_hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let first_manager = WorkerManager::start(
        first_hub.clone(),
        WorkerDependencies {
            diagnostics: None,
            provider_factory: Arc::new(FixedProviderFactory {
                provider: Arc::clone(&provider),
            }),
            tool_factory: Arc::new(BrokerToolFactory::with_computer_backend(Arc::clone(
                &first_backend,
            )
                as Arc<dyn ComputerBackend>)),
            delegation: None,
            web_search: None,
        },
        false,
    );
    first_hub
        .install_worker_manager(first_manager.handle())
        .expect("install first manager");
    let session_id = SessionId::new("permission-restart-session");
    let run_id = RunId::new("permission-restart-run");
    let device_id = DeviceId::new("permission-restart-device");
    create_session(&first_hub, &session_id, &device_id).await;
    submit_turn(
        &first_hub,
        &first_manager.handle(),
        &store,
        &session_id,
        &run_id,
        device_id.clone(),
    )
    .await;

    let before_restart = timeout(Duration::from_secs(12), async {
        loop {
            let events = store.read(&session_id, 0, 4096).await.expect("journal");
            if events.iter().any(|event| {
                matches!(
                    PermissionEventPayload::from_payload_value(event.payload.clone().into()),
                    Ok(PermissionEventPayload::PermissionGrantNeeded(ref needed))
                        if needed.auto_restart_pending
                )
            }) {
                break events;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("restart-required grant card");
    assert_eq!(first_backend.prompts.load(Ordering::Acquire), 1);
    assert_eq!(first_backend.polls.load(Ordering::Acquire), 1);
    assert_eq!(first_backend.attempts.load(Ordering::Acquire), 0);
    assert!(!before_restart.iter().any(|event| matches!(
        event.payload.decode_event(),
        Ok(EventPayload::Effect(EffectPhase::Dispatched { .. }))
    )));

    first_manager.shutdown().await.expect("drain first manager");
    first_hub.shutdown().await.expect("shutdown first hub");
    store.close().await.expect("close first store");

    let recovered_store = SqliteStoreHandle::open(root.path())
        .await
        .expect("reopen store");
    let mut recovered = recover_interrupted_turns(&recovered_store, &device_id)
        .await
        .expect("recover parked permission");
    if recovered.len() != 1 {
        let events = recovered_store
            .read(&session_id, 0, 4096)
            .await
            .expect("recovery diagnostics");
        panic!("expected one recovered permission checkpoint: {events:#?}");
    }
    let RecoveredWork::Checkpoint(recovered) = recovered.remove(0) else {
        panic!("screen permission wait must recover as a checkpoint");
    };
    assert_eq!(
        recovered.checkpoint.menu.origin,
        super::COMPUTER_PERMISSION_MENU_ORIGIN
    );
    assert_eq!(recovered.provider_request_ordinal, 1);

    let fresh_backend = Arc::new(PermissionFlipBackend {
        screenshot: inspect_png_fixture(),
        permission: SystemPermission::ScreenRecording,
        poll_result: ComputerPermissionPoll::TimedOut,
        granted: AtomicBool::new(true),
        prompts: AtomicUsize::new(0),
        attempts: AtomicUsize::new(0),
        polls: AtomicUsize::new(0),
    });
    let fresh_hub =
        SessionHub::new(recovered_store.clone(), SessionHubConfig::default()).expect("fresh hub");
    let fresh_manager = WorkerManager::start(
        fresh_hub.clone(),
        WorkerDependencies {
            diagnostics: None,
            provider_factory: Arc::new(FixedProviderFactory { provider }),
            tool_factory: Arc::new(BrokerToolFactory::with_computer_backend(Arc::clone(
                &fresh_backend,
            )
                as Arc<dyn ComputerBackend>)),
            delegation: None,
            web_search: None,
        },
        false,
    );
    fresh_hub
        .install_worker_manager(fresh_manager.handle())
        .expect("install fresh manager");
    fresh_manager
        .handle()
        .recover_checkpoint(
            recovered.accepted,
            recovered.checkpoint,
            recovered.committed_answer,
            recovered.provider_request_ordinal,
        )
        .await
        .expect("resume parked permission");
    let events = wait_for_run_state(&recovered_store, &session_id, &run_id, RunState::Done).await;
    assert_eq!(fresh_backend.prompts.load(Ordering::Acquire), 0);
    assert_eq!(fresh_backend.polls.load(Ordering::Acquire), 0);
    assert_eq!(fresh_backend.attempts.load(Ordering::Acquire), 1);
    assert!(events.iter().any(|event| matches!(
        event.payload.decode_event(),
        Ok(EventPayload::ToolResult { ref call_id, .. }) if call_id == "restart-screenshot"
    )));

    fresh_manager.shutdown().await.expect("manager shutdown");
    fresh_hub.shutdown().await.expect("hub shutdown");
    recovered_store.close().await.expect("store close");
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
            FakeStep::EmitText {
                text: "computer complete; graph remains intentionally active".into(),
            },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
        ])
        .with_vision_native(),
    );
    let backend = Arc::new(FakeComputerBackend::new(large_png_fixture()));
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let factory: Arc<dyn TurnToolFactory> =
        Arc::new(BrokerToolFactory::with_computer_backend_and_redaction(
            Arc::clone(&backend) as Arc<dyn ComputerBackend>,
            Arc::new(
                ExcludeRegionScreenshotRedaction::new(vec![ScreenshotRedactionRegion {
                    x: 0,
                    y: 0,
                    width: 3_000,
                    height: 1_000,
                }])
                .expect("redaction policy"),
            ),
        ));
    let manager = WorkerManager::start(
        hub.clone(),
        WorkerDependencies {
            diagnostics: None,
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
    hub.pin_graph(GraphPinCommand {
        command_id: "pin-cu6-computer-evidence".into(),
        request_digest: "pin-cu6-computer-evidence-digest".into(),
        request_json: r#"{"template":"ship-loop"}"#.into(),
        session_id: session_id.clone(),
        worker_generation: store.worker_generation(),
        graph_id: GraphId::new("cu6-computer-evidence"),
        template: SHIP_LOOP_TEMPLATE.into(),
        device_id: device_id.clone(),
    })
    .await
    .expect("pin graph for computer evidence");
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

    let events = abandon_active_graph_and_wait_for_done(
        &hub,
        &store,
        &session_id,
        &run_id,
        device_id.clone(),
    )
    .await;
    let (journal_json, image) = events
        .iter()
        .find_map(|event| {
            let payload = event.payload.decode_event().ok()?;
            match payload {
                EventPayload::ToolResult { result, .. } if !result.images.is_empty() => {
                    Some((event.payload.to_string(), result.images[0].clone()))
                }
                _ => None,
            }
        })
        .expect("computer screenshot result");
    assert_eq!((image.width, image.height), (1_567, 522));
    assert!(!journal_json.contains("data_base64"));
    let cas_bytes = store.get(&image.artifact).await.expect("screenshot CAS");
    assert!(!journal_json.contains(&base64::engine::general_purpose::STANDARD.encode(&cas_bytes)));

    let requests: Vec<TurnRequest> = provider.requests();
    assert_eq!(requests.len(), 5);
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
    let redacted = image::load_from_memory(&cas_bytes)
        .expect("decode redacted CAS image")
        .to_rgba8();
    assert!(redacted.pixels().all(|pixel| pixel.0 == [0, 0, 0, 255]));
    let inspect_image = requests[2]
        .messages
        .iter()
        .find_map(|message| match message.blocks.as_slice() {
            [Block::ToolResult {
                call_id,
                preview,
                images,
                ..
            }] if call_id == "cu2-inspect"
                && preview == "{\"role\":\"button\",\"label\":\"Send message\",\"title\":\"Send\",\"bounds\":{\"x\":980,\"y\":320,\"width\":88,\"height\":42},\"value\":null}" => images.first().cloned(),
            _ => None,
        })
        .expect("provider inspect image");
    let (older_preview, older_images) = requests[2]
        .messages
        .iter()
        .find_map(|message| match message.blocks.as_slice() {
            [
                Block::ToolResult {
                    call_id,
                    preview,
                    images,
                    ..
                },
            ] if call_id == "cu2-screenshot" => Some((preview, images)),
            _ => None,
        })
        .expect("older screenshot result remains as retrievable history");
    assert!(older_images.is_empty());
    assert!(older_preview.contains("\"scope\":\"computer_screenshot_history\""));
    assert!(older_preview.contains(image.artifact.as_str()));
    assert_eq!(
        requests[2]
            .attachments
            .iter()
            .map(|attachment| &attachment.artifact)
            .collect::<Vec<_>>(),
        vec![&inspect_image.artifact],
        "only the newest full computer screenshot reaches the provider"
    );
    assert_ne!(inspect_image.artifact, image.artifact);
    assert_eq!((inspect_image.width, inspect_image.height), (1_500, 700));
    assert_eq!(
        backend.viewports.lock().expect("viewport lock").as_slice(),
        &[
            (image.width, image.height),
            (inspect_image.width, inspect_image.height),
        ]
    );
    let inspect_cas = store
        .get(&inspect_image.artifact)
        .await
        .expect("inspect screenshot CAS");
    let inspect_redacted = image::load_from_memory(&inspect_cas)
        .expect("decode redacted inspect image")
        .to_rgba8();
    assert!(
        inspect_redacted
            .pixels()
            .all(|pixel| pixel.0 == [0, 0, 0, 255])
    );
    assert!(requests[3].messages.iter().any(|message| {
        matches!(message.blocks.as_slice(), [Block::ToolResult { call_id, preview, images, .. }]
            if call_id == "cu2-click" && preview == "left_click completed" && images.is_empty())
    }));

    let mut control_effect = None;
    let mut inspect_effect = None;
    let mut phases = Vec::new();
    for event in &events {
        if let Ok(EventPayload::Effect(phase)) = event.payload.decode_event() {
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

    let graph = hub
        .graph_inspect(&session_id, None, u32::MAX)
        .await
        .expect("inspect computer evidence graph");
    assert_eq!(graph.snapshot.evidence.len(), 2);
    assert!(graph.snapshot.evidence.iter().all(|recorded| {
        recorded.authority == EvidenceAuthority::DaemonVerified
            && matches!(
                &recorded.source,
                GraphEvidenceSource::ComputerObservation {
                    observation,
                    image: evidence_image,
                    workspace_revision,
                    ..
                } if workspace_revision.as_str() == "workspace-revision:0"
                    && match observation {
                        ComputerObservationKind::Screenshot => evidence_image == &image,
                        ComputerObservationKind::Inspect => evidence_image == &inspect_image,
                    }
            )
    }));
    let status = hub
        .graph_status(&session_id)
        .await
        .expect("graph status")
        .expect("active graph");
    let build = status
        .nodes
        .iter()
        .find(|node| node.node.as_str() == "BUILD")
        .expect("BUILD node");
    assert_eq!(build.evidence, Default::default());
    assert!(!build.satisfied);
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
            diagnostics: None,
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
        if let Ok(EventPayload::Effect(EffectPhase::Intent(intent))) = event.payload.decode_event()
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
            let EventPayload::Effect(phase) = event.payload.decode_event().ok()? else {
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
                event.payload.decode_event(),
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
                event.payload.decode_event(),
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

#[tokio::test]
async fn screenshot_region_redacts_before_crop_and_installs_delivered_mapping() {
    let root = tempfile::tempdir().expect("profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitToolCall {
            call_id: "cu2-region".into(), name: "computer".into(),
            args: serde_json::json!({"action":"screenshot","region":{"x":100,"y":100,"width":200,"height":100,"reference_width":1500,"reference_height":500}}),
        },
        FakeStep::Finish { reason: FinishReason::ToolUse },
        FakeStep::ExpectToolResult { call_id: "cu2-region".into() },
        FakeStep::EmitText { text: "region complete".into() },
        FakeStep::Finish { reason: FinishReason::EndTurn },
    ]).with_vision_native());
    let backend = Arc::new(FakeComputerBackend::new(large_png_fixture()));
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let factory: Arc<dyn TurnToolFactory> =
        Arc::new(BrokerToolFactory::with_computer_backend_and_redaction(
            Arc::clone(&backend) as Arc<dyn ComputerBackend>,
            Arc::new(
                ExcludeRegionScreenshotRedaction::new(vec![ScreenshotRedactionRegion {
                    x: 200,
                    y: 200,
                    width: 10,
                    height: 10,
                }])
                .expect("redaction"),
            ),
        ));
    let manager = WorkerManager::start(
        hub.clone(),
        WorkerDependencies {
            diagnostics: None,
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
        .expect("manager");
    let session_id = SessionId::new("cu2-region-session");
    let run_id = RunId::new("cu2-region-run");
    let device_id = DeviceId::new("cu2-region-device");
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
    let image = events
        .iter()
        .find_map(|event| match event.payload.decode_event().ok()? {
            EventPayload::ToolResult { result, .. } => result.images.first().cloned(),
            _ => None,
        })
        .expect("region image admitted");
    assert_eq!((image.width, image.height), (400, 200));
    let cas = store.get(&image.artifact).await.expect("region CAS");
    let decoded = image::load_from_memory(&cas)
        .expect("region PNG")
        .into_rgba8();
    assert_eq!(decoded.get_pixel(0, 0).0, [0, 0, 0, 255]);
    assert_ne!(decoded.get_pixel(10, 0).0, [0, 0, 0, 255]);
    assert_eq!(
        backend.viewports.lock().expect("viewport").as_slice(),
        &[(image.width, image.height)]
    );
    assert_eq!(
        backend.crops.lock().expect("crop").as_slice(),
        &[Some(haider_tools::ComputerScreenshotCrop {
            x: 200,
            y: 200,
            width: 400,
            height: 200,
            source_width: 3000,
            source_height: 1000,
        })]
    );
    let requests = provider.requests();
    assert!(
        requests[1]
            .attachments
            .iter()
            .any(|attachment| attachment.artifact == image.artifact)
    );
}

/// A backend that honours its action cancel token the way the real macOS
/// backend does (returns `Cancelled` promptly instead of being dropped).
struct CancelAwareBackend {
    entered: Notify,
}

#[async_trait]
impl ComputerBackend for CancelAwareBackend {
    async fn execute(
        &self,
        action: &ComputerAction,
        cancel: &ComputerCancelToken,
    ) -> ComputerResult<ComputerOutput> {
        cancel.check()?;
        if matches!(action, ComputerAction::Wait { .. }) {
            self.entered.notify_one();
            while !cancel.is_cancelled() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            return Err(ComputerError::Cancelled);
        }
        Ok(ComputerOutput::Confirmed {
            action: "confirmed".into(),
        })
    }
}

/// Owner 2026-09-24 (cu-presence): the overlay's Stop button cancels the
/// in-flight computer action at once AND the run, through the same
/// receipt-backed turn cancellation as ESC, and no later CU action runs.
/// MUTATION CHECK: drop the run cancel hook (or the in-flight cancel) from
/// `CuPresence::stop`. Expected runtime failure: the run never reaches
/// `Cancelled` / the follow-up click is executed.
#[tokio::test]
async fn presence_stop_cancels_the_in_flight_action_and_the_run() {
    let root = tempfile::tempdir().expect("profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitToolCall {
            call_id: "presence-wait".into(),
            name: "computer".into(),
            args: serde_json::json!({"action": "wait", "ms": 60_000}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "presence-wait".into(),
        },
        FakeStep::EmitToolCall {
            call_id: "presence-click".into(),
            name: "computer".into(),
            args: serde_json::json!({"action": "left_click", "x": 1, "y": 1}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
    ]));
    let backend = Arc::new(CancelAwareBackend {
        entered: Notify::new(),
    });
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let presence =
        crate::cu_presence::CuPresence::new(Duration::from_secs(30), Duration::from_secs(1));
    let factory: Arc<dyn TurnToolFactory> = Arc::new(
        BrokerToolFactory::with_computer_backend(Arc::clone(&backend) as Arc<dyn ComputerBackend>)
            .with_presence(Arc::clone(&presence)),
    );
    let manager = WorkerManager::start(
        hub.clone(),
        WorkerDependencies {
            diagnostics: None,
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
    let session_id = SessionId::new("presence-stop-session");
    let run_id = RunId::new("presence-stop-run");
    let device_id = DeviceId::new("presence-stop-device");
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
        .expect("backend enters the in-flight wait");

    // The human presses Stop on the overlay.
    assert!(
        presence.stop(haider_tools::presence::PresenceSurface::Screen) >= 1,
        "the running computer-use run holds a screen presence lease"
    );

    let events = wait_for_run_state(&store, &session_id, &run_id, RunState::Cancelled).await;
    // The provider may race ahead and REQUEST another action before the
    // turn cancellation lands; it may be authorized but must never be
    // dispatched to the OS.
    let click_effects: Vec<EffectId> = events
        .iter()
        .filter_map(|event| match event.payload.decode_event() {
            Ok(EventPayload::Effect(EffectPhase::Intent(intent)))
                if intent.summary == "computer left_click" =>
            {
                Some(intent.effect)
            }
            _ => None,
        })
        .collect();
    let dispatched_click = events.iter().any(|event| {
        matches!(
            event.payload.decode_event(),
            Ok(EventPayload::Effect(EffectPhase::Dispatched { ref effect }))
                if click_effects.contains(effect)
        )
    });
    assert!(
        !dispatched_click,
        "no CU action may be dispatched after Stop"
    );
    let wait_cancelled = events.iter().any(|event| {
        matches!(
            event.payload.decode_event(),
            Ok(EventPayload::Effect(EffectPhase::Outcome {
                outcome: EffectOutcome::Cancelled,
                ..
            }))
        )
    });
    assert!(wait_cancelled, "the in-flight wait is journaled Cancelled");
    manager.shutdown().await.expect("manager shutdown");
    hub.shutdown().await.expect("hub shutdown");
}

/// A Screen renderer that records pointer commands but never acknowledges
/// them, so every click parks in the bounded pointer-ack wait.
struct SilentOverlay {
    pointers: Arc<AtomicUsize>,
}

impl crate::cu_presence::PresenceRenderer for SilentOverlay {
    fn send(&mut self, command: &haider_tools::presence::PresenceCommand) -> bool {
        if matches!(
            command,
            haider_tools::presence::PresenceCommand::Pointer { .. }
        ) {
            self.pointers.fetch_add(1, Ordering::SeqCst);
        }
        true
    }
    fn acknowledges_pointers(&self) -> bool {
        true
    }
}

/// Verifier finding (191a8d60) + the live "~60 s cancelling" hang: Stop
/// pressed while a click is parked on its pointer ack. The click must never
/// execute and the run must reach `Cancelled` promptly (the live repro sat in
/// `cancelling` until daemon shutdown).
/// MUTATION CHECK: remove the post-ack `is_stopped` re-check. Expected
/// runtime failure: the backend records the click.
#[tokio::test]
async fn presence_stop_during_a_parked_click_never_clicks_and_cancels_the_run() {
    let root = tempfile::tempdir().expect("profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let mut steps = Vec::new();
    for index in 0..4 {
        if index > 0 {
            steps.push(FakeStep::ExpectToolResult {
                call_id: format!("parked-{}", index - 1),
            });
        }
        steps.push(FakeStep::EmitToolCall {
            call_id: format!("parked-{index}"),
            name: "computer".into(),
            args: if index == 0 {
                serde_json::json!({"action": "screenshot"})
            } else {
                serde_json::json!({"action": "left_click", "x": 3, "y": 4})
            },
        });
        steps.push(FakeStep::Finish {
            reason: FinishReason::ToolUse,
        });
    }
    let provider = Arc::new(FakeProvider::new(steps));
    let backend = Arc::new(FakeComputerBackend::new(large_png_fixture()));
    let presence =
        crate::cu_presence::CuPresence::new(Duration::from_secs(30), Duration::from_secs(1));
    let pointers = Arc::new(AtomicUsize::new(0));
    presence.register_renderer(
        haider_tools::presence::PresenceSurface::Screen,
        Arc::new({
            let pointers = Arc::clone(&pointers);
            move |_sink| {
                Some(Box::new(SilentOverlay {
                    pointers: Arc::clone(&pointers),
                })
                    as Box<dyn crate::cu_presence::PresenceRenderer>)
            }
        }),
    );
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let factory: Arc<dyn TurnToolFactory> = Arc::new(
        BrokerToolFactory::with_computer_backend(Arc::clone(&backend) as Arc<dyn ComputerBackend>)
            .with_presence(Arc::clone(&presence)),
    );
    let manager = WorkerManager::start(
        hub.clone(),
        WorkerDependencies {
            diagnostics: None,
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
    let session_id = SessionId::new("presence-parked-session");
    let run_id = RunId::new("presence-parked-run");
    let device_id = DeviceId::new("presence-parked-device");
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
    // Pointers: screenshot (no ack wait) then the first click, which parks.
    timeout(Duration::from_secs(8), async {
        while pointers.load(Ordering::SeqCst) < 2 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("the first click reaches its pointer-ack wait");
    assert!(presence.stop(haider_tools::presence::PresenceSurface::Screen) >= 1);

    let events = wait_for_run_state(&store, &session_id, &run_id, RunState::Cancelled).await;
    let clicked = backend
        .actions
        .lock()
        .expect("actions lock")
        .iter()
        .any(|action| matches!(action, ComputerAction::LeftClick { .. }));
    assert!(!clicked, "no click reached the OS after Stop");
    assert!(events.iter().any(|event| matches!(
        event.payload.decode_event(),
        Ok(EventPayload::Effect(EffectPhase::Outcome {
            outcome: EffectOutcome::Cancelled,
            ..
        }))
    )));
    manager.shutdown().await.expect("manager shutdown");
    hub.shutdown().await.expect("hub shutdown");
}

/// Live 3-repair trial 1: Stop landed BETWEEN provider rounds (the previous
/// computer action had completed and the provider was preparing the next
/// request), and the run sat in `cancelling`. Drive exactly that moment —
/// either through the presence Stop or through the same internal turn
/// cancellation directly — and require a prompt `Cancelled`.
async fn cancel_between_rounds(via_presence: bool) {
    let root = tempfile::tempdir().expect("profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitToolCall {
            call_id: "between-0".into(),
            name: "computer".into(),
            args: serde_json::json!({"action": "screenshot"}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "between-0".into(),
        },
        FakeStep::Delay { ms: 5_000 },
        FakeStep::EmitToolCall {
            call_id: "between-1".into(),
            name: "computer".into(),
            args: serde_json::json!({"action": "left_click", "x": 3, "y": 4}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
    ]));
    let backend = Arc::new(FakeComputerBackend::new(large_png_fixture()));
    let presence =
        crate::cu_presence::CuPresence::new(Duration::from_secs(30), Duration::from_secs(1));
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let factory: Arc<dyn TurnToolFactory> = Arc::new(
        BrokerToolFactory::with_computer_backend(Arc::clone(&backend) as Arc<dyn ComputerBackend>)
            .with_presence(Arc::clone(&presence)),
    );
    let manager = WorkerManager::start(
        hub.clone(),
        WorkerDependencies {
            diagnostics: None,
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
    let tag = if via_presence { "presence" } else { "direct" };
    let session_id = SessionId::new(format!("between-{tag}-session"));
    let run_id = RunId::new(format!("between-{tag}-run"));
    let device_id = DeviceId::new(format!("between-{tag}-device"));
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
    // Wait for the screenshot's tool result, then let the next provider
    // round start (it sits in its 5 s delay).
    timeout(Duration::from_secs(8), async {
        loop {
            let events = store.read(&session_id, 0, 4096).await.expect("journal");
            if events.iter().any(|event| {
                matches!(
                    event.payload.decode_event(),
                    Ok(EventPayload::ToolResult { ref call_id, .. }) if call_id == "between-0"
                )
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("screenshot settles");
    tokio::time::sleep(Duration::from_millis(300)).await;
    if via_presence {
        assert_eq!(
            presence.stop(haider_tools::presence::PresenceSurface::Screen),
            1
        );
    } else {
        let request_json = format!(r#"{{"run":"{run_id}"}}"#);
        hub.cancel_internal_turn(TurnCancelCommand {
            command_id: format!("between-{tag}-cancel"),
            request_digest: crate::delegation::digest_bytes(request_json.as_bytes()),
            request_json,
            session_id: session_id.clone(),
            worker_generation: store.worker_generation(),
            run_id: run_id.clone(),
            cancelling_event_id: EventId::new(format!("between-{tag}-cancelling")),
            device_id,
        })
        .await
        .expect("cancel");
    }
    wait_for_run_state(&store, &session_id, &run_id, RunState::Cancelled).await;
    let clicked = backend
        .actions
        .lock()
        .expect("actions lock")
        .iter()
        .any(|action| matches!(action, ComputerAction::LeftClick { .. }));
    assert!(!clicked, "no click after the cancellation");
    manager.shutdown().await.expect("manager shutdown");
    hub.shutdown().await.expect("hub shutdown");
}

#[tokio::test]
async fn presence_stop_between_provider_rounds_cancels_promptly() {
    cancel_between_rounds(true).await;
}

#[tokio::test]
async fn direct_turn_cancel_between_provider_rounds_cancels_promptly() {
    cancel_between_rounds(false).await;
}

/// Fires a cancellation from INSIDE the completing action, so it lands while
/// the core is settling the tool result and opening the next provider round —
/// the exact window of live 3-repair trial 1 (`cancelling` 2 ms after
/// `provider_round_terminal`).
struct CancelOnReturnBackend {
    fire: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

#[async_trait]
impl ComputerBackend for CancelOnReturnBackend {
    async fn execute(
        &self,
        action: &ComputerAction,
        cancel: &ComputerCancelToken,
    ) -> ComputerResult<ComputerOutput> {
        cancel.check()?;
        if matches!(action, ComputerAction::LeftClick { .. })
            && let Some(fire) = self.fire.lock().expect("fire lock").take()
        {
            fire();
        }
        Ok(match action {
            ComputerAction::Screenshot => ComputerOutput::ScreenshotPng(large_png_fixture()),
            _ => ComputerOutput::Confirmed {
                action: "confirmed".into(),
            },
        })
    }
}

async fn cancel_while_settling(via_presence: bool, iteration: usize) {
    let root = tempfile::tempdir().expect("profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let mut steps = Vec::new();
    for index in 0..5 {
        if index > 0 {
            steps.push(FakeStep::ExpectToolResult {
                call_id: format!("settle-{}", index - 1),
            });
        }
        steps.push(FakeStep::EmitToolCall {
            call_id: format!("settle-{index}"),
            name: "computer".into(),
            args: if index == 0 {
                serde_json::json!({"action": "screenshot"})
            } else {
                serde_json::json!({"action": "left_click", "x": 3, "y": 4})
            },
        });
        steps.push(FakeStep::Finish {
            reason: FinishReason::ToolUse,
        });
    }
    let provider = Arc::new(FakeProvider::new(steps));
    let presence =
        crate::cu_presence::CuPresence::new(Duration::from_secs(30), Duration::from_secs(1));
    let backend = Arc::new(CancelOnReturnBackend {
        fire: Mutex::new(None),
    });
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let factory: Arc<dyn TurnToolFactory> = Arc::new(
        BrokerToolFactory::with_computer_backend(Arc::clone(&backend) as Arc<dyn ComputerBackend>)
            .with_presence(Arc::clone(&presence)),
    );
    let manager = WorkerManager::start(
        hub.clone(),
        WorkerDependencies {
            diagnostics: None,
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
    let tag = format!("{}-{iteration}", if via_presence { "p" } else { "d" });
    let session_id = SessionId::new(format!("settle-{tag}-session"));
    let run_id = RunId::new(format!("settle-{tag}-run"));
    let device_id = DeviceId::new(format!("settle-{tag}-device"));
    let fire: Box<dyn FnOnce() + Send> = if via_presence {
        let presence = Arc::clone(&presence);
        Box::new(move || {
            presence.stop(haider_tools::presence::PresenceSurface::Screen);
        })
    } else {
        let hub = hub.clone();
        let (session_id, run_id, device_id) =
            (session_id.clone(), run_id.clone(), device_id.clone());
        let generation = store.worker_generation();
        Box::new(move || {
            tokio::spawn(async move {
                let request_json = format!(r#"{{"run":"{run_id}"}}"#);
                let _ = hub
                    .cancel_internal_turn(TurnCancelCommand {
                        command_id: format!("settle-cancel-{run_id}"),
                        request_digest: crate::delegation::digest_bytes(request_json.as_bytes()),
                        request_json,
                        session_id,
                        worker_generation: generation,
                        run_id: run_id.clone(),
                        cancelling_event_id: EventId::new(format!("settle-cancelling-{run_id}")),
                        device_id,
                    })
                    .await;
            });
        })
    };
    *backend.fire.lock().expect("fire lock") = Some(fire);
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
    wait_for_run_state(&store, &session_id, &run_id, RunState::Cancelled).await;
    manager.shutdown().await.expect("manager shutdown");
    hub.shutdown().await.expect("hub shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn presence_stop_while_an_action_settles_cancels_promptly() {
    for iteration in 0..10 {
        cancel_while_settling(true, iteration).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_turn_cancel_while_an_action_settles_cancels_promptly() {
    for iteration in 0..10 {
        cancel_while_settling(false, iteration).await;
    }
}

/// Shared ordered log for the dispatcher-level conceal test.
type OrderLog = Arc<Mutex<Vec<String>>>;

/// Records every capture the dispatcher asks the OS for, in order.
struct LoggingBackend {
    log: OrderLog,
}

#[async_trait]
impl ComputerBackend for LoggingBackend {
    async fn execute(
        &self,
        action: &ComputerAction,
        cancel: &ComputerCancelToken,
    ) -> ComputerResult<ComputerOutput> {
        cancel.check()?;
        match action {
            ComputerAction::Screenshot => {
                self.log
                    .lock()
                    .expect("log")
                    .push("capture:screenshot".into());
                Ok(ComputerOutput::ScreenshotPng(large_png_fixture()))
            }
            ComputerAction::Inspect { .. } => {
                self.log.lock().expect("log").push("capture:inspect".into());
                Ok(ComputerOutput::Inspection {
                    inspection: ComputerInspection {
                        role: Some("button".into()),
                        label: None,
                        title: None,
                        bounds: None,
                        value: None,
                    },
                    screenshot_png: large_png_fixture(),
                })
            }
            _ => Ok(ComputerOutput::Confirmed {
                action: "confirmed".into(),
            }),
        }
    }
}

/// A capturable Screen renderer (a Linux notification, or an overlay whose
/// capture exclusion was refused) that logs conceal/reveal.
struct CapturableOverlay {
    log: OrderLog,
}

impl crate::cu_presence::PresenceRenderer for CapturableOverlay {
    fn send(&mut self, command: &haider_tools::presence::PresenceCommand) -> bool {
        use haider_tools::presence::PresenceCommand;
        let entry = match command {
            PresenceCommand::Conceal { .. } => "conceal",
            PresenceCommand::Reveal { .. } => "reveal",
            _ => return true,
        };
        self.log.lock().expect("log").push(entry.into());
        true
    }
    fn capturable(&self) -> bool {
        true
    }
}

/// Verifier finding (0ddd89a0): nothing tested that the broker dispatcher
/// itself conceals a capturable indicator around model-facing captures.
/// MUTATION CHECK: replace `self.cu_presence.conceal_for_capture().await`
/// in worker.rs with `None`. Expected runtime failure: no "conceal" around
/// either capture.
#[tokio::test]
async fn dispatcher_conceals_capturable_presence_around_screenshot_and_inspect() {
    let root = tempfile::tempdir().expect("profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitToolCall {
            call_id: "conceal-shot".into(),
            name: "computer".into(),
            args: serde_json::json!({"action": "screenshot"}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "conceal-shot".into(),
        },
        FakeStep::EmitToolCall {
            call_id: "conceal-inspect".into(),
            name: "computer".into(),
            args: serde_json::json!({"action": "inspect", "x": 3, "y": 4}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "conceal-inspect".into(),
        },
        FakeStep::EmitText {
            text: "done".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let log: OrderLog = Arc::new(Mutex::new(Vec::new()));
    let backend = Arc::new(LoggingBackend {
        log: Arc::clone(&log),
    });
    let presence =
        crate::cu_presence::CuPresence::new(Duration::from_secs(30), Duration::from_secs(1));
    presence.register_renderer(
        haider_tools::presence::PresenceSurface::Screen,
        Arc::new({
            let log = Arc::clone(&log);
            move |_sink| {
                Some(Box::new(CapturableOverlay {
                    log: Arc::clone(&log),
                })
                    as Box<dyn crate::cu_presence::PresenceRenderer>)
            }
        }),
    );
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let factory: Arc<dyn TurnToolFactory> = Arc::new(
        BrokerToolFactory::with_computer_backend(Arc::clone(&backend) as Arc<dyn ComputerBackend>)
            .with_presence(Arc::clone(&presence)),
    );
    let manager = WorkerManager::start(
        hub.clone(),
        WorkerDependencies {
            diagnostics: None,
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
    let session_id = SessionId::new("conceal-wiring-session");
    let run_id = RunId::new("conceal-wiring-run");
    let device_id = DeviceId::new("conceal-wiring-device");
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
    wait_for_run_state(&store, &session_id, &run_id, RunState::Done).await;
    let log = log.lock().expect("log").clone();
    assert_eq!(
        log,
        vec![
            "conceal",
            "capture:screenshot",
            "reveal",
            "conceal",
            "capture:inspect",
            "reveal",
        ],
        "each model-facing capture is bracketed by conceal/reveal"
    );
    manager.shutdown().await.expect("manager shutdown");
    hub.shutdown().await.expect("hub shutdown");
}

/// Verifier finding (0ddd89a0), "cause A": a Stop-refused action that
/// returns immediately (`Err(Cancelled)`, mapped to a `Cancelled` tool
/// result) lets the provider start ANOTHER round before the slower turn
/// cancellation commits (here Stop's cancel is delayed 400 ms), so the model
/// issues a further action after the human pressed Stop. Waiting for the
/// cancellation stops the run at the refused action.
/// MUTATION CHECK (cause A only; cause B stays fixed): make the Stop-refused
/// branch return `Err(ToolError::Computer(ComputerError::Cancelled))`.
/// Expected runtime failure: the "slow-2" tool call is started after Stop.
#[tokio::test]
async fn slow_cancel_commit_still_settles_a_stop_refused_action_as_cancelled() {
    let root = tempfile::tempdir().expect("profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let mut steps = Vec::new();
    for index in 0..4 {
        if index > 0 {
            steps.push(FakeStep::ExpectToolResult {
                call_id: format!("slow-{}", index - 1),
            });
        }
        steps.push(FakeStep::EmitToolCall {
            call_id: format!("slow-{index}"),
            name: "computer".into(),
            args: if index == 0 {
                serde_json::json!({"action": "screenshot"})
            } else {
                serde_json::json!({"action": "left_click", "x": 3, "y": 4})
            },
        });
        steps.push(FakeStep::Finish {
            reason: FinishReason::ToolUse,
        });
    }
    let provider = Arc::new(FakeProvider::new(steps));
    let backend = Arc::new(FakeComputerBackend::new(large_png_fixture()));
    let presence =
        crate::cu_presence::CuPresence::new(Duration::from_secs(30), Duration::from_secs(1));
    presence.set_stop_cancel_delay(Duration::from_millis(400));
    let pointers = Arc::new(AtomicUsize::new(0));
    presence.register_renderer(
        haider_tools::presence::PresenceSurface::Screen,
        Arc::new({
            let pointers = Arc::clone(&pointers);
            move |_sink| {
                Some(Box::new(SilentOverlay {
                    pointers: Arc::clone(&pointers),
                })
                    as Box<dyn crate::cu_presence::PresenceRenderer>)
            }
        }),
    );
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let factory: Arc<dyn TurnToolFactory> = Arc::new(
        BrokerToolFactory::with_computer_backend(Arc::clone(&backend) as Arc<dyn ComputerBackend>)
            .with_presence(Arc::clone(&presence)),
    );
    let manager = WorkerManager::start(
        hub.clone(),
        WorkerDependencies {
            diagnostics: None,
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
    let session_id = SessionId::new("slow-cancel-session");
    let run_id = RunId::new("slow-cancel-run");
    let device_id = DeviceId::new("slow-cancel-device");
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
    timeout(Duration::from_secs(8), async {
        while pointers.load(Ordering::SeqCst) < 2 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("the first click parks on its pointer ack");
    assert!(presence.stop(haider_tools::presence::PresenceSurface::Screen) >= 1);
    let events = wait_for_run_state(&store, &session_id, &run_id, RunState::Cancelled).await;
    assert!(
        !events.iter().any(|event| matches!(
            event.payload.decode_event(),
            Ok(EventPayload::RunFailed { .. } | EventPayload::RunState(RunState::Errored))
        )),
        "a Stop is a cancellation, never a failure"
    );
    let later_call = events.iter().any(|event| {
        matches!(
            event.payload.decode_event(),
            Ok(EventPayload::Item(ItemEvent::Started {
                item: TurnItem::ToolCall { ref call_id, .. },
                ..
            })) if call_id == "slow-2"
        )
    });
    assert!(
        !later_call,
        "no further model round (and no further action) starts after Stop"
    );
    let clicked = backend
        .actions
        .lock()
        .expect("actions lock")
        .iter()
        .any(|action| matches!(action, ComputerAction::LeftClick { .. }));
    assert!(!clicked, "no click reached the OS after Stop");
    manager.shutdown().await.expect("manager shutdown");
    hub.shutdown().await.expect("hub shutdown");
}
