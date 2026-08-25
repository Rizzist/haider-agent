//! Host-only mobile capability, dispatch-fence, and mock fast-path coverage.

#![allow(clippy::expect_used)]

use super::*;
use crate::session_hub::{SessionHub, SessionHubConfig};
use async_trait::async_trait;
use haider_core::{CancelToken, EventIdGenerator, MemoryStore, SqliteStoreHandle, StoreHandle};
use haider_protocol::envelope::{
    EventEnvelope, PromptRender, RawEnvelope, RenderTargets, SCHEMA_VERSION,
};
use haider_protocol::mobile::{MobileAction, MobileOutput, SmsMessage};
use haider_protocol::session::{SessionMetadataV1, SessionPermissionOverridesV1};
use haider_store::{SessionCreateCommand, TurnAcceptCommand};
use haider_tools::{MobileBackend, MobileCancelToken, MobileResult};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Debug)]
struct FakeMobileBackend {
    output: MobileOutput,
    calls: AtomicUsize,
    actions: Mutex<Vec<MobileAction>>,
}

impl FakeMobileBackend {
    fn new(output: MobileOutput) -> Self {
        Self {
            output,
            calls: AtomicUsize::new(0),
            actions: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl MobileBackend for FakeMobileBackend {
    async fn execute(
        &self,
        action: &MobileAction,
        cancel: &MobileCancelToken,
    ) -> MobileResult<MobileOutput> {
        cancel.check()?;
        self.calls.fetch_add(1, Ordering::AcqRel);
        self.actions
            .lock()
            .expect("mobile actions lock")
            .push(action.clone());
        Ok(self.output.clone())
    }
}

fn canned_sms() -> MobileOutput {
    MobileOutput::SmsList(vec![
        SmsMessage {
            id: "sms-1".into(),
            address: "+15550000001".into(),
            body: "First canned message".into(),
            date_ms: 1_725_000_000_000,
            folder: "inbox".into(),
        },
        SmsMessage {
            id: "sms-2".into(),
            address: "+15550000002".into(),
            body: "Second canned message".into(),
            date_ms: 1_725_000_000_500,
            folder: "inbox".into(),
        },
    ])
}

fn memory_envelope(
    session_id: &SessionId,
    event: &str,
    text: &str,
    agent_id: Option<AgentId>,
) -> RawEnvelope {
    EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(event),
        seq: 0,
        session_id: session_id.clone(),
        branch_id: None,
        run_id: Some(RunId::new(format!("{event}-run"))),
        agent_id,
        device_id: DeviceId::new("mobile-test-device"),
        authority_epoch: 0,
        worker_generation: 1,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Verbatim,
        },
        payload: serde_json::to_value(EventPayload::UserMessage {
            text: text.into(),
            attachments: Vec::new(),
            mode: DeliveryMode::Steer,
        })
        .expect("user message serializes"),
    }
}

/// MUTATION PIN: removing the unconditional mobile retain must make the
/// inactive assertion fail by exposing the schema.
#[test]
fn mobile_authorized_definitions_omit_inactive_include_active() {
    let factory: Arc<dyn TurnToolFactory> = Arc::new(BrokerToolFactory);
    let inactive = authorized_tool_definitions(&factory, None, false);
    assert!(!inactive.iter().any(|tool| tool.name == "mobile"));

    let active = authorized_tool_definitions(&factory, None, true);
    let mobile = active
        .iter()
        .find(|tool| tool.name == "mobile")
        .expect("active session advertises mobile exactly once");
    assert_eq!(
        mobile.input_schema["properties"]["action"]["enum"],
        serde_json::json!(["sms_read"])
    );
    assert_eq!(
        active.iter().filter(|tool| tool.name == "mobile").count(),
        1
    );
}

#[tokio::test]
async fn mobile_inactive_has_no_manual_or_inventory_trace() {
    let factory: Arc<dyn TurnToolFactory> = Arc::new(BrokerToolFactory);
    let inactive = authorized_tool_definitions(&factory, None, false);
    assert!(!tool_manual(&inactive).contains("\n- mobile("));

    let store = MemoryStore::new();
    let session_id = SessionId::new("mobile-inventory-gate");
    let snapshot = tool_inventory_snapshot(&store, &session_id)
        .await
        .expect("inactive inventory");
    assert!(
        !snapshot
            .tools
            .iter()
            .any(|entry| entry.manifest.name == "mobile")
    );

    let event = memory_envelope(
        &session_id,
        "mobile-inventory-activation",
        "/MoBiLe-UsE\tread messages",
        None,
    );
    store.append(&mut [event]).await.expect("append activation");
    let active_snapshot = tool_inventory_snapshot(&store, &session_id)
        .await
        .expect("active inventory");
    assert!(
        active_snapshot
            .tools
            .iter()
            .any(|entry| entry.manifest.name == "mobile")
    );
    let active = authorized_tool_definitions(&factory, None, true);
    assert!(tool_manual(&active).contains("\n- mobile("));
}

#[tokio::test]
async fn mobile_activation_is_root_prefix_bounded_case_insensitive() {
    for text in [
        "mobile-use",
        "/mobile-use read messages",
        " MoBiLe-UsE\tread messages ",
    ] {
        assert!(explicit_mobile_use_intent(text), "must activate: {text}");
    }
    for text in [
        "please mobile-use now",
        "mobile-useful feature",
        "/mobile-useful",
    ] {
        assert!(
            !explicit_mobile_use_intent(text),
            "must not activate: {text}"
        );
    }

    let root_store = MemoryStore::new();
    let root_session = SessionId::new("mobile-root-activation");
    let root = memory_envelope(
        &root_session,
        "mobile-root-message",
        "mobile-use read messages",
        None,
    );
    root_store
        .append(&mut [root])
        .await
        .expect("append root activation");
    assert!(
        durable_session_tool_state(&root_store, &root_session)
            .await
            .expect("root durable state")
            .mobile_use_active
    );

    let agent_store = MemoryStore::new();
    let agent_session = SessionId::new("mobile-agent-nonactivation");
    let agent = memory_envelope(
        &agent_session,
        "mobile-agent-message",
        "/mobile-use read messages",
        Some(AgentId::new("child-agent")),
    );
    agent_store
        .append(&mut [agent])
        .await
        .expect("append agent message");
    assert!(
        !durable_session_tool_state(&agent_store, &agent_session)
            .await
            .expect("agent durable state")
            .mobile_use_active
    );

    let prose_store = MemoryStore::new();
    let prose_session = SessionId::new("mobile-mid-message-nonactivation");
    let prose = memory_envelope(
        &prose_session,
        "mobile-prose-message",
        "please mobile-use now",
        None,
    );
    prose_store
        .append(&mut [prose])
        .await
        .expect("append prose message");
    assert!(
        !durable_session_tool_state(&prose_store, &prose_session)
            .await
            .expect("prose durable state")
            .mobile_use_active
    );
}

struct MobileDispatcherFixture {
    _profile: tempfile::TempDir,
    _workspace: tempfile::TempDir,
    store: SqliteStoreHandle,
    hub: SessionHub,
    session_id: SessionId,
    run_id: RunId,
    dispatcher: Arc<dyn ToolDispatcher>,
}

async fn mobile_dispatcher_fixture(
    label: &str,
    user_text: &str,
    backend: Arc<dyn MobileBackend>,
) -> MobileDispatcherFixture {
    let profile = tempfile::tempdir().expect("profile");
    let workspace = tempfile::tempdir().expect("workspace");
    let cwd = std::fs::canonicalize(workspace.path())
        .expect("canonical workspace")
        .to_string_lossy()
        .into_owned();
    let store = SqliteStoreHandle::open(profile.path())
        .await
        .expect("store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let session_id = SessionId::new(format!("{label}-session"));
    let run_id = RunId::new(format!("{label}-run"));
    let overrides = Some(SessionPermissionOverridesV1 {
        allow_writes: false,
        allow_exec: false,
        auto_allow: true,
    });
    hub.create_internal_session(SessionCreateCommand {
        command_id: format!("create-{label}"),
        request_digest: format!("create-{label}-digest"),
        request_json: format!(r#"{{"session":"{label}"}}"#),
        session_id: session_id.clone(),
        cwd: cwd.clone(),
        provider: "fake".into(),
        model: "fake-model".into(),
        max_tokens: 4096,
        permission_overrides: overrides,
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        system_prompt_version: SystemPromptBuilder::VERSION.into(),
        event_id: EventId::new(format!("{label}-created")),
        device_id: DeviceId::new(format!("{label}-device")),
    })
    .await
    .expect("create mobile session");
    hub.accept_internal_turn(TurnAcceptCommand {
        command_id: format!("submit-{label}"),
        request_digest: format!("submit-{label}-digest"),
        request_json: format!(r#"{{"turn":"{label}"}}"#),
        session_id: session_id.clone(),
        worker_generation: store.worker_generation(),
        run_id: run_id.clone(),
        agent_id: None,
        branch_id: None,
        text: user_text.into(),
        attachments: Vec::new(),
        mode: DeliveryMode::Queue,
        queued_event_id: EventId::new(format!("{label}-queued")),
        user_event_id: EventId::new(format!("{label}-user")),
        active_event_id: EventId::new(format!("{label}-active")),
        device_id: DeviceId::new(format!("{label}-device")),
    })
    .await
    .expect("accept mobile tool run");
    let lease = hub
        .acquire_worker_lease(session_id.clone())
        .await
        .expect("mobile tool lease");
    let mobile_use_active = durable_session_tool_state(&lease, &session_id)
        .await
        .expect("mobile activation snapshot")
        .mobile_use_active;
    let factory = BrokerToolFactory::with_mobile_backend(backend);
    let dispatcher = TurnToolFactory::create(
        &factory,
        WorkerToolContext {
            diagnostics: None,
            metadata: SessionMetadataV1 {
                cwd,
                provider: "fake".into(),
                model: "fake-model".into(),
                max_tokens: 4096,
                system_prompt_version: Some(SystemPromptBuilder::VERSION.into()),
                permission_overrides: overrides,
                title: None,
                effort: None,
                fast: false,
                cache_policy: Default::default(),
                created_at_ms: 1,
                agent_type: None,
            },
            store: lease,
            run_id: run_id.clone(),
            branch_id: None,
            device_id: DeviceId::new(format!("{label}-tool-device")),
            event_ids: Arc::new(EventIdGenerator::new(format!("{label}-tool-event"))),
            delegation: crate::delegation::DelegationHandle::new(hub.clone()),
            tasks: crate::tasks::TaskFacade::new(hub.clone()),
            agent_id: None,
            grant: None,
            mobile_use_active,
            cli_scope: None,
            web_search: None,
        },
    )
    .await
    .expect("create mobile dispatcher")
    .expect("mobile dispatcher available");
    MobileDispatcherFixture {
        _profile: profile,
        _workspace: workspace,
        store,
        hub,
        session_id,
        run_id,
        dispatcher,
    }
}

async fn close_fixture(fixture: MobileDispatcherFixture) {
    fixture
        .dispatcher
        .close()
        .await
        .expect("close mobile dispatcher");
    fixture.hub.shutdown().await.expect("shutdown hub");
    fixture.store.close().await.expect("close store");
}

/// MUTATION PIN: disabling the explicit runtime fence must reach the backend
/// because this fixture independently auto-allows ordinary effect policy.
#[tokio::test]
async fn mobile_dispatch_fence_rejects_inactive_without_backend() {
    let backend = Arc::new(FakeMobileBackend::new(canned_sms()));
    let fixture = mobile_dispatcher_fixture(
        "mobile-inactive-fence",
        "read my messages",
        Arc::clone(&backend) as Arc<dyn MobileBackend>,
    )
    .await;
    let outcome = fixture
        .dispatcher
        .execute(
            &fixture.run_id,
            &ItemId::new("mobile-inactive-item"),
            "mobile-inactive-call",
            "mobile",
            serde_json::json!({"action": "sms_read", "limit": 2}),
            &CancelToken::new(),
        )
        .await
        .expect("inactive dispatch returns a typed refusal");
    let ToolDispatchResult::Completed(result) = outcome else {
        panic!("inactive mobile dispatch must complete with a refusal");
    };
    assert_eq!(backend.calls.load(Ordering::Acquire), 0);
    assert_eq!(result.status, ToolResultStatus::Rejected);
    let preview: serde_json::Value =
        serde_json::from_str(&result.preview).expect("refusal preview JSON");
    assert_eq!(preview["error"]["kind"], "capability_denied");
    assert_eq!(
        result
            .presentation
            .as_ref()
            .expect("typed presentation")
            .subcode
            .as_str(),
        "capability-denied"
    );
    let events = fixture
        .store
        .read(&fixture.session_id, 0, 512)
        .await
        .expect("inactive journal");
    assert!(!events.into_iter().any(|event| {
        serde_json::from_value::<EventPayload>(event.payload).is_ok_and(|payload| {
            matches!(
                payload,
                EventPayload::Effect(EffectPhase::Intent(EffectIntent {
                    class: EffectClass::ReadSms,
                    ..
                }))
            )
        })
    }));
    close_fixture(fixture).await;
}

#[tokio::test]
async fn mobile_sms_read_fast_path_serializes_json_without_images() {
    let expected = canned_sms();
    let backend = Arc::new(FakeMobileBackend::new(expected.clone()));
    let fixture = mobile_dispatcher_fixture(
        "mobile-active-fast-path",
        "mobile-use read my messages",
        Arc::clone(&backend) as Arc<dyn MobileBackend>,
    )
    .await;
    let outcome = fixture
        .dispatcher
        .execute(
            &fixture.run_id,
            &ItemId::new("mobile-active-item"),
            "mobile-active-call",
            "mobile",
            serde_json::json!({
                "action": "sms_read",
                "folder": "inbox",
                "limit": 2
            }),
            &CancelToken::new(),
        )
        .await
        .expect("active mobile dispatch");
    let ToolDispatchResult::Completed(result) = outcome else {
        panic!("active mobile dispatch must complete");
    };
    assert_eq!(result.status, ToolResultStatus::Completed);
    assert_eq!(
        result.preview,
        serde_json::to_string(&expected).expect("expected mobile output serializes")
    );
    assert!(result.images.is_empty());
    assert!(result.artifact.is_none());
    assert!(result.cursor.is_none());
    assert_eq!(backend.calls.load(Ordering::Acquire), 1);
    assert_eq!(
        backend
            .actions
            .lock()
            .expect("mobile actions lock")
            .as_slice(),
        [MobileAction::SmsRead {
            folder: Some("inbox".into()),
            since: None,
            limit: Some(2),
        }]
    );

    let events = fixture
        .store
        .read(&fixture.session_id, 0, 512)
        .await
        .expect("active journal");
    let phases = events
        .into_iter()
        .filter_map(|event| serde_json::from_value::<EventPayload>(event.payload).ok())
        .filter_map(|payload| match payload {
            EventPayload::Effect(phase) => Some(phase),
            _ => None,
        })
        .collect::<Vec<_>>();
    let effect = phases.iter().find_map(|phase| match phase {
        EffectPhase::Intent(intent) if intent.class == EffectClass::ReadSms => {
            Some(intent.effect.clone())
        }
        _ => None,
    });
    let effect = effect.expect("ReadSms intent");
    assert!(phases.iter().any(|phase| matches!(
        phase,
        EffectPhase::Dispatched { effect: dispatched } if dispatched == &effect
    )));
    assert!(phases.iter().any(|phase| matches!(
        phase,
        EffectPhase::Outcome {
            effect: completed,
            outcome: EffectOutcome::Ok,
            ..
        } if completed == &effect
    )));
    close_fixture(fixture).await;
}
