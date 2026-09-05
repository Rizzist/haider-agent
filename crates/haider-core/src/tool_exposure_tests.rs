#![allow(clippy::expect_used)]

use super::*;

fn definition(name: &str) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        description: format!("Use {name}"),
        input_schema: serde_json::json!({"type": "object"}),
    }
}

fn config() -> HarnessConfig {
    let mut config = HarnessConfig::for_session(
        SessionId::new("discovery-session"),
        DeviceId::new("discovery-device"),
        1,
        1,
    );
    config.system_prompt = Some("stable policy".into());
    config.tools = [
        "list_tools",
        "todo_write",
        "fs_read",
        "fs_glob",
        "fs_search",
        "fs_write",
        "fs_edit",
        "process_exec",
        "computer",
        "monitor",
        "plan",
        "ssh_list",
        "web_fetch",
    ]
    .into_iter()
    .map(definition)
    .collect();
    config.enable_tool_discovery(Vec::new());
    config
}

fn names(config: &HarnessConfig) -> Vec<&str> {
    config
        .tool_definitions()
        .iter()
        .map(|tool| tool.name.as_str())
        .collect()
}

#[test]
fn default_coding_surface_and_catalog_read_do_not_promote() {
    let config = config();
    assert_eq!(
        names(&config),
        [
            "list_tools",
            "todo_write",
            "fs_read",
            "fs_glob",
            "fs_search",
            "fs_write",
            "fs_edit",
            "process_exec",
        ]
    );
    let before = config.canonical_tool_pack_digest();
    let listing = config.discovered_tool_result(serde_json::json!({}));
    let payload: serde_json::Value = serde_json::from_str(&listing.preview).expect("catalog JSON");
    assert!(
        payload["tools"]
            .as_array()
            .expect("names")
            .contains(&serde_json::json!("monitor"))
    );
    assert!(listing.data.is_none());
    assert_eq!(before, config.canonical_tool_pack_digest());
    assert!(!tool_call_within_advertised_ceiling(&config, "monitor"));
    assert!(tool_call_within_advertised_ceiling(&config, "exec"));
}

#[test]
fn discovery_promotes_once_in_catalog_order_and_keeps_policy_byte_stable() {
    let mut first = config();
    let mut second = config();
    let initial = usage_prefix_digests(&first, &[]);
    for name in ["plan", "monitor", "plan"] {
        let result = first.discovered_tool_result(serde_json::json!({"filter": name}));
        assert!(!names(&first).contains(&"computer"));
        first.promote_committed_tools(&result);
    }
    for name in ["monitor", "plan"] {
        let result = second.discovered_tool_result(serde_json::json!({"filter": name}));
        second.promote_committed_tools(&result);
    }
    assert_eq!(first.tool_definitions(), second.tool_definitions());
    assert_eq!(
        first.canonical_tool_pack_digest(),
        second.canonical_tool_pack_digest()
    );
    assert_eq!(first.system_prompt, second.system_prompt);
    let promoted = usage_prefix_digests(&first, &[]);
    let key = CacheDiagnosticKey::from_bytes([0x71; 32]);
    let old = cache_breakpoint_hashes(&key, &initial);
    let new = cache_breakpoint_hashes(&key, &promoted);
    assert_eq!(old.system, new.system);
    assert_ne!(old.tools, new.tools);
    assert_ne!(old.history, new.history);
    assert!(tool_call_within_advertised_ceiling(&first, "monitor"));
    let stable = first.canonical_tool_pack_digest();
    let repeat = first.discovered_tool_result(serde_json::json!({"filter": "monitor"}));
    first.promote_committed_tools(&repeat);
    assert_eq!(stable, first.canonical_tool_pack_digest());
}

#[test]
fn discovery_cannot_promote_outside_catalog_or_from_rejected_results() {
    let mut config = config();
    let original = names(&config)
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    for args in [
        serde_json::json!({"filter": "mobile"}),
        serde_json::json!({"filter": 7}),
        serde_json::json!({"filter": "x".repeat(129)}),
    ] {
        let result = config.discovered_tool_result(args);
        config.promote_committed_tools(&result);
    }
    assert_eq!(names(&config), original);
    let mut rejected = config.discovered_tool_result(serde_json::json!({"filter": "monitor"}));
    rejected.status = ToolResultStatus::Rejected;
    config.promote_committed_tools(&rejected);
    assert_eq!(names(&config), original);
}

#[test]
fn broad_discovery_promotes_only_described_rows_with_honest_truncation() {
    let mut config = config();
    let result = config.discovered_tool_result(serde_json::json!({"filter": "use"}));
    assert!(result.truncated);
    let truncation = result.truncation.as_ref().expect("typed truncation");
    assert!(truncation.original_bytes > truncation.payload_bytes);
    assert!(result.preview.contains("[haider:truncated "));
    let Some(haider_protocol::tool::ToolResultData::ToolsDiscovered { promoted }) = &result.data
    else {
        panic!("typed discovery receipt");
    };
    assert_eq!(promoted.len(), DISCOVERY_ROW_CAP);
    let page: serde_json::Value =
        serde_json::from_str(result.payload_text()).expect("bounded page");
    assert_eq!(
        page["tools"].as_array().expect("described rows").len(),
        promoted.len()
    );
    config.promote_committed_tools(&result);
    assert!(
        !names(&config).contains(&"monitor"),
        "an omitted row must remain hidden"
    );
}

