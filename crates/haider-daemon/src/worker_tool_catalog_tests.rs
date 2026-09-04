#![allow(clippy::expect_used)]

use super::*;
use crate::model_select::SelectionRefusal;

#[tokio::test]
async fn unavailable_workspace_tool_call_is_a_typed_rejection_without_an_effect() {
    let dispatcher = WorkspaceUnavailableToolDispatcher {
        unavailable: WorkspaceUnavailable {
            path: "/gone".into(),
            reason: haider_protocol::workspace::WorkspaceUnavailableReason::Missing,
            detail: "not found".into(),
        },
    };
    let result = dispatcher
        .execute(
            &RunId::new("workspace-unavailable-run"),
            &ItemId::new("workspace-unavailable-item"),
            "workspace-unavailable-call",
            "read",
            serde_json::json!({"path":"README.md"}),
            &CancelToken::new(),
        )
        .await
        .expect("workspace refusal is a completed tool result");
    let ToolDispatchResult::Completed(result) = result else {
        panic!("workspace refusal must not park")
    };
    assert_eq!(result.status, ToolResultStatus::Rejected);
    assert_eq!(
        result
            .presentation
            .as_ref()
            .map(|presentation| presentation.subcode.as_str()),
        Some(ErrorCode::WorkspaceUnavailable.as_subcode())
    );
    let preview: serde_json::Value = serde_json::from_str(&result.preview).expect("typed preview");
    assert_eq!(
        preview
            .pointer("/error/kind")
            .and_then(serde_json::Value::as_str),
        Some("workspace_unavailable")
    );
    assert_eq!(
        preview
            .pointer("/error/path")
            .and_then(serde_json::Value::as_str),
        Some("/gone")
    );
}

struct PeerPermissionJournal;

#[async_trait::async_trait]
impl JournalSink for PeerPermissionJournal {
    async fn append(&mut self, _payload: EventPayload) -> ToolResult<()> {
        Ok(())
    }
}

#[test]
fn tool_catalog_and_provider_schemas_have_process_wide_identity() {
    let first_catalog = registered_tool_catalog();
    let second_catalog = registered_tool_catalog();
    assert!(std::ptr::eq(first_catalog, second_catalog));
    let first_tools = registered_tools();
    let second_tools = registered_tools();
    assert_eq!(first_tools.as_ptr(), second_tools.as_ptr());

    let first_definitions = TurnToolFactory::shared_definitions(&BrokerToolFactory);
    let second_definitions = TurnToolFactory::shared_definitions(&BrokerToolFactory);
    assert!(Arc::ptr_eq(&first_definitions, &second_definitions));
    assert_eq!(first_definitions.as_ptr(), second_definitions.as_ptr());
}

#[test]
fn request_input_provider_schema_matches_pinned_wire_bytes() {
    const GOLDEN: &[u8] = br#"{"name":"request_input","description":"","input_schema":{"properties":{"body":{"items":{"type":"string"},"type":"array"},"default":{"type":"string"},"kind":{"enum":["question","choice"],"type":"string"},"options":{"items":{"properties":{"detail":{"type":"string"},"key":{"type":"string"},"label":{"type":"string"}},"required":["key","label"],"type":"object"},"type":"array"},"title":{"type":"string"}},"required":["kind","title"],"type":"object"}}"#;
    let definition = registered_provider_definitions()
        .first()
        .cloned()
        .expect("request_input provider definition");
    assert_eq!(definition.name, "request_input");
    assert_eq!(serde_json::to_vec(&definition).expect("wire bytes"), GOLDEN);
}

