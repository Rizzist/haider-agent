#![allow(clippy::expect_used)]

use super::*;
use haider_protocol::item::ToolStatus;

fn configured_factory(names: Option<Vec<String>>) -> ConfiguredToolExposureFactory {
    ConfiguredToolExposureFactory {
        inner: Arc::new(BrokerToolFactory),
        names,
    }
}

fn discovery_start(call_id: &str) -> EventPayload {
    EventPayload::Item(ItemEvent::Started {
        item_id: ItemId::new(format!("item-{call_id}")),
        item: TurnItem::ToolCall {
            call_id: call_id.into(),
            name: "list_tools".into(),
            args: serde_json::json!({"filter": "monitor"}),
            status: ToolStatus::InProgress,
        },
    })
}

fn discovery_result(call_id: &str, status: ToolResultStatus) -> EventPayload {
    EventPayload::ToolResult {
        call_id: call_id.into(),
        result: BoundedResult {
            preview: "described monitor".into(),
            truncated: false,
            truncation: None,
            effects: Vec::new(),
            data: Some(haider_protocol::tool::ToolResultData::ToolsDiscovered {
                promoted: vec!["monitor".into()],
            }),
            artifact: None,
            images: Vec::new(),
            cursor: None,
            status,
            reason: None,
            presentation: None,
        },
    }
}

fn envelope(seq: u64, payload: EventPayload) -> RawEnvelope {
    RawEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(format!("exposure-{seq}")),
        seq,
        session_id: SessionId::new("exposure-session"),
        branch_id: None,
        run_id: Some(RunId::new("exposure-run")),
        agent_id: None,
        device_id: DeviceId::new("exposure-device"),
        authority_epoch: 0,
        worker_generation: 1,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: seq,
        render: RenderTargets {
            ui: false,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: serde_json::to_value(payload).expect("payload").into(),
    }
}

#[test]
fn tier_configuration_preserves_explicit_grants_and_lockdown_allowlists() {
    assert_eq!(configured_tool_exposure(None), Some(Vec::new()));
    assert_eq!(
        configured_tool_exposure(Some("monitor, ssh_list")),
        Some(vec!["monitor".into(), "ssh_list".into()])
    );
    assert_eq!(configured_tool_exposure(Some("all")), None);
    let factory = configured_factory(Some(Vec::new()));
    assert_eq!(
        initial_tool_exposure_for_turn(&factory, None, false, vec!["monitor".into()]),
        Some(vec!["monitor".into()])
    );
    let grant = Grant {
        tools: vec!["plan".into()],
        effect_ceiling: Vec::new(),
    };
    assert!(
        initial_tool_exposure_for_turn(&factory, Some(&grant), false, vec!["monitor".into()])
            .is_none()
    );
    assert!(initial_tool_exposure_for_turn(&factory, None, true, vec!["monitor".into()]).is_none());
    let full = registered_tool_catalog().provider_definition_pack.clone();
    let allowed = crate::lockdown::allowed_tool_names();
    let actual = lockdown_tool_definition_pack(full, Some(&allowed));
    let actual_names = actual
        .definitions
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(actual_names, allowed.iter().map(String::as_str).collect());
    assert!(!actual_names.contains("list_tools"));
    assert!(!actual_names.contains("monitor"));
}

#[test]
fn production_coding_surface_and_explicit_names_remain_authorized() {
    let factory: Arc<dyn TurnToolFactory> = Arc::new(configured_factory(Some(vec![
        "monitor".into(),
        "mobile".into(),
    ])));
    let full = advertised_tool_definitions(&factory, None, "fake", WebCapabilityDegrade::default());
    let mut config =
        HarnessConfig::for_session(SessionId::new("tier"), DeviceId::new("tier"), 0, 1);
    config.tools = full;
    config.enable_tool_discovery(
        initial_tool_exposure_for_turn(factory.as_ref(), None, false, Vec::new())
            .expect("coding tier"),
    );
    let names = config
        .tool_definitions()
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        [
            "list_tools",
            "todo_write",
            "fs_read",
            "fs_glob",
            "fs_search",
            "fs_write",
            "fs_edit",
            "process_exec",
            "monitor"
        ]
    );
    assert!(
        !names.contains(&"mobile"),
        "configuration cannot activate the mobile capability"
    );
    assert!(!names.contains(&"computer"));
    let explicit = DaemonDependencies::default().with_tool_exposure(None);
    assert!(explicit.tool_factory.initial_tool_exposure().is_none());
}