#[test]
fn provider_refresh_and_fallback_preserve_the_discovery_tier() {
    let mut config = config();
    let full = config
        .tool_exposure
        .as_ref()
        .expect("exposure")
        .current
        .clone();
    let state = ProviderDerivedRequestState {
        tool_result_images_supported: false,
        local_web_tool_names: Vec::new(),
        provider_fallback_local_web_tool_names: vec!["web_fetch".into()],
    };
    let current: Arc<[ToolDefinition]> = full
        .iter()
        .filter(|tool| tool.name != "web_fetch")
        .cloned()
        .collect::<Vec<_>>()
        .into();
    config.install_shared_tool_packs(
        SharedToolPacks {
            base: full.clone(),
            local_web_tool_names: vec!["web_fetch".into()].into(),
            current_digest: canonical_tool_definitions_digest(&current),
            current,
            fallback: Some((full.clone(), canonical_tool_definitions_digest(&full))),
            variants: Arc::default(),
        },
        &state,
    );
    let result = config.discovered_tool_result(serde_json::json!({"filter": "monitor"}));
    config.promote_committed_tools(&result);
    config.install_provider_derived_request_state(&state);
    assert!(names(&config).contains(&"monitor"));
    assert!(!names(&config).contains(&"computer"));
    config.activate_provider_tool_fallback();
    let result = config.discovered_tool_result(serde_json::json!({"filter": "web_fetch"}));
    config.promote_committed_tools(&result);
    assert!(names(&config).contains(&"web_fetch"));
    assert!(names(&config).contains(&"monitor"));
    assert!(!names(&config).contains(&"computer"));
}

/// VERIFIER F3: the standalone owned tools vector becomes a shared filtered
/// view on enablement. A provider refresh without a base must not discard
/// the full catalog when it resets that shared view.
#[test]
fn owned_catalog_survives_provider_refresh_without_a_provider_base() {
    let mut config = config();
    let initial = config.shared_tool_definitions();
    let digest = config.canonical_tool_pack_digest();
    let state = ProviderDerivedRequestState::default();
    config.install_provider_derived_request_state(&state);
    assert_eq!(config.tool_definitions(), initial.as_ref());
    assert_eq!(config.canonical_tool_pack_digest(), digest);
    let result = config.discovered_tool_result(serde_json::json!({"filter": "monitor"}));
    config.promote_committed_tools(&result);
    assert!(names(&config).contains(&"monitor"));
    let promoted = config.shared_tool_definitions();
    config.install_provider_derived_request_state(&state);
    assert_eq!(config.tool_definitions(), promoted.as_ref());
    assert!(!names(&config).contains(&"computer"));
}

/// VERIFIER F3: public owned provider bases derive owned fallback vectors;
/// exposure must retain those before installing its shared filtered view.
#[test]
fn owned_provider_base_retains_fallback_across_enablement_and_refresh() {
    let mut config = HarnessConfig::for_session(
        SessionId::new("owned-fallback"),
        DeviceId::new("owned-fallback"),
        0,
        1,
    );
    config.provider_tool_base = Some(
        ["list_tools", "fs_read", "monitor", "web_fetch"]
            .into_iter()
            .map(definition)
            .collect(),
    );
    config.provider_local_web_tools = vec![definition("web_fetch")];
    let state = ProviderDerivedRequestState {
        tool_result_images_supported: false,
        local_web_tool_names: Vec::new(),
        provider_fallback_local_web_tool_names: vec!["web_fetch".into()],
    };
    config.install_provider_derived_request_state(&state);
    assert!(!config.provider_tool_fallback_tools.is_empty());
    config.enable_tool_discovery(Vec::new());
    assert!(config.has_provider_tool_fallback());
    let result = config.discovered_tool_result(serde_json::json!({"filter": "monitor"}));
    config.promote_committed_tools(&result);
    config.install_provider_derived_request_state(&state);
    assert!(config.has_provider_tool_fallback());
    assert!(names(&config).contains(&"monitor"));
    config.activate_provider_tool_fallback();
    let result = config.discovered_tool_result(serde_json::json!({"filter": "web_fetch"}));
    config.promote_committed_tools(&result);
    assert!(names(&config).contains(&"web_fetch"));
    assert!(names(&config).contains(&"monitor"));
}

#[tokio::test]
async fn discovery_is_committed_before_the_next_request_advertises_it() {
    use haider_provider::{FakeProvider, FakeStep};
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitToolCall {
            call_id: "discover".into(),
            name: "list_tools".into(),
            args: serde_json::json!({"filter": "monitor"}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "discover".into(),
        },
        FakeStep::EmitText {
            text: "done".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
        FakeStep::EmitText {
            text: "next turn".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let store = Arc::new(crate::MemoryStore::new());
    let handle = HarnessActor::spawn(config(), provider.clone(), store.clone());
    for text in ["discover monitor", "continue"] {
        let turn = handle
            .submit_turn(crate::SubmitTurn::new(text))
            .await
            .expect("submit");
        assert_eq!(turn.wait().await.expect("terminal").state, RunState::Done);
    }
    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    assert!(!requests[0].tools.iter().any(|tool| tool.name == "monitor"));
    assert!(requests[1].tools.iter().any(|tool| tool.name == "monitor"));
    assert_eq!(requests[1].tools, requests[2].tools);
    assert_eq!(requests[0].system_prompt, requests[2].system_prompt);
    let events = store.events(&SessionId::new("discovery-session")).await;
    assert!(events.iter().any(|event| matches!(event.payload.decode_event(), Ok(EventPayload::ToolResult { result: BoundedResult { data: Some(haider_protocol::tool::ToolResultData::ToolsDiscovered { promoted }), .. }, .. }) if promoted == ["monitor"])));
}
