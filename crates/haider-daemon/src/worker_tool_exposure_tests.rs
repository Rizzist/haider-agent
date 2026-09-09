#![allow(clippy::expect_used)]

use super::*;
use haider_protocol::item::ToolStatus;

fn configured_factory(names: Option<Vec<String>>) -> ConfiguredToolExposureFactory {
    ConfiguredToolExposureFactory {
        inner: Arc::new(BrokerToolFactory),
        names,
        profile: None,
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
    // The standard lockdown policy permits bounded children. A narrower
    // allowlist must still remove delegation despite its default exposure.
    assert!(actual_names.contains("spawn_subagent"));
    let without_spawn = allowed
        .into_iter()
        .filter(|name| name != "spawn_subagent")
        .collect::<Vec<_>>();
    let narrowed = lockdown_tool_definition_pack(
        registered_tool_catalog().provider_definition_pack.clone(),
        Some(&without_spawn),
    );
    assert!(
        narrowed
            .definitions
            .iter()
            .all(|tool| tool.name != "spawn_subagent")
    );
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
            "task_outcome",
            "fs_read",
            "fs_glob",
            "fs_search",
            "fs_write",
            "fs_edit",
            "process_exec",
            "spawn_subagent",
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
fn default_delegation_exposure_preserves_tool_and_effect_grant_ceilings() {
    let factory: Arc<dyn TurnToolFactory> = Arc::new(configured_factory(Some(Vec::new())));
    for grant in [
        Grant {
            tools: vec!["fs_read".into()],
            effect_ceiling: vec![EffectClass::FsRead, EffectClass::AgentSpawn],
        },
        Grant {
            tools: vec!["fs_read".into(), "spawn_subagent".into()],
            effect_ceiling: vec![EffectClass::FsRead],
        },
    ] {
        let mut config =
            HarnessConfig::for_session(SessionId::new("scope"), DeviceId::new("scope"), 0, 1);
        config.tools = advertised_tool_definitions(
            &factory,
            Some(&grant),
            "fake",
            WebCapabilityDegrade::default(),
        );
        config.enable_tool_discovery(vec!["spawn_subagent".into()]);
        assert_eq!(
            config
                .tool_definitions()
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            ["fs_read"]
        );
    }
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

fn capability_profile_config(
    profile: ToolCapabilityProfile,
    grant: Option<&Grant>,
) -> HarnessConfig {
    let factory: Arc<dyn TurnToolFactory> = Arc::new(ConfiguredToolExposureFactory {
        inner: Arc::new(BrokerToolFactory),
        names: Some(Vec::new()),
        profile: Some(profile),
    });
    let mut config =
        HarnessConfig::for_session(SessionId::new("profile"), DeviceId::new("profile"), 0, 1);
    config.tools =
        advertised_tool_definitions(&factory, grant, "fake", WebCapabilityDegrade::default());
    if let Some(promoted) =
        initial_tool_exposure_for_turn(factory.as_ref(), grant, false, Vec::new())
    {
        config.enable_tool_discovery_with_profile(factory.tool_capability_profile(), promoted);
    }
    config
}

#[test]
fn capability_profile_packs_match_golden_and_report_estimator_delta() {
    let profiles = [
        ("coding", ToolCapabilityProfile::Coding),
        ("inspection", ToolCapabilityProfile::Inspection),
        ("automation", ToolCapabilityProfile::Automation),
        ("discovery", ToolCapabilityProfile::Discovery),
    ];
    let mut packs = std::collections::BTreeMap::new();
    let system = Some(SystemPromptBuilder::shared_immutable_base(
        &[],
        "fixture-grant",
    ));
    for (name, profile) in profiles {
        assert_eq!(ToolCapabilityProfile::from_name(name), Some(profile));
        let config = capability_profile_config(profile, None);
        let estimate =
            estimate_provider_request_input_tokens(&[], &system, config.tool_definitions(), &[]);
        println!(
            "capability_profile={name} estimated_fixed_tokens={estimate} tools={}",
            config.tool_definitions().len()
        );
        packs.insert(name, config.tool_definitions().to_vec());
    }
    assert_eq!(ToolCapabilityProfile::from_name("unknown"), None);
    let actual = serde_json::to_string_pretty(&packs).expect("profile goldens") + "\n";
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/capability_profiles.json");
    if std::env::var_os("HAIDER_UPDATE_CAPABILITY_GOLDEN").is_some() {
        std::fs::write(&path, &actual).expect("write golden");
    }
    assert_eq!(
        actual,
        std::fs::read_to_string(path).expect("checked-in profile golden")
    );
    let coding = estimate_provider_request_input_tokens(&[], &system, &packs["coding"], &[]);
    let automation =
        estimate_provider_request_input_tokens(&[], &system, &packs["automation"], &[]);
    assert!(
        automation < coding,
        "fewer schema bytes must reduce the repository estimate"
    );
}

#[test]
fn capability_profiles_intersect_tool_and_effect_grants() {
    let grant = Grant {
        tools: vec![
            "list_tools".into(),
            "fs_read".into(),
            "process_exec".into(),
            "spawn_subagent".into(),
        ],
        effect_ceiling: vec![EffectClass::FsRead],
    };
    let config = capability_profile_config(ToolCapabilityProfile::Automation, Some(&grant));
    assert_eq!(
        config
            .tool_definitions()
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["list_tools", "fs_read"]
    );
    let grant = Grant {
        tools: vec!["plan".into()],
        effect_ceiling: Vec::new(),
    };
    let config = capability_profile_config(ToolCapabilityProfile::Discovery, Some(&grant));
    assert_eq!(
        config
            .tool_definitions()
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["plan"]
    );
}

#[test]
fn capability_profile_configuration_composes_with_explicit_exposure() {
    for profile_first in [false, true] {
        let dependencies = DaemonDependencies::default();
        let dependencies = if profile_first {
            dependencies
                .with_tool_capability_profile(ToolCapabilityProfile::Automation)
                .with_tool_exposure(Some(vec!["monitor".into()]))
        } else {
            dependencies
                .with_tool_exposure(Some(vec!["monitor".into()]))
                .with_tool_capability_profile(ToolCapabilityProfile::Automation)
        };
        assert_eq!(
            dependencies.tool_factory.tool_capability_profile(),
            ToolCapabilityProfile::Automation
        );
        assert_eq!(
            dependencies.tool_factory.initial_tool_exposure(),
            Some(vec!["monitor".into()])
        );
    }
    let dependencies = DaemonDependencies::default()
        .with_tool_exposure(None)
        .with_tool_capability_profile(ToolCapabilityProfile::Discovery);
    assert!(dependencies.tool_factory.initial_tool_exposure().is_none());
}

struct ProfileFixtureDispatcher;

#[async_trait]
impl ToolDispatcher for ProfileFixtureDispatcher {
    async fn execute(
        &self,
        _run_id: &RunId,
        _item_id: &ItemId,
        call_id: &str,
        name: &str,
        _args: serde_json::Value,
        _cancel: &haider_core::CancelToken,
    ) -> Result<ToolDispatchResult, HaiderError> {
        assert_eq!(name, "process_exec");
        Ok(ToolDispatchResult::Completed(BoundedResult {
            preview: format!(
                "NEEDLE:{call_id}:path=a/b;digest=0123456789;ordinal=17;Unicode=\u{062d}\u{0642}\n"
            ),
            truncated: false,
            truncation: None,
            effects: Vec::new(),
            data: None,
            artifact: None,
            images: Vec::new(),
            cursor: None,
            status: ToolResultStatus::Completed,
            reason: None,
            presentation: None,
        }))
    }
}

/// Replays the economy fixture's seven tool batches (17 results), followed
/// by its eighth/final request. The dispatcher is synthetic; the separate
/// CLI economy capture verifies real process and filesystem effects.
#[tokio::test]
async fn capability_profiles_keep_eight_request_economy_shape_and_committed_needles() {
    use haider_provider::{FakeProvider, FakeStep};
    let mut previous_messages = None;
    for profile in [
        ToolCapabilityProfile::Coding,
        ToolCapabilityProfile::Automation,
    ] {
        let mut steps = Vec::new();
        let mut expected_results = std::collections::BTreeMap::new();
        for (batch, count) in [5, 5, 3, 1, 1, 1, 1].into_iter().enumerate() {
            for index in 0..count {
                let call_id = format!("economy-{batch}-{index}");
                expected_results.insert(call_id.clone(), format!("NEEDLE:{call_id}:path=a/b;digest=0123456789;ordinal=17;Unicode=\u{062d}\u{0642}\n"));
                steps.push(FakeStep::EmitToolCall {
                    call_id,
                    name: "process_exec".into(),
                    args: serde_json::json!({"command": "fixture"}),
                });
            }
            steps.push(FakeStep::Finish {
                reason: FinishReason::ToolUse,
            });
        }
        steps.push(FakeStep::EmitText {
            text: "done".into(),
        });
        steps.push(FakeStep::Finish {
            reason: FinishReason::EndTurn,
        });
        let provider = Arc::new(FakeProvider::new(steps));
        let store = Arc::new(haider_core::MemoryStore::new());
        let (actor, handle) = HarnessActor::new_with_dispatcher(
            capability_profile_config(profile, None),
            provider.clone(),
            store.clone(),
            Some(Arc::new(ProfileFixtureDispatcher)),
        );
        let task = tokio::spawn(actor.run());
        let turn = handle
            .submit_turn(haider_core::SubmitTurn::new("run the economy fixture"))
            .await
            .expect("submit");
        assert_eq!(turn.wait().await.expect("terminal").state, RunState::Done);
        let requests = provider.requests();
        assert_eq!(
            requests.len(),
            8,
            "profiles must not add discovery/repair requests"
        );
        let final_results = requests[7]
            .messages
            .iter()
            .flat_map(|message| &message.blocks)
            .filter_map(|block| match block {
                haider_protocol::provider::Block::ToolResult {
                    call_id, preview, ..
                } => Some((call_id.clone(), preview.clone())),
                _ => None,
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(
            final_results, expected_results,
            "every committed result survives byte-exactly in tool role"
        );
        let committed = store
            .events(&SessionId::new("profile"))
            .await
            .into_iter()
            .filter_map(|event| match event.payload.decode_event() {
                Ok(EventPayload::ToolResult { call_id, result }) => Some((call_id, result.preview)),
                _ => None,
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(committed, expected_results);
        let messages = requests
            .iter()
            .map(|request| request.messages.clone())
            .collect::<Vec<_>>();
        if let Some(previous) = &previous_messages {
            assert_eq!(
                &messages, previous,
                "only schemas may change across profiles"
            );
        }
        previous_messages = Some(messages);
        handle.stop().await.expect("stop actor");
        task.await.expect("join actor");
    }
}