#[test]
fn durable_discovery_requires_correlated_success_and_resets_at_new_consent_boundary() {
    let mut reduction = DurableToolStateReduction::default();
    reduction.observe(
        None,
        &discovery_result("forged", ToolResultStatus::Completed),
    );
    reduction.observe(None, &discovery_start("failed"));
    reduction.observe(
        None,
        &discovery_result("failed", ToolResultStatus::Rejected),
    );
    assert!(reduction.snapshot().promoted_tools.is_empty());
    reduction.observe(None, &discovery_start("accepted"));
    reduction.observe(
        None,
        &discovery_result("accepted", ToolResultStatus::Completed),
    );
    assert_eq!(reduction.snapshot().promoted_tools, ["monitor"]);
    reduction.reset_at_session_fork();
    assert!(reduction.snapshot().promoted_tools.is_empty());
    reduction.observe(
        None,
        &discovery_result("accepted", ToolResultStatus::Completed),
    );
    assert!(reduction.snapshot().promoted_tools.is_empty());
    reduction.observe(
        None,
        &EventPayload::UserMessage {
            text: "/computer-use".into(),
            attachments: Vec::new(),
            mode: haider_protocol::DeliveryMode::default(),
        },
    );
    assert_eq!(reduction.snapshot().promoted_tools, ["computer"]);
    reduction.reset_at_workspace_selection();
    assert!(reduction.snapshot().promoted_tools.is_empty());
}

#[tokio::test]
async fn discovery_survives_cold_journal_and_turn_setup_reduction() {
    let store = haider_core::MemoryStore::new();
    let mut events = [
        envelope(1, discovery_start("discover")),
        envelope(2, discovery_result("discover", ToolResultStatus::Completed)),
    ];
    StoreHandle::append(&store, &mut events)
        .await
        .expect("commit receipt");
    for _ in 0..2 {
        let state = durable_session_tool_state(&store, &SessionId::new("exposure-session"))
            .await
            .expect("cold journal reduction");
        assert_eq!(state.promoted_tools, ["monitor"]);
    }
    assert!(
        TURN_SETUP_REDUCTION_PAYLOAD_KINDS.contains(&"tool_result"),
        "SQL pushdown must retain discovery receipts"
    );
    let selector = TurnSetupReductionSelector {
        run_id: RunId::new("next-run"),
        branch_id: None,
        agent_id: None,
        provider: "fake".into(),
        model: "fake".into(),
        account_scope: None,
        auth_scope: "none".into(),
    };
    let mut reduction = TurnSetupReduction::new(selector);
    for event in events {
        reduction.observe_envelope(event).expect("setup reduction");
    }
    assert_eq!(reduction.durable_tool_state().promoted_tools, ["monitor"]);
}

#[test]
fn workspace_selection_preserves_discovery_and_clears_permission_state() {
    let mut reduction = DurableToolStateReduction::default();
    reduction.observe(None, &discovery_start("discovered"));
    reduction.observe(
        None,
        &discovery_result("discovered", ToolResultStatus::Completed),
    );
    reduction.observe(None, &discovery_start("in-flight"));
    reduction.grants.push(SessionGrant::for_effect(
        EffectClass::FsWrite,
        "old-workspace",
    ));
    reduction.bindings.insert(
        MenuId::new("old-menu"),
        (EffectClass::FsWrite, "old-intent".into()),
    );
    reduction.explicit_computer_intent = true;
    reduction.mobile_use_active = true;
    reduction.reset_at_workspace_selection();
    let state = reduction.snapshot();
    assert_eq!(state.promoted_tools, ["monitor"]);
    assert!(state.grants.is_empty());
    assert!(state.bindings.is_empty());
    assert!(!state.mobile_use_active);
    assert!(!reduction.explicit_computer_intent);
    assert!(reduction.discovery_calls.is_empty());
    reduction.reset_at_session_fork();
    assert!(reduction.snapshot().promoted_tools.is_empty());
}

#[tokio::test]
async fn cold_journal_workspace_selection_preserves_discovery_but_revokes_capability_consent() {
    let store = haider_core::MemoryStore::new();
    let mut selected = envelope(4, discovery_start("unused"));
    selected.payload = haider_protocol::workspace::WorkspaceEventPayload::WorkspaceSelected(
        haider_protocol::workspace::WorkspaceSelected {
            path: "/new-workspace".into(),
            previous_path: Some("/old-workspace".into()),
        },
    )
    .to_payload_value()
    .expect("workspace event")
    .into();
    let mut events = [
        envelope(1, discovery_start("discovered")),
        envelope(
            2,
            discovery_result("discovered", ToolResultStatus::Completed),
        ),
        envelope(
            3,
            EventPayload::UserMessage {
                text: "/mobile-use".into(),
                attachments: Vec::new(),
                mode: haider_protocol::DeliveryMode::default(),
            },
        ),
        selected,
    ];
    StoreHandle::append(&store, &mut events)
        .await
        .expect("commit workspace selection");
    let state = durable_session_tool_state(&store, &SessionId::new("exposure-session"))
        .await
        .expect("cold reduction");
    assert_eq!(state.promoted_tools, ["monitor"]);
    assert!(!state.mobile_use_active);
    let mut setup = TurnSetupReduction::new(TurnSetupReductionSelector {
        run_id: RunId::new("next-run"),
        branch_id: None,
        agent_id: None,
        provider: "fake".into(),
        model: "fake".into(),
        account_scope: None,
        auth_scope: "none".into(),
    });
    for event in events {
        setup.observe_envelope(event).expect("turn setup reduction");
    }
    assert_eq!(
        setup.durable_tool_state().promoted_tools,
        state.promoted_tools
    );
    assert!(!setup.durable_tool_state().mobile_use_active);
}
