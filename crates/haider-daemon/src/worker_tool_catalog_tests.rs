#![allow(clippy::expect_used)]

use super::*;

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
    let reduced = lockdown_tool_definition_pack(full, true);
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

/// MUTATION CHECK: reuse a mutable provider-trust lookup from inside the
/// dispatcher instead of the pack/context snapshot. Expected failure: the
/// first pack changes when the simulated toggle constructs the next turn.
#[test]
fn trust_toggle_changes_only_the_next_turn_pack() {
    let full = registered_tool_catalog().provider_definition_pack.clone();
    let in_flight = lockdown_tool_definition_pack(full.clone(), true);
    let next_turn = lockdown_tool_definition_pack(full, false);

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
    let sandbox = Path::new("/lockdown/research");
    assert_eq!(
        lockdown_write_relative(sandbox, Path::new("notes/result.txt")),
        Ok(Path::new("notes/result.txt"))
    );
    assert_eq!(
        lockdown_write_relative(sandbox, Path::new("/lockdown/research/result.txt")),
        Ok(Path::new("result.txt"))
    );
    assert_eq!(
        lockdown_write_relative(sandbox, Path::new("/workspace/src/lib.rs")),
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