#[test]
fn shared_filtered_tool_view_preserves_legacy_wire_bytes() {
    let factory: Arc<dyn TurnToolFactory> = Arc::new(BrokerToolFactory);
    let grant = default_child_grant();
    let mut expected = build_registered_tools()
        .iter()
        .map(|entry| provider_definition(&entry.manifest))
        .collect::<Vec<_>>();
    expected.retain(|definition| definition.name != "mobile");
    expected.retain(|definition| {
        grant.tools.contains(&definition.name)
            && registered_tool_by_name(&definition.name).is_some_and(|entry| {
                grant_admits_tool_manifest(&grant, &entry.manifest.name, &entry.manifest.effects)
            })
    });
    let (local_web_tool_names, _) =
        provider_web_tool_names("fake", WebCapabilityDegrade::default());
    expected.retain(|definition| {
        !is_local_web_tool(&definition.name) || local_web_tool_names.contains(&definition.name)
    });

    let actual = advertised_tool_definitions(
        &factory,
        Some(&grant),
        "fake",
        WebCapabilityDegrade::default(),
    );
    let first_pack = advertised_tool_pack_for_mobile_state(
        &factory,
        Some(&grant),
        "fake",
        WebCapabilityDegrade::default(),
        false,
    );
    let second_pack = advertised_tool_pack_for_mobile_state(
        &factory,
        Some(&grant),
        "fake",
        WebCapabilityDegrade::default(),
        false,
    );
    assert!(Arc::ptr_eq(&first_pack, &second_pack));
    assert!(Arc::ptr_eq(
        &first_pack.definitions,
        &second_pack.definitions
    ));
    assert_eq!(
        first_pack.digest,
        canonical_tool_definitions_digest(&expected)
    );
    assert_eq!(
        serde_json::to_vec(&actual).expect("actual wire bytes"),
        serde_json::to_vec(&expected).expect("legacy wire bytes")
    );
}

/// MUTATION CHECK: bypass the lockdown pack filter, or accidentally admit one
/// of the named escape routes. Expected failure: the exact names diverge or a
/// forbidden tool survives.
#[test]
fn lockdown_turn_advertises_only_the_fixed_reduced_pack() {
    let full = registered_tool_catalog().provider_definition_pack.clone();
    let allowed = crate::lockdown::allowed_tool_names();
    let reduced = lockdown_tool_definition_pack(full, Some(&allowed));
    let names = reduced
        .definitions
        .iter()
        .map(|definition| definition.name.as_str())
        .collect::<Vec<_>>();
    assert!(!names.is_empty());
    assert!(names.iter().all(|name| crate::lockdown::tool_allowed(name)));
    for required in [
        "fs_read",
        "fs_glob",
        "fs_search",
        "fs_write",
        "web_fetch",
        "web_search",
        "todo_write",
        "plan",
        "spawn_subagent",
        "list_models",
        "peer_list",
        "ssh_list",
    ] {
        assert!(
            names.contains(&required),
            "missing lockdown tool {required}"
        );
    }
    for forbidden in [
        "process_exec",
        "task_output",
        "task_kill",
        "fs_edit",
        "fs_path",
        "peer_send",
        "ssh_shell",
        "monitor",
        "computer",
        "mobile",
        "loom_register",
        "graph_evidence",
    ] {
        assert!(
            !names.contains(&forbidden),
            "lockdown advertised forbidden tool {forbidden}"
        );
    }
    assert!(
        LOCKDOWN_HARD_DENIED_EFFECTS.contains(&EffectClass::RemoteExecution),
        "ssh_shell must remain below the lockdown hard ceiling, not ordinary Ask/Allow"
    );
    assert!(
        LOCKDOWN_HARD_DENIED_EFFECTS.contains(&EffectClass::PeerMessage),
        "peer_send must remain below the lockdown hard ceiling, not ordinary Ask/Allow"
    );
}

/// AUTO-HERMETIC: reuse the lockdown pack machinery with the stricter local
/// no-auth envelope. No web/gateway-adjacent route may be advertised even
/// though ordinary configured lockdown deliberately permits web tools.
#[test]
fn auto_hermetic_turn_advertises_no_egress_tools() {
    let full = registered_tool_catalog().provider_definition_pack.clone();
    let allowed =
        crate::auto_hermetic::tools_for(crate::auto_hermetic::ProviderLockdownPolicy::AutoHermetic);
    let reduced = lockdown_tool_definition_pack(full, Some(&allowed));
    let names = reduced
        .definitions
        .iter()
        .map(|definition| definition.name.as_str())
        .collect::<Vec<_>>();
    assert!(names.contains(&"fs_read"));
    for egress in [
        "web_search",
        "web_fetch",
        "peer_list",
        "ssh_list",
        "spawn_subagent",
        "list_models",
        "process_exec",
    ] {
        assert!(!names.contains(&egress), "advertised egress tool {egress}");
    }
}

