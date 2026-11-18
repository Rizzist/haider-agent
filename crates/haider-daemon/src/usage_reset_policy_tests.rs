#![allow(clippy::expect_used)]
//! Agent-tool policy laws for `usage_reset_apply`: the session policy denial
//! (`usage_reset::policy_denial_reason`) is checked before any approval menu,
//! so a read-only or lockdown session can never park a menu whose approval
//! would spend a provider reset credit.

use crate::session_hub::{SessionHub, SessionHubConfig};
use crate::tasks::TaskFacade;
use crate::worker::{BrokerToolFactory, TurnToolFactory, WorkerToolContext};
use haider_core::{
    CancelToken, EventIdGenerator, SessionCreateCommand, SqliteStoreHandle, StoreHandle,
    ToolDispatchResult, TurnAcceptCommand,
};
use haider_protocol::DeliveryMode;
use haider_protocol::EventPayload;
use haider_protocol::credential::{AuthMethod, CredentialDescriptor, CredentialStatus};
use haider_protocol::ids::{CredentialAlias, DeviceId, EventId, ItemId, RunId, SessionId};
use haider_protocol::session::{SessionMetadataV1, SessionPermissionOverridesV1};
use std::sync::Arc;
use std::time::Duration;

const ACCOUNT: &str = "synthetic-openai";

#[derive(Clone, Copy, Debug)]
enum Case {
    /// Ordinary session: the approval menu opens (proves the harness reaches
    /// the menu, so a missing denial would be observable).
    Control,
    ReadOnly,
    /// Lockdown turn whose tool envelope is widened to admit the tool, which
    /// isolates the effect-class capability ceiling (hard deny) from the
    /// separate advertised-tool envelope.
    LockdownCeiling,
    /// Lockdown turn with the real fixed envelope: refused by the envelope.
    LockdownEnvelope,
}

fn metadata(cwd: &str, read_only: bool) -> SessionMetadataV1 {
    SessionMetadataV1 {
        selection_epoch: None,
        route_reset_epoch: None,
        budget_clamp_notice_epoch: None,
        resolved_route_alias: None,
        resolved_route_seen: false,
        launch_origin: None,
        workspace_allocation: None,
        provider_base_url: None,
        provider_rebind_id: None,
        cwd: cwd.to_owned(),
        provider: "fake".into(),
        account_alias: None,
        model: "fake-model".into(),
        resolved_model: None,
        model_catalog_key: None,
        max_tokens: 4096,
        max_tokens_source: None,
        system_prompt_version: Some(crate::worker::SystemPromptBuilder::VERSION.into()),
        permission_overrides: Some(SessionPermissionOverridesV1 {
            read_only,
            ..SessionPermissionOverridesV1::default()
        }),
        interaction_mode: Default::default(),
        title: None,
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        context_economy: Default::default(),
        created_at_ms: 1,
        agent_type: None,
    }
}

struct Outcome {
    preview: serde_json::Value,
    head_before: u64,
    head_after: u64,
    menu_opened: bool,
    lockdown_refused: bool,
}