/// MUTATION CHECK: reuse a mutable provider-trust lookup from inside the
/// dispatcher instead of the pack/context snapshot. Expected failure: the
/// first pack changes when the simulated toggle constructs the next turn.
#[test]
fn trust_toggle_changes_only_the_next_turn_pack() {
    let full = registered_tool_catalog().provider_definition_pack.clone();
    let allowed = crate::lockdown::allowed_tool_names();
    let in_flight = lockdown_tool_definition_pack(full.clone(), Some(&allowed));
    let next_turn = lockdown_tool_definition_pack(full, None);

    assert!(
        !in_flight
            .definitions
            .iter()
            .any(|definition| definition.name == "process_exec")
    );
    assert!(
        next_turn
            .definitions
            .iter()
            .any(|definition| definition.name == "process_exec")
    );
    assert!(
        !in_flight
            .definitions
            .iter()
            .any(|definition| definition.name == "process_exec"),
        "constructing the next turn must not mutate the in-flight pack"
    );
}

#[test]
fn lockdown_write_path_ceiling_runs_before_any_user_policy() {
    let fixture = tempfile::tempdir().expect("temporary lockdown path fixture");
    let root = std::fs::canonicalize(fixture.path()).expect("canonical lockdown path fixture");
    let sandbox = root.join("lockdown/research");
    let inside = sandbox.join("result.txt");
    let outside = root.join("workspace/src/lib.rs");
    assert_eq!(
        lockdown_write_relative(&sandbox, Path::new("notes/result.txt")),
        Ok(Path::new("notes/result.txt"))
    );
    assert_eq!(
        lockdown_write_relative(&sandbox, &inside),
        Ok(Path::new("result.txt"))
    );
    assert_eq!(lockdown_write_relative(&sandbox, &outside), Err(()));
    assert_eq!(lockdown_write_relative(&sandbox, Path::new("")), Err(()));
    assert_eq!(
        lockdown_write_relative(&sandbox, Path::new("./notes/result.txt")),
        Ok(Path::new("./notes/result.txt"))
    );
    assert_eq!(
        lockdown_write_relative(&sandbox, Path::new("../escape")),
        Err(())
    );
    let inside_with_parent = sandbox.join("../escape");
    assert_eq!(
        lockdown_write_relative(&sandbox, &inside_with_parent),
        Err(())
    );
    let rooted = Path::new(std::path::MAIN_SEPARATOR_STR).join("outside");
    assert_eq!(lockdown_write_relative(&sandbox, &rooted), Err(()));
    #[cfg(windows)]
    assert_eq!(
        lockdown_write_relative(&sandbox, Path::new(r"C:escape")),
        Err(())
    );
}

/// MUTATION CHECK: invert either side of the provider-scoped child ceiling.
/// Expected failure: Full→Lockdown research becomes unavailable or a
/// Lockdown parent can escape through a Full child.
#[test]
fn lockdown_subagent_provider_rule_has_one_way_flow() {
    assert!(lockdown_child_provider_allowed(false, false));
    assert!(lockdown_child_provider_allowed(false, true));
    assert!(lockdown_child_provider_allowed(true, true));
    assert!(!lockdown_child_provider_allowed(true, false));
}

/// A provider-pair fallback reuses the already-created dispatcher and tool
/// pack. Crossing a trust boundary mid-turn would therefore either advertise
/// Full tools to a locked provider or widen a locked turn.
///
/// MUTATION CHECK: remove either lockdown operand. Expected failure: one
/// direction of the cross-provider escape becomes allowed.
#[test]
fn automatic_pair_switches_cannot_cross_a_lockdown_boundary() {
    let switch = haider_core::ProviderPairSwitch {
        run_id: haider_protocol::ids::RunId::new("lockdown-switch-run"),
        switch_ordinal: 0,
        from_provider: "source".to_owned(),
        from_model: "source-model".to_owned(),
        to_provider: "target".to_owned(),
        to_model: "target-model".to_owned(),
        cause: haider_core::ProviderPairSwitchCause::FallbackChain,
    };
    assert!(!lockdown_pair_switch_allowed(&switch, true, false));
    assert!(!lockdown_pair_switch_allowed(&switch, false, true));
    assert!(!lockdown_pair_switch_allowed(&switch, true, true));
    assert!(lockdown_pair_switch_allowed(&switch, false, false));

    let same_provider = haider_core::ProviderPairSwitch {
        to_provider: switch.from_provider.clone(),
        ..switch
    };
    assert!(lockdown_pair_switch_allowed(&same_provider, true, true));
}

/// MUTATION CHECK: expose the raw refusal as assistant prose or omit the
/// provider/allowed-pack adaptation hint. Expected failure: the compact model
/// message ceases to be the fixed sentence while the typed detail diverges.
#[test]
fn lockdown_refusal_is_typed_and_compacted_for_the_model() {
    let lockdown = crate::lockdown::LockdownTurn {
        provider: "research".into(),
        sandbox: PathBuf::from("/lockdown/research"),
        tools_allowed: vec!["fs_read".into(), "web_search".into()],
    };
    let result = lockdown_refusal_result(
        &lockdown,
        "peer_send",
        "peer messaging is outside the fixed envelope",
    );
    let preview: serde_json::Value =
        serde_json::from_str(&result.preview).expect("typed refusal preview");
    assert_eq!(
        preview["message"],
        "provider research is in lockdown mode; available tools: fs_read, web_search"
    );
    assert_eq!(preview["details"]["tool"], "peer_send");
    assert_eq!(
        preview["details"]["typed_error"],
        "RefusedByLockdown { tool: peer_send, reason: peer messaging is outside the fixed envelope }"
    );
    assert_eq!(result.status, ToolResultStatus::Rejected);
}

/// The selector refusal is committed inside the durable tool-result preview;
/// suggestions therefore belong to the typed payload before journal append,
/// not to a live-only presentation wrapper.
#[test]
fn spawn_model_refusal_preview_schema_includes_bounded_suggestions() {
    let result = selection_rejection_result(&SelectionRefusal::ModelNotResolvable {
        model: "glm4.7 flash".into(),
        candidates: Vec::new(),
        suggestions: vec![
            "glm-4.7-flashx · haider-code".into(),
            "glm-4.7-flash · local".into(),
        ],
    });
    let preview: serde_json::Value =
        serde_json::from_str(&result.preview).expect("typed selector refusal preview");
    assert_eq!(result.status, ToolResultStatus::Rejected);
    assert_eq!(preview["status"], "rejected");
    assert_eq!(preview["error"]["kind"], "model_not_resolvable");
    assert_eq!(
        preview["error"]["details"],
        serde_json::json!({
            "kind": "model_not_resolvable",
            "model": "glm4.7 flash",
            "candidates": [],
            "suggestions": [
                "glm-4.7-flashx · haider-code",
                "glm-4.7-flash · local",
            ],
        })
    );
    assert!(
        preview["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("call list_models"))
    );
}

/// MUTATION CHECK: return `HaiderError::InvalidArgument` from the cached
/// parser or map it through `tool_error`. The exact model-authored fixture
/// then cannot produce this continuable rejected result.
#[test]
fn parser_missing_required_path_becomes_typed_rejected_tool_result() {
    let error = match BrokerToolDispatcher::parse_tool_operation(
        RegisteredToolRoute::FsRead,
        "missing-path",
        &serde_json::json!({"message": "..."}),
    ) {
        Ok(_) => panic!("fs_read without path must fail parser validation"),
        Err(error) => error,
    };
    assert_eq!(
        error,
        ToolError::InvalidArgument {
            message: "tool argument `path` must be a non-empty string".into(),
        }
    );

    let ToolDispatchResult::Completed(result) =
        model_tool_argument_failure(error).expect("model argument failure is continuable")
    else {
        panic!("model argument failure must settle as a completed tool result")
    };
    assert_eq!(result.status, ToolResultStatus::Rejected);
    let preview: serde_json::Value =
        serde_json::from_str(&result.preview).expect("typed invalid-argument preview");
    assert_eq!(preview["status"], "rejected");
    assert_eq!(preview["error"]["kind"], "invalid_argument");
    assert_eq!(
        preview["error"]["message"],
        "tool argument `path` must be a non-empty string"
    );
}

#[test]
fn approval_retry_cache_reuses_typed_operation_and_fences_full_call_identity() {
    let parses = std::cell::Cell::new(0_u32);
    let mut operations = HashMap::new();
    let key = ParsedToolOperationKey {
        run_id: RunId::new("run-shared"),
        item_id: ItemId::new("item-shared"),
        call_id: "call-shared".into(),
        route: RegisteredToolRoute::FsWrite,
    };
    let first = cache_parsed_operation(&mut operations, key.clone(), || {
        parses.set(parses.get() + 1);
        BrokerToolDispatcher::parse_tool_operation(
            key.route,
            &key.call_id,
            &serde_json::json!({"path": "one.txt", "content": "first"}),
        )
    })
    .expect("first typed parse");
    let second = cache_parsed_operation(&mut operations, key.clone(), || {
        parses.set(parses.get() + 1);
        BrokerToolDispatcher::parse_tool_operation(
            key.route,
            &key.call_id,
            &serde_json::json!({"path": "two.txt", "content": "must-not-reparse"}),
        )
    })
    .expect("approval retry cache");

    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(parses.get(), 1);
    let ParsedToolOperation::FsWrite(operation) = first.as_ref() else {
        panic!("cached route must retain its typed operation")
    };
    assert_eq!(operation.path, PathBuf::from("one.txt"));

    let distinct = ParsedToolOperationKey {
        item_id: ItemId::new("item-distinct"),
        ..key.clone()
    };
    let third = cache_parsed_operation(&mut operations, distinct, || {
        parses.set(parses.get() + 1);
        BrokerToolDispatcher::parse_tool_operation(
            key.route,
            &key.call_id,
            &serde_json::json!({"path": "three.txt", "content": "new-call-identity"}),
        )
    })
    .expect("distinct item identity parses independently");
    assert!(!Arc::ptr_eq(&first, &third));
    assert_eq!(parses.get(), 2);

    operations.remove(&key);
    assert!(!operations.contains_key(&key));
}

#[test]
fn parsed_operation_lease_retains_only_an_approval_retry() {
    let key = ParsedToolOperationKey {
        run_id: RunId::new("run-lease"),
        item_id: ItemId::new("item-lease"),
        call_id: "call-lease".into(),
        route: RegisteredToolRoute::FsWrite,
    };
    let operation = Arc::new(ParsedToolOperation::FsWrite(FsWrite::new(
        "lease.txt",
        "content",
    )));
    let operations = StdMutex::new(HashMap::from([(key.clone(), Arc::clone(&operation))]));

    {
        let _terminal = ParsedToolOperationLease::new(&operations, key.clone());
    }
    assert!(
        !operations
            .lock()
            .expect("operation cache")
            .contains_key(&key)
    );

    operations
        .lock()
        .expect("operation cache")
        .insert(key.clone(), operation);
    {
        let mut approval = ParsedToolOperationLease::new(&operations, key.clone());
        approval.retain_for_approval();
    }
    assert!(
        operations
            .lock()
            .expect("operation cache")
            .contains_key(&key)
    );
}

#[test]
fn filtered_tool_pack_cache_is_process_bounded() {
    let mut cache = FilteredToolPackCache {
        packs: HashMap::new(),
        insertion_order: VecDeque::new(),
    };
    for index in 0..=FILTERED_TOOL_PACK_CACHE_CAPACITY {
        cache.insert(
            vec![index],
            Arc::new(ToolDefinitionPack {
                definitions: Arc::<[ToolDefinition]>::from(Vec::new()),
                digest: index.to_string(),
            }),
        );
    }
    assert_eq!(cache.packs.len(), FILTERED_TOOL_PACK_CACHE_CAPACITY);
    assert!(cache.get(&[0]).is_none());
    assert!(cache.get(&[FILTERED_TOOL_PACK_CACHE_CAPACITY]).is_some());
}

fn cached_pack_for_test(
    cache: &StdMutex<TurnToolPackCache>,
    registry_revision: Arc<[ToolDefinition]>,
    grant: Option<&Grant>,
    provider_name: &str,
    lockdown: bool,
    mobile_use_active: bool,
) -> Arc<SharedToolPacks> {
    let (local_web_tool_names, provider_fallback_local_web_tool_names) =
        provider_web_tool_names(provider_name, WebCapabilityDegrade::default());
    let provider_request_state = ProviderDerivedRequestState {
        tool_result_images_supported: false,
        local_web_tool_names,
        provider_fallback_local_web_tool_names,
    };
    let lockdown_tools = lockdown.then(crate::lockdown::allowed_tool_names);
    cached_turn_tool_packs(
        cache,
        TurnToolPackInputs {
            provider_name,
            provider_request_state: &provider_request_state,
            grant: ToolPackGrantSnapshot::new(grant).expect("grant revision"),
            lockdown_tools: lockdown_tools.as_deref(),
            registry_revision,
            mobile_use_active,
        },
    )
}

/// SECURITY MUTATION CHECK: remove the provider revision from the key.
/// Expected failure: the Anthropic declaration reuses the generic provider's
/// local-web pack instead of rebuilding it.
#[test]
fn turn_tool_pack_cache_rebuilds_when_provider_revision_changes() {
    let cache = StdMutex::new(TurnToolPackCache::new());
    let registry = registered_provider_definitions();
    let initial = cached_pack_for_test(&cache, Arc::clone(&registry), None, "fake", false, false);
    let repeated = cached_pack_for_test(&cache, Arc::clone(&registry), None, "fake", false, false);
    let changed = cached_pack_for_test(
        &cache,
        registry,
        None,
        ANTHROPIC_PROVIDER_NAME,
        false,
        false,
    );
    assert!(Arc::ptr_eq(&initial, &repeated));
    assert!(!Arc::ptr_eq(&initial, &changed));
    assert_ne!(initial.current_digest, changed.current_digest);
}

/// SECURITY MUTATION CHECK: remove the normalized grant revision from the
/// key. Expected failure: a delegated child receives the prior root pack.
#[test]
fn turn_tool_pack_cache_rebuilds_when_grant_revision_changes() {
    let cache = StdMutex::new(TurnToolPackCache::new());
    let registry = registered_provider_definitions();
    let child_grant = default_child_grant();
    let initial = cached_pack_for_test(&cache, Arc::clone(&registry), None, "fake", false, false);
    let repeated = cached_pack_for_test(&cache, Arc::clone(&registry), None, "fake", false, false);
    let changed = cached_pack_for_test(&cache, registry, Some(&child_grant), "fake", false, false);
    assert!(Arc::ptr_eq(&initial, &repeated));
    assert!(!Arc::ptr_eq(&initial, &changed));
    assert_ne!(initial.current_digest, changed.current_digest);
}

/// SECURITY MUTATION CHECK: remove the frozen lockdown revision from the
/// key. Expected failure: a newly locked-down turn retains `process_exec`.
#[test]
fn turn_tool_pack_cache_rebuilds_when_lockdown_revision_changes() {
    let cache = StdMutex::new(TurnToolPackCache::new());
    let registry = registered_provider_definitions();
    let initial = cached_pack_for_test(&cache, Arc::clone(&registry), None, "fake", false, false);
    let repeated = cached_pack_for_test(&cache, Arc::clone(&registry), None, "fake", false, false);
    let changed = cached_pack_for_test(&cache, registry, None, "fake", true, false);
    assert!(Arc::ptr_eq(&initial, &repeated));
    assert!(!Arc::ptr_eq(&initial, &changed));
    assert_ne!(initial.current_digest, changed.current_digest);
    assert!(
        initial
            .current
            .iter()
            .any(|tool| tool.name == "process_exec")
    );
    assert!(
        !changed
            .current
            .iter()
            .any(|tool| tool.name == "process_exec")
    );
}

/// SECURITY MUTATION CHECK: remove the immutable registry revision from the
/// key. Expected failure: a replacement registry aliases an older pack even
/// when its current bytes happen to match.
#[test]
fn turn_tool_pack_cache_rebuilds_when_registry_revision_changes() {
    let cache = StdMutex::new(TurnToolPackCache::new());
    let registry = registered_provider_definitions();
    let replacement: Arc<[ToolDefinition]> = registry.as_ref().to_vec().into();
    let initial = cached_pack_for_test(&cache, Arc::clone(&registry), None, "fake", false, false);
    let repeated = cached_pack_for_test(&cache, registry, None, "fake", false, false);
    let changed = cached_pack_for_test(&cache, replacement, None, "fake", false, false);
    assert!(Arc::ptr_eq(&initial, &repeated));
    assert!(!Arc::ptr_eq(&initial, &changed));
    assert_eq!(initial.current_digest, changed.current_digest);
}

/// SECURITY MUTATION CHECK: remove the mobile-use mode revision from the
/// key. Expected failure: activation cannot add the mobile tool next turn.
#[test]
fn turn_tool_pack_cache_rebuilds_when_mode_revision_changes() {
    let cache = StdMutex::new(TurnToolPackCache::new());
    let registry = registered_provider_definitions();
    let initial = cached_pack_for_test(&cache, Arc::clone(&registry), None, "fake", false, false);
    let repeated = cached_pack_for_test(&cache, Arc::clone(&registry), None, "fake", false, false);
    let changed = cached_pack_for_test(&cache, registry, None, "fake", false, true);
    assert!(Arc::ptr_eq(&initial, &repeated));
    assert!(!Arc::ptr_eq(&initial, &changed));
    assert_ne!(initial.current_digest, changed.current_digest);
    assert!(!initial.current.iter().any(|tool| tool.name == "mobile"));
    assert!(changed.current.iter().any(|tool| tool.name == "mobile"));
}

#[test]
fn turn_tool_pack_cache_is_process_bounded() {
    let cache = StdMutex::new(TurnToolPackCache::new());
    let registry = registered_provider_definitions();
    for index in 0..=TURN_TOOL_PACK_CACHE_CAPACITY {
        let _ = cached_pack_for_test(
            &cache,
            Arc::clone(&registry),
            None,
            &format!("provider-{index}"),
            false,
            false,
        );
    }
    assert_eq!(
        cache.lock().expect("turn tool pack cache").packs.len(),
        TURN_TOOL_PACK_CACHE_CAPACITY
    );
}

/// MUTATION CHECK: the public peer schemas, routes, permission defaults, and
/// provider-pack digest must move together. Changing either manifest requires
/// an explicit update to the expected definitions below.
#[test]
fn peer_tool_surface_is_manifest_and_digest_pinned() {
    let list = registered_tool_by_name("peer_list").expect("peer_list manifest");
    assert_eq!(list.route, RegisteredToolRoute::PeerList);
    assert_eq!(list.default, ToolPermissionDefault::Allow);
    assert!(list.manifest.effects.is_empty());

    let send = registered_tool_by_name("peer_send").expect("peer_send manifest");
    assert_eq!(send.route, RegisteredToolRoute::PeerSend);
    assert_eq!(send.default, ToolPermissionDefault::Ask);
    assert_eq!(send.manifest.effects, [EffectClass::PeerMessage]);

    let actual = [
        provider_definition(&list.manifest),
        provider_definition(&send.manifest),
    ];
    let expected = [
        ToolDefinition {
            name: "peer_list".into(),
            description: String::new(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {"filter": {"type": "string"}}
            }),
        },
        ToolDefinition {
            name: "peer_send".into(),
            description: String::new(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "to": {"type": "string"},
                    "message": {"type": "string"},
                    "summary": {"type": "string"}
                },
                "required": ["to", "message"]
            }),
        },
    ];
    assert_eq!(actual, expected);
    assert_eq!(
        canonical_tool_definitions_digest(&actual),
        canonical_tool_definitions_digest(&expected)
    );
}