async fn run_case(case: Case) -> Outcome {
    let label = format!("reset-policy-{case:?}").to_ascii_lowercase();
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
    hub.install_usage_report(Arc::new(crate::usage_report::UsageReportService::new(
        Arc::new(std::sync::Mutex::new(vec![CredentialDescriptor {
            credential_digest: None,
            alias: CredentialAlias::new(ACCOUNT),
            provider: haider_provider::OPENAI_OAUTH_PROVIDER_NAME.into(),
            base_url: None,
            auth_method: AuthMethod::OAuth,
            identity: "synthetic account".into(),
            status: CredentialStatus::Ok,
            active: true,
            label: None,
            account_identity: Some(haider_protocol::credential::AccountIdentity {
                subject_hash: Some("synthetic-principal".into()),
                email: Some("synthetic@example.test".into()),
                display_name: None,
                account_id: Some("synthetic-account".into()),
                plan: None,
                issuer: None,
                captured_at: 1,
                verified: false,
            }),
            incarnation: 1,
            created_at_ms: None,
        }])),
        None,
        Arc::new(crate::usage_report::ReqwestUsageMeterHttp::new()),
    )))
    .expect("install synthetic reset-capable account");

    let read_only = matches!(case, Case::ReadOnly);
    let session_id = SessionId::new(format!("{label}-session"));
    hub.create_internal_session(SessionCreateCommand {
        command_id: format!("create-{label}"),
        request_digest: format!("create-{label}-digest"),
        request_json: format!("{{\"session\":\"{label}\"}}"),
        session_id: session_id.clone(),
        cwd: cwd.clone(),
        provider: "fake".into(),
        model: "fake-model".into(),
        resolved_model: None,
        model_catalog_key: None,
        max_tokens: 4096,
        max_tokens_source: None,
        permission_overrides: metadata(&cwd, read_only).permission_overrides,
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        system_prompt_version: crate::worker::SystemPromptBuilder::VERSION.into(),
        event_id: EventId::new(format!("created-{label}")),
        device_id: DeviceId::new(format!("{label}-device")),
    })
    .await
    .expect("create session");
    let run_id = RunId::new(format!("{label}-run"));
    hub.accept_internal_turn(TurnAcceptCommand {
        command_id: format!("submit-{label}"),
        request_digest: format!("submit-{label}-digest"),
        request_json: format!(r#"{{"turn":"{label}"}}"#),
        session_id: session_id.clone(),
        worker_generation: hub.worker_generation(),
        run_id: run_id.clone(),
        agent_id: None,
        branch_id: None,
        text: "reset my usage".into(),
        attachments: Vec::new(),
        mode: DeliveryMode::Queue,
        queued_event_id: EventId::new(format!("queued-{label}")),
        user_event_id: EventId::new(format!("user-{label}")),
        active_event_id: EventId::new(format!("active-{label}")),
        device_id: DeviceId::new(format!("{label}-device")),
    })
    .await
    .expect("accept tool run");

    let lockdown = match case {
        Case::Control | Case::ReadOnly => None,
        Case::LockdownCeiling => Some(crate::lockdown::LockdownTurn {
            provider: "fake".into(),
            sandbox: workspace.path().to_path_buf(),
            tools_allowed: vec!["usage_reset_apply".into()],
        }),
        Case::LockdownEnvelope => Some(crate::lockdown::LockdownTurn {
            provider: "fake".into(),
            sandbox: workspace.path().to_path_buf(),
            tools_allowed: crate::lockdown::allowed_tool_names(),
        }),
    };
    let lease = hub
        .acquire_worker_lease(session_id.clone())
        .await
        .expect("tool lease");
    let dispatcher = TurnToolFactory::create(
        &BrokerToolFactory,
        WorkerToolContext {
            lockdown,
            diagnostics: None,
            metadata: metadata(&cwd, read_only),
            store: lease,
            run_id: run_id.clone(),
            turn_ordinal: 1,
            provider_request_ordinals: haider_provider::ProviderRequestOrdinal::new(0),
            turn_trace: None,
            run_deadline: None,
            branch_id: None,
            device_id: DeviceId::new(format!("{label}-tool-device")),
            event_ids: Arc::new(EventIdGenerator::new(format!("{label}-tool-event"))),
            delegation: crate::delegation::DelegationHandle::new(hub.clone()),
            tasks: TaskFacade::with_kill_grace(hub.clone(), Duration::from_millis(300)),
            agent_id: None,
            session_context_tail: String::new(),
            grant: None,
            mobile_use_active: false,
            cli_scope: None,
            typed_workflow_execution: None,
            loom_provider_fenced: false,
            orchestration_child_tools: Vec::new(),
            web_search: None,
        },
    )
    .await
    .expect("create dispatcher")
    .expect("dispatcher available");

    let head_before = store.latest_seq(&session_id).await.expect("head before");
    let result = dispatcher
        .execute(
            &run_id,
            &ItemId::new(format!("{label}-item")),
            "reset-call",
            "usage_reset_apply",
            serde_json::json!({ "account": ACCOUNT }),
            &CancelToken::new(),
        )
        .await
        .expect("tool dispatch");
    let ToolDispatchResult::Completed(result) = result else {
        panic!("usage_reset_apply returns a completed result");
    };
    let preview: serde_json::Value =
        serde_json::from_str(&result.preview).expect("typed tool result");
    let head_after = store.latest_seq(&session_id).await.expect("head after");
    let events = store.read(&session_id, 0, 1_024).await.expect("journal");
    let menu_opened = events.iter().any(|event| {
        matches!(
            event.payload.decode_event(),
            Ok(EventPayload::MenuOpened(_))
        )
    });
    let lockdown_refused = events.iter().any(|event| {
        matches!(
            event.payload.decode_event(),
            Ok(EventPayload::LockdownRefused(_))
        )
    });
    dispatcher.close().await.expect("dispatcher close");
    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
    Outcome {
        preview,
        head_before,
        head_after,
        menu_opened,
        lockdown_refused,
    }
}

fn assert_policy_denied(outcome: &Outcome, reason_fragment: &str) {
    assert_eq!(
        outcome.preview["error"]["kind"], "permission_denied",
        "typed policy denial: {}",
        outcome.preview
    );
    assert!(
        outcome.preview["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains(reason_fragment)),
        "denial names the policy rule: {}",
        outcome.preview
    );
    assert!(!outcome.menu_opened, "no approval menu may be parked");
    assert_eq!(
        outcome.head_after, outcome.head_before,
        "a denied reset leaves the journal untouched"
    );
}

/// Control: without a deny rule the same harness parks the human approval.
#[tokio::test]
async fn agent_usage_reset_control_session_parks_an_approval_menu() {
    let outcome = run_case(Case::Control).await;
    assert_eq!(outcome.preview["status"], "approval_pending");
    assert!(outcome.menu_opened);
    assert!(outcome.head_after > outcome.head_before);
}

/// LAW: a read-only session's agent cannot park a reset approval.
///
/// MUTATION CHECK: disable the tool's `policy_denial_reason` check
/// (`.filter(|_| false)`); the tool then returns `approval_pending`, opens a
/// `MenuOpened` and advances the journal head.
#[tokio::test]
async fn agent_usage_reset_read_only_session_is_denied_before_any_menu() {
    let outcome = run_case(Case::ReadOnly).await;
    assert_policy_denied(&outcome, crate::worker::READ_ONLY_USAGE_RESET_DENIAL);
}

/// LAW: the lockdown capability ceiling hard-denies the reset effect even if
/// the tool envelope admitted the tool, so no user approval can lift it.
///
/// MUTATION CHECK: disable the tool's `policy_denial_reason` check; the tool
/// then parks an approval menu inside a lockdown turn.
#[tokio::test]
async fn agent_usage_reset_lockdown_ceiling_is_denied_before_any_menu() {
    let outcome = run_case(Case::LockdownCeiling).await;
    assert_policy_denied(&outcome, "lockdown cannot be lifted by user approval");
    assert!(!outcome.lockdown_refused);
}

/// The real fixed lockdown envelope never dispatches the tool at all (the
/// outer layer); the refusal is typed and no menu opens.
#[tokio::test]
async fn agent_usage_reset_lockdown_envelope_refuses_the_tool() {
    let outcome = run_case(Case::LockdownEnvelope).await;
    assert!(outcome.lockdown_refused);
    assert!(!outcome.menu_opened);
    assert_ne!(outcome.preview["status"], "approval_pending");
}