/// MUTATION CHECK: make catalog discovery effectful, drop the filter, or let
/// its daemon route drift away from the advertised manifest.
#[test]
fn list_models_surface_is_manifest_route_and_text_pinned() {
    let list = registered_tool_by_name("list_models").expect("list_models manifest");
    assert_eq!(list.route, RegisteredToolRoute::ListModels);
    assert_eq!(list.default, ToolPermissionDefault::Allow);
    assert_eq!(list.manifest.dispatch, DispatchMode::Await);
    assert!(list.manifest.effects.is_empty());
    assert_eq!(
        provider_definition(&list.manifest),
        ToolDefinition {
            name: "list_models".into(),
            description: String::new(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {"filter": {"type": "string"}}
            }),
        }
    );
    let manual = tool_manual_line("list_models").expect("list_models manual line");
    assert_eq!(
        manual,
        "list_models(filter?) — read the daemon's cached model/provider catalog; filter matches model, provider, or alias without a network refresh"
    );
    let spawn = tool_manual_line("spawn_subagent").expect("spawn_subagent manual line");
    assert_eq!(
        spawn,
        "spawn_subagent(task, prompt, model?, provider?, agent_type?, workflow?, workflow_trigger?, parent_slot?, workflow_author?) — delegate one bounded task to a depth-capped child; bare model matching ignores case, `-`, `_`, `.`, and whitespace, with literal exact slugs first; call list_models to inspect valid pairs; agent_type = a registered Loom specialist (its Job frames the child)"
    );
}

/// The manifest default is not merely descriptive: the broker must park a
/// peer send on an ordinary permission menu before any dispatch phase exists.
#[tokio::test]
async fn peer_send_ask_default_is_enforced_by_the_effect_broker() {
    let workspace = tempfile::tempdir().expect("workspace");
    let mut broker = EffectBroker::new_at(
        Box::new(PeerPermissionJournal),
        workspace.path(),
        SessionId::new("peer-permission-session"),
        1,
        1_700_000_000_000,
    )
    .expect("effect broker");
    let operation = PeerSendOperation {
        to: "reviewer".into(),
        message: "Review the boundary".into(),
        summary: Some("request boundary review".into()),
    };
    let intent = broker
        .normalize(&operation)
        .await
        .expect("normalize peer send");
    let mut policy = PermissionPolicy::default();
    policy.ask(EffectClass::PeerMessage);
    let AuthorizationVerdict::Ask { menu } = broker
        .authorize(&intent, &policy)
        .await
        .expect("authorize peer send")
    else {
        panic!("peer_send must require permission");
    };
    let menu = broker.permission_menu(&menu).expect("permission menu");
    assert!(menu.title.contains("send peer message to reviewer"));
    assert_eq!(
        broker
            .journal_snapshot()
            .iter()
            .filter(|phase| matches!(phase, EffectPhase::Dispatched { .. }))
            .count(),
        0
    );
}

/// MUTATION CHECK: SSH visibility, remote execution, their permission
/// defaults, and the provider schemas must move as one contract surface.
#[test]
fn ssh_tool_surface_is_manifest_and_digest_pinned() {
    let list = registered_tool_by_name("ssh_list").expect("ssh_list manifest");
    assert_eq!(list.route, RegisteredToolRoute::SshList);
    assert_eq!(list.default, ToolPermissionDefault::Allow);
    assert!(list.manifest.effects.is_empty());

    let shell = registered_tool_by_name("ssh_shell").expect("ssh_shell manifest");
    assert_eq!(shell.route, RegisteredToolRoute::SshShell);
    assert_eq!(shell.default, ToolPermissionDefault::Ask);
    assert_eq!(shell.manifest.effects, [EffectClass::RemoteExecution]);

    let process = registered_tool_by_name("process_exec").expect("process_exec manifest");
    assert!(
        process
            .manifest
            .effects
            .contains(&EffectClass::RemoteExecution),
        "the unified shell tool must declare remote authority for profile targets"
    );

    let actual = [
        provider_definition(&list.manifest),
        provider_definition(&shell.manifest),
    ];
    let expected = [
        ToolDefinition {
            name: "ssh_list".into(),
            description: String::new(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
        },
        ToolDefinition {
            name: "ssh_shell".into(),
            description: String::new(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "profile": {"type": "string"},
                    "command": {"type": "string"},
                    "cwd": {"type": "string"},
                    "timeout_s": {"type": "integer"}
                },
                "required": ["profile", "command"]
            }),
        },
    ];
    assert_eq!(actual, expected);
    assert_eq!(
        canonical_tool_definitions_digest(&actual),
        canonical_tool_definitions_digest(&expected)
    );
}

/// A model-authored remote command must stop at an Ask menu before a russh
/// channel can be opened. The approval copy explicitly names the machine as
/// remote so local-process authority cannot be mistaken for SSH authority.
#[tokio::test]
async fn ssh_shell_ask_default_is_enforced_by_the_effect_broker() {
    let workspace = tempfile::tempdir().expect("workspace");
    let mut broker = EffectBroker::new_at(
        Box::new(PeerPermissionJournal),
        workspace.path(),
        SessionId::new("ssh-permission-session"),
        1,
        1_700_000_000_000,
    )
    .expect("effect broker");
    let operation = SshShellOperation {
        profile: "prod".into(),
        command: "uname -a".into(),
        cwd: None,
        timeout_s: Some(30),
    };
    assert!(matches!(
        haider_tools::SessionGrant::for_effect(operation.effect_class(), "ssh-command-shape").scope,
        haider_tools::SessionGrantScope::CommandShape { .. }
    ));
    let intent = broker
        .normalize(&operation)
        .await
        .expect("normalize remote execution");
    let mut policy = PermissionPolicy::default();
    policy.ask(EffectClass::RemoteExecution);
    let AuthorizationVerdict::Ask { menu } = broker
        .authorize(&intent, &policy)
        .await
        .expect("authorize remote execution")
    else {
        panic!("ssh_shell must require permission");
    };
    let menu = broker.permission_menu(&menu).expect("permission menu");
    assert!(menu.title.contains("remote SSH machine prod"));
    assert!(
        operation
            .approval_preview()
            .iter()
            .any(|line| line.contains("Remote machine"))
    );
    assert_eq!(
        broker
            .journal_snapshot()
            .iter()
            .filter(|phase| matches!(phase, EffectPhase::Dispatched { .. }))
            .count(),
        0
    );
}
