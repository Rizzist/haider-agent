#![allow(clippy::expect_used)]

use crate::turn_recovery::{RecoveredWork, recover_interrupted_turns};
use crate::worker::{
    BrokerToolFactory, PendingShellExec, RegisteredToolRoute, TurnToolFactory,
    WebCapabilityDegrade, advertised_tool_definitions, autonomous_permission_resolution_command,
    cli_scope_admits, defer_shell_handoff, durable_read_only_terminal_failure,
    durable_session_tool_state, effective_permission_defaults, explicit_computer_auto_grant_value,
    explicit_computer_use_intent, grant_admits_manifest_effect, loom_inventory_line, loom_run_tail,
    loom_task_type_id, plan_gate_admits, registered_tool_route, registered_tools,
    scoped_network_hosts, stub_schema, tool_inventory_snapshot, typed_child_grant,
    typed_tool_result, web_fetch_host_allowed,
};
use haider_core::{MemoryStore, RequestInputCheckpoint, SqliteStoreHandle, StoreHandle};
use haider_protocol::EventPayload;
use haider_protocol::effect::{
    AuthorizationVerdict, EffectClass, EffectIntent, EffectOutcome, EffectPhase, FileFreshness,
};
use haider_protocol::envelope::{
    EventEnvelope, PromptRender, RawEnvelope, RenderTargets, SCHEMA_VERSION,
};
use haider_protocol::ids::{
    BranchId, DeviceId, EffectId, EventId, ItemId, MenuId, RunId, SessionId,
};
use haider_protocol::item::{ItemEvent, ToolStatus, TurnItem};
use haider_protocol::menu::{
    AnswerVia, DecisionKind, Menu, MenuAnswer, MenuKind, MenuOption, MenuScope,
};
use haider_protocol::session::{
    SessionInteractionModeV1, SessionMetadataV1, SessionPermissionOverridesV1,
};
use haider_protocol::state::RunState;
use haider_protocol::tool::{DispatchMode, RememberedGrantScope, ToolPermissionDefault};
use haider_store::{AcceptedShellExec, EventStore, SessionCreateCommand, Store};
use haider_tools::{FsEditAnchorMismatch, ToolError};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;

#[test]
fn explicit_computer_intent_is_positive_bounded_and_negation_safe() {
    for text in [
        "computer-use my chrome and click Sign in",
        "/computer-use take a screenshot",
    ] {
        assert!(explicit_computer_use_intent(text), "must opt in: {text}");
    }
    for text in [
        "do not use my computer",
        "Never control my screen",
        "Explain what the computer-use tool does",
        "The documentation contains `computer-use my chrome` as an example",
        "Please use my computer to open Chrome",
        "Can you control my screen and close that dialog?",
        "computer vision is useful",
    ] {
        assert!(
            !explicit_computer_use_intent(text),
            "must not opt in: {text}"
        );
    }
}

#[test]
fn explicit_computer_auto_grant_has_a_documented_fail_closed_opt_out() {
    assert!(explicit_computer_auto_grant_value(None));
    assert!(explicit_computer_auto_grant_value(Some("yes")));
    for disabled in ["0", "false", "NO", " off "] {
        assert!(!explicit_computer_auto_grant_value(Some(disabled)));
    }
}

#[tokio::test]
async fn explicit_computer_command_reconstructs_only_session_screen_grants() {
    let store = MemoryStore::new();
    let session_id = SessionId::new("explicit-computer-session");
    let mut event = envelope(
        &session_id,
        "explicit-computer-user",
        EventPayload::UserMessage {
            text: "computer-use my chrome".into(),
            attachments: Vec::new(),
            mode: haider_protocol::DeliveryMode::Steer,
        },
    );
    event.run_id = Some(RunId::new("explicit-computer-run"));
    store.append(&mut [event]).await.expect("append opt-in");
    let state = durable_session_tool_state(&store, &session_id)
        .await
        .expect("reconstruct grants");
    assert_eq!(state.grants.len(), 2);
    assert!(state.grants.iter().any(|grant| {
        grant.class == EffectClass::ScreenObserve
            && grant.scope == haider_tools::SessionGrantScope::Class
    }));
    assert!(state.grants.iter().any(|grant| {
        grant.class == EffectClass::ScreenControl
            && grant.scope == haider_tools::SessionGrantScope::Class
    }));
}

/// MUTATION CHECK: advertise a name that has no typed registry route, or add
/// legacy `exec` to the manifests. Expected runtime failure: advertised and
/// dispatchable canonical sets differ, or the exact migration assertions fail.
#[test]
fn canonical_inventory_equals_advertised_dispatchable_set() {
    let definitions = TurnToolFactory::definitions(&BrokerToolFactory);
    let advertised = definitions
        .iter()
        .map(|definition| definition.name.as_str())
        .collect::<Vec<_>>();
    let registry = registered_tools();
    let registered = registry
        .iter()
        .map(|entry| entry.manifest.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(advertised, registered);
    assert!(advertised.contains(&"process_exec"));
    assert!(advertised.contains(&"message_subagent"));
    assert!(advertised.contains(&"computer"));
    assert!(advertised.contains(&"mobile"));
    assert!(advertised.contains(&"monitor"));
    assert!(advertised.contains(&"list_models"));
    assert!(advertised.contains(&"peer_list"));
    assert!(advertised.contains(&"peer_send"));
    assert!(!advertised.contains(&"exec"));
    assert!(
        advertised
            .iter()
            .all(|name| registered_tool_route(name).is_some())
    );
    assert_eq!(
        registered_tool_route("exec"),
        Some(RegisteredToolRoute::ProcessExec),
        "legacy history remains dispatchable without being advertised"
    );
    assert_eq!(
        registered_tool_route("fs_search"),
        Some(RegisteredToolRoute::FsSearch)
    );
    assert_eq!(
        registered_tool_route("fs_glob"),
        Some(RegisteredToolRoute::FsGlob)
    );
    assert_eq!(
        registered_tool_route("fs_edit"),
        Some(RegisteredToolRoute::FsEdit)
    );
    assert_eq!(
        registered_tool_route("fs_path"),
        Some(RegisteredToolRoute::FsPath)
    );
    assert_eq!(
        registered_tool_route("message_subagent"),
        Some(RegisteredToolRoute::MessageSubagent)
    );
    assert_eq!(
        registered_tool_route("computer"),
        Some(RegisteredToolRoute::Computer)
    );
    assert_eq!(
        registered_tool_route("mobile"),
        Some(RegisteredToolRoute::Mobile)
    );
    assert_eq!(
        registered_tool_route("monitor"),
        Some(RegisteredToolRoute::Monitor)
    );
    assert_eq!(
        registered_tool_route("list_models"),
        Some(RegisteredToolRoute::ListModels)
    );
    assert_eq!(
        registered_tool_route("peer_list"),
        Some(RegisteredToolRoute::PeerList)
    );
    assert_eq!(
        registered_tool_route("peer_send"),
        Some(RegisteredToolRoute::PeerSend)
    );
}

/// §E: monitor registry administration is effect-free and canonical. Grant
/// policy remains owned by the existing delegation layer.
#[test]
fn monitor_is_advertised_dispatchable_and_effect_free() {
    let registry = registered_tools();
    let entry = registry
        .iter()
        .find(|entry| entry.manifest.name == "monitor")
        .expect("monitor manifest");
    assert_eq!(entry.route, RegisteredToolRoute::Monitor);
    assert_eq!(entry.manifest.dispatch, DispatchMode::Await);
    assert!(entry.manifest.effects.is_empty());
    assert_eq!(entry.default, ToolPermissionDefault::NotApplicable);
}

/// MUTATION CHECK: drop any C1 registry entry or change its typed route.
/// Expected RUNTIME failure: the literal manifest name has no matching route.
#[test]
fn advertised_equals_dispatchable_for_all_consolidated_fs_tools() {
    let registry = registered_tools();
    for (name, route) in [
        ("fs_search", RegisteredToolRoute::FsSearch),
        ("fs_glob", RegisteredToolRoute::FsGlob),
        ("fs_edit", RegisteredToolRoute::FsEdit),
        ("fs_path", RegisteredToolRoute::FsPath),
    ] {
        assert!(
            registry.iter().any(|entry| entry.manifest.name == name),
            "{name} must be advertised"
        );
        assert_eq!(registered_tool_route(name), Some(route));
    }
}

/// CG-M1/M2e LAW: the testimony tool is canonical and effect-free. It remains
/// absent from the DEFAULT delegated grant; only workflow children receive it.
#[test]
fn graph_evidence_is_advertised_dispatchable_and_root_only() {
    let registry = registered_tools();
    let entry = registry
        .iter()
        .find(|entry| entry.manifest.name == "graph_evidence")
        .expect("graph_evidence manifest");
    assert_eq!(entry.route, RegisteredToolRoute::GraphEvidence);
    assert_eq!(
        entry.manifest.dispatch,
        haider_protocol::tool::DispatchMode::Await
    );
    assert!(entry.manifest.effects.is_empty());
    assert_eq!(entry.default, ToolPermissionDefault::NotApplicable);
    assert_eq!(
        registered_tool_route("graph_evidence"),
        Some(RegisteredToolRoute::GraphEvidence)
    );
    assert!(
        !crate::worker::default_child_grant()
            .tools
            .iter()
            .any(|name| name == "graph_evidence")
    );
}

/// CG-M2e LAW 4: the bare child pack remains the exact default. A workflow
/// grant deliberately surfaces graph testimony, and workflow authoring is
/// independently gated instead of leaking into root or plain-child packs.
/// MUTATION CHECK: add either capability to `default_child_grant`, or stop
/// filtering `workflow_author` from roots. Expected failure: a bare path gains
/// workflow machinery, or the gated pack cannot use its pinned child graph.
#[test]
fn workflow_capabilities_are_sparse_and_grant_scoped() {
    let factory: Arc<dyn TurnToolFactory> = Arc::new(BrokerToolFactory);
    let root = advertised_tool_definitions(&factory, None, "fake", WebCapabilityDegrade::default());
    let plain_grant = crate::worker::default_child_grant();
    let plain = advertised_tool_definitions(
        &factory,
        Some(&plain_grant),
        "fake",
        WebCapabilityDegrade::default(),
    );
    assert!(!root.iter().any(|tool| tool.name == "workflow_author"));
    assert!(!plain.iter().any(|tool| tool.name == "graph_evidence"));
    assert!(!plain.iter().any(|tool| tool.name == "workflow_author"));

    let mut workflow_grant = plain_grant;
    workflow_grant.tools.push("graph_evidence".into());
    let workflow = advertised_tool_definitions(
        &factory,
        Some(&workflow_grant),
        "fake",
        WebCapabilityDegrade::default(),
    );
    assert!(workflow.iter().any(|tool| tool.name == "graph_evidence"));
    assert!(!workflow.iter().any(|tool| tool.name == "workflow_author"));

    workflow_grant.tools.push("workflow_author".into());
    let authored = advertised_tool_definitions(
        &factory,
        Some(&workflow_grant),
        "fake",
        WebCapabilityDegrade::default(),
    );
    assert!(authored.iter().any(|tool| tool.name == "graph_evidence"));
    assert!(authored.iter().any(|tool| tool.name == "workflow_author"));
}

/// MUTATION CHECK: apply overrides before registry defaults, map exec to the
/// wrong class, or synthesize a user-typed preauthorization. Expected
/// RUNTIME failure: flagged classes are not ordinary policy `Allow`, or the
/// unflagged W8a defaults stop being `Ask`.
#[test]
fn session_permission_overrides_grant_only_their_named_effect_families() {
    let metadata = |permission_overrides| SessionMetadataV1 {
        provider_base_url: None,
        provider_rebind_id: None,
        cwd: "/tmp".into(),
        provider: "fake".into(),
        account_alias: None,
        model: "fake-model".into(),
        max_tokens: 4096,
        permission_overrides,
        interaction_mode: Default::default(),
        system_prompt_version: None,
        title: None,
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        context_economy: Default::default(),
        created_at_ms: 1,
        agent_type: None,
    };
    let decision = |metadata: &SessionMetadataV1, class: EffectClass| {
        effective_permission_defaults(metadata)
            .into_iter()
            .find_map(|(candidate, default)| (candidate == class).then_some(default))
            .expect("registered effect class")
    };

    let baseline = metadata(None);
    assert_eq!(
        decision(&baseline, EffectClass::FsWrite),
        ToolPermissionDefault::Ask
    );
    assert_eq!(
        decision(&baseline, EffectClass::ProcessExec),
        ToolPermissionDefault::Ask
    );
    assert_eq!(
        decision(&baseline, EffectClass::RemoteExecution),
        ToolPermissionDefault::Ask,
        "SSH remote execution must require approval by default"
    );
    assert_eq!(
        decision(&baseline, EffectClass::ScreenObserve),
        ToolPermissionDefault::Ask
    );
    assert_eq!(
        decision(&baseline, EffectClass::ScreenControl),
        ToolPermissionDefault::Ask
    );
    assert_eq!(
        decision(&baseline, EffectClass::PeerMessage),
        ToolPermissionDefault::Ask,
        "peer_send must not silently leave the permission broker"
    );
    assert_eq!(
        decision(&baseline, EffectClass::ReadSms),
        ToolPermissionDefault::Ask
    );
    assert_eq!(
        decision(&baseline, EffectClass::MobileObserve),
        ToolPermissionDefault::Ask
    );
    assert_eq!(
        decision(&baseline, EffectClass::MobileControl),
        ToolPermissionDefault::Ask
    );

    let writes = metadata(Some(SessionPermissionOverridesV1 {
        read_only: false,
        allow_writes: true,
        allow_exec: false,
        allow_mobile: false,
        auto_allow: false,
    }));
    assert_eq!(
        decision(&writes, EffectClass::FsWrite),
        ToolPermissionDefault::Allow
    );
    assert_eq!(
        decision(&writes, EffectClass::ProcessExec),
        ToolPermissionDefault::Ask
    );

    let exec = metadata(Some(SessionPermissionOverridesV1 {
        read_only: false,
        allow_writes: false,
        allow_exec: true,
        allow_mobile: false,
        auto_allow: false,
    }));
    assert_eq!(
        decision(&exec, EffectClass::FsWrite),
        ToolPermissionDefault::Ask
    );
    assert_eq!(
        decision(&exec, EffectClass::ProcessExec),
        ToolPermissionDefault::Allow
    );
    assert_eq!(
        decision(&exec, EffectClass::RemoteExecution),
        ToolPermissionDefault::Ask,
        "allow_exec grants local process authority, not remote execution"
    );
    assert_eq!(
        decision(&exec, EffectClass::ScreenObserve),
        ToolPermissionDefault::Ask,
        "allow_exec must never imply screen observation"
    );
    assert_eq!(
        decision(&exec, EffectClass::ScreenControl),
        ToolPermissionDefault::Ask,
        "allow_exec must never imply screen control"
    );
    assert_eq!(
        decision(&exec, EffectClass::ReadSms),
        ToolPermissionDefault::Ask,
        "allow_exec must never imply SMS access"
    );
    assert_eq!(
        decision(&exec, EffectClass::MobileObserve),
        ToolPermissionDefault::Ask,
        "allow_exec must never imply mobile observation"
    );
    assert_eq!(
        decision(&exec, EffectClass::MobileControl),
        ToolPermissionDefault::Ask,
        "allow_exec must never imply mobile control"
    );

    let mobile = metadata(Some(SessionPermissionOverridesV1 {
        read_only: false,
        allow_writes: false,
        allow_exec: false,
        allow_mobile: true,
        auto_allow: false,
    }));
    for class in [
        EffectClass::ReadSms,
        EffectClass::MobileObserve,
        EffectClass::MobileControl,
    ] {
        assert_eq!(decision(&mobile, class), ToolPermissionDefault::Allow);
    }
    for class in [
        EffectClass::FsWrite,
        EffectClass::ProcessExec,
        EffectClass::ScreenObserve,
        EffectClass::ScreenControl,
    ] {
        assert_eq!(
            decision(&mobile, class),
            ToolPermissionDefault::Ask,
            "allow_mobile must stay scoped to the Android transport"
        );
    }
}

/// Autonomous mode resolves every registry Ask to ordinary Allow. Explicit
/// deny rules are applied separately by the broker and still win.
#[test]
fn autonomous_effect_defaults_allow_every_ask_class() {
    let metadata = |permission_overrides| SessionMetadataV1 {
        provider_base_url: None,
        provider_rebind_id: None,
        cwd: "/tmp".into(),
        provider: "fake".into(),
        account_alias: None,
        model: "fake-model".into(),
        max_tokens: 4096,
        permission_overrides,
        interaction_mode: SessionInteractionModeV1::Autonomous,
        system_prompt_version: None,
        title: None,
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        context_economy: Default::default(),
        created_at_ms: 1,
        agent_type: None,
    };
    let decision = |metadata: &SessionMetadataV1, class: EffectClass| {
        effective_permission_defaults(metadata)
            .into_iter()
            .find_map(|(candidate, default)| (candidate == class).then_some(default))
            .expect("registered effect class")
    };

    let baseline = metadata(None);
    for (class, default) in effective_permission_defaults(&baseline) {
        assert_ne!(
            default,
            ToolPermissionDefault::Ask,
            "autonomous mode left {class:?} unresolved"
        );
        assert_ne!(
            default,
            ToolPermissionDefault::Deny,
            "a registry default denied {class:?} without an explicit user rule"
        );
    }
    for class in [
        EffectClass::FsWrite,
        EffectClass::ProcessExec,
        EffectClass::ScreenObserve,
        EffectClass::ScreenControl,
        EffectClass::ReadSms,
        EffectClass::MobileObserve,
        EffectClass::MobileControl,
    ] {
        assert_eq!(decision(&baseline, class), ToolPermissionDefault::Allow);
    }

    let explicit = metadata(Some(SessionPermissionOverridesV1 {
        read_only: false,
        allow_writes: true,
        allow_exec: true,
        allow_mobile: false,
        auto_allow: false,
    }));
    assert_eq!(
        decision(&explicit, EffectClass::FsWrite),
        ToolPermissionDefault::Allow
    );
    assert_eq!(
        decision(&explicit, EffectClass::ProcessExec),
        ToolPermissionDefault::Allow
    );
    assert_eq!(
        decision(&explicit, EffectClass::ScreenObserve),
        ToolPermissionDefault::Allow
    );

    let read_only = metadata(Some(SessionPermissionOverridesV1 {
        read_only: true,
        allow_writes: true,
        allow_exec: true,
        allow_mobile: false,
        auto_allow: true,
    }));
    assert_eq!(
        decision(&read_only, EffectClass::FsWrite),
        ToolPermissionDefault::Deny,
        "explicit read-only must win over every autonomous allow"
    );
    assert_eq!(
        decision(&read_only, EffectClass::ProcessExec),
        ToolPermissionDefault::Deny,
        "read-only must block shell commands that can write indirectly"
    );
    assert_eq!(
        decision(&read_only, EffectClass::RemoteExecution),
        ToolPermissionDefault::Deny,
        "read-only must block remote commands that can write indirectly"
    );
    assert_eq!(
        decision(&read_only, EffectClass::ScreenControl),
        ToolPermissionDefault::Deny,
        "read-only must block desktop control that can write indirectly"
    );
    assert_eq!(
        decision(&read_only, EffectClass::ScreenObserve),
        ToolPermissionDefault::Allow,
        "read-only still permits non-mutating observation"
    );
    assert_eq!(
        decision(&read_only, EffectClass::PeerMessage),
        ToolPermissionDefault::Deny,
        "read-only must not delegate mutation to an existing writable peer"
    );
    for class in [EffectClass::GitOp, EffectClass::GuiAct] {
        assert_eq!(
            decision(&read_only, class.clone()),
            ToolPermissionDefault::Deny,
            "read-only must install an explicit {class:?} deny even before a registry tool advertises it"
        );
    }
}

/// MUTATION CHECK: make `auto_allow` a no-op, scope it to only writes/exec, or
/// let it promote a `NotApplicable` class into an effect. Expected RUNTIME
/// failure: a class still on `Ask` under auto-allow (computer/screen/fetch), or
/// a non-effect class fabricated as `Allow`.
#[test]
fn auto_allow_promotes_every_ask_class_including_computer_and_fetch() {
    let metadata = |permission_overrides| SessionMetadataV1 {
        provider_base_url: None,
        provider_rebind_id: None,
        cwd: "/tmp".into(),
        provider: "fake".into(),
        account_alias: None,
        model: "fake-model".into(),
        max_tokens: 4096,
        permission_overrides,
        interaction_mode: Default::default(),
        system_prompt_version: None,
        title: None,
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        context_economy: Default::default(),
        created_at_ms: 1,
        agent_type: None,
    };
    let defaults = effective_permission_defaults(&metadata(Some(SessionPermissionOverridesV1 {
        read_only: false,
        allow_writes: false,
        allow_exec: false,
        allow_mobile: false,
        auto_allow: true,
    })));
    let decision = |class: EffectClass| {
        defaults
            .iter()
            .find_map(|(candidate, default)| (*candidate == class).then_some(*default))
            .expect("registered effect class")
    };

    // The blanket auto-allow flip lifts the classes that allow_writes/allow_exec
    // deliberately never touch — computer observation and control above all.
    assert_eq!(
        decision(EffectClass::ScreenObserve),
        ToolPermissionDefault::Allow
    );
    assert_eq!(
        decision(EffectClass::ScreenControl),
        ToolPermissionDefault::Allow
    );
    assert_eq!(decision(EffectClass::ReadSms), ToolPermissionDefault::Allow);
    assert_eq!(
        decision(EffectClass::MobileObserve),
        ToolPermissionDefault::Allow
    );
    assert_eq!(
        decision(EffectClass::MobileControl),
        ToolPermissionDefault::Allow
    );
    assert_eq!(decision(EffectClass::FsWrite), ToolPermissionDefault::Allow);
    assert_eq!(
        decision(EffectClass::ProcessExec),
        ToolPermissionDefault::Allow
    );
    // Web fetch is a Network effect (class-family, host-agnostic here).
    let network_allowed = defaults.iter().any(|(class, default)| {
        matches!(class, EffectClass::Network { .. }) && *default == ToolPermissionDefault::Allow
    });
    let network_present = defaults
        .iter()
        .any(|(class, _)| matches!(class, EffectClass::Network { .. }));
    assert!(
        !network_present || network_allowed,
        "auto-allow must promote the network/fetch class to Allow"
    );

    // Auto-allow only ever promotes `Ask`: it leaves no class on the Ask path,
    // while `NotApplicable` non-effects are neither promoted nor removed.
    for (_, default) in &defaults {
        assert_ne!(
            *default,
            ToolPermissionDefault::Ask,
            "auto-allow must leave no class on the Ask path"
        );
    }
}

fn pending_shell(run_id: &str) -> PendingShellExec {
    PendingShellExec {
        accepted: AcceptedShellExec {
            session_id: SessionId::new("deferred-shell-session"),
            run_id: RunId::new(run_id),
            item_id: ItemId::new(format!("item-{run_id}")),
            accepted_seq: 2,
            worker_generation: 1,
        },
        command_id: format!("command-{run_id}"),
        branch_id: None,
        agent_id: None,
        command: "printf deferred".into(),
        cwd: None,
    }
}

/// MUTATION CHECK: warn-and-drop a shell handoff received while the prior
/// provider turn still has in-memory ownership. Expected runtime failure: the
/// accepted run disappears instead of remaining owned for the next loop.
#[test]
fn terminal_turn_cleanup_race_defers_one_owned_shell_handoff() {
    let mut deferred = VecDeque::new();
    defer_shell_handoff(&mut deferred, pending_shell("shell-run"));
    defer_shell_handoff(&mut deferred, pending_shell("shell-run"));
    assert_eq!(deferred.len(), 1, "same receipt handoff is deduplicated");
    assert_eq!(
        deferred.pop_front().expect("owned handoff").accepted.run_id,
        RunId::new("shell-run")
    );
}

/// The tested ownership helper must remain wired into the active-turn
/// production arm; otherwise an accepted handoff can regress to warn/drop
/// while the helper-only law still passes.
///
/// MUTATION CHECK: replace the active arm's `defer_shell_handoff` call with a
/// warning. Expected runtime failure: this source-level seam assertion fails.
#[test]
fn active_supervisor_shell_arm_uses_owned_deferred_handoff() {
    let source = include_str!("worker.rs").replace("\r\n", "\n");
    let arm = source
        .split_once("// Store admission can observe the turn's durable")
        .map(|(_, tail)| tail)
        .expect("active-turn shell arm remains identifiable");
    let arm = arm
        .split_once("Some(SupervisorCommand::Shutdown) | None => {")
        .map(|(arm, _)| arm)
        .expect("active-turn shell arm stays bounded by shutdown");
    assert!(
        arm.contains("defer_shell_handoff(&mut deferred_shell, *pending);"),
        "active-turn shell handoff must retain ownership until cleanup"
    );
}

fn envelope(session_id: &SessionId, event: &str, payload: EventPayload) -> RawEnvelope {
    EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(event),
        seq: 0,
        session_id: session_id.clone(),
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: DeviceId::new("inventory-test"),
        authority_epoch: 0,
        worker_generation: 1,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: serde_json::to_value(payload)
            .expect("payload serializes")
            .into(),
    }
}

/// MUTATION CHECK: fabricate a snapshot row or stop projecting grants from
/// durable effect/menu facts. Expected runtime failure: names/defaults cease
/// matching the registry or the remembered class grant disappears at runtime.
#[tokio::test]
async fn inventory_snapshot_projects_registry_defaults_and_durable_grants() {
    let store = MemoryStore::new();
    let session_id = SessionId::new("inventory-session");
    let effect = EffectId::new("effect-write");
    let menu_id = MenuId::new("menu-write");
    let menu = Menu {
        id: menu_id.clone(),
        kind: MenuKind::Permission {
            effect_summary: "write a file".into(),
        },
        title: "Allow write?".into(),
        body: vec!["Allows FsWrite for this session".into()],
        options: vec![MenuOption {
            key: "approve_for_session".into(),
            label: "Allow for this session".into(),
            detail: None,
            decision: Some(DecisionKind::AllowAlways),
        }],
        blocking: true,
        scope: MenuScope::Session,
        origin: "inventory-test".into(),
        ttl_ms: None,
        timeout_option: None,
    };
    let answer = MenuAnswer {
        menu: menu_id.clone(),
        option_index: 0,
        option_key: Some("approve_for_session".into()),
        value: None,
        via: AnswerVia::Rpc,
    };
    let mut events = [
        envelope(
            &session_id,
            "intent",
            EventPayload::Effect(EffectPhase::Intent(EffectIntent {
                effect: effect.clone(),
                class: EffectClass::FsWrite,
                summary: "write a file".into(),
                args_digest: "write-digest".into(),
                workspace_revision: None,
            })),
        ),
        envelope(
            &session_id,
            "authorized",
            EventPayload::Effect(EffectPhase::Authorized {
                effect,
                verdict: AuthorizationVerdict::Ask {
                    menu: menu_id.clone(),
                },
            }),
        ),
        envelope(&session_id, "menu-opened", EventPayload::MenuOpened(menu)),
        envelope(
            &session_id,
            "menu-answered",
            EventPayload::MenuAnswered(answer),
        ),
    ];
    store
        .append(&mut events)
        .await
        .expect("append durable facts");

    let snapshot = tool_inventory_snapshot(&store, &session_id)
        .await
        .expect("inventory");
    assert_eq!(
        snapshot
            .tools
            .iter()
            .map(|entry| entry.manifest.name.as_str())
            .collect::<Vec<_>>(),
        [
            "list_tools",
            "request_input",
            "plan",
            "loom_register",
            "todo_write",
            "graph_evidence",
            "fs_read",
            "fs_glob",
            "fs_search",
            "fs_write",
            "fs_edit",
            "write",
            "edit",
            "fs_path",
            "process_exec",
            "spawn_subagent",
            "message_subagent",
            "task_output",
            "task_kill",
            "web_fetch",
            "web_search",
            "computer",
            "monitor",
            "list_models",
            "peer_list",
            "peer_send",
            "ssh_list",
            "ssh_shell",
        ]
    );
    // M2e: `workflow_author` is a GATED child capability, excluded from the
    // general inventory snapshot (see `tool_inventory_snapshot`) — the standard
    // projection is the registry MINUS that gated tool. This session holds an
    // FsWrite grant, not the workflow-author grant, so it must not surface.
    let registry: Vec<_> = registered_tools()
        .iter()
        .filter(|entry| !matches!(entry.manifest.name.as_str(), "workflow_author" | "mobile"))
        .collect();
    assert_eq!(snapshot.tools.len(), registry.len());
    for (projected, registered) in snapshot.tools.iter().zip(registry) {
        assert_eq!(projected.manifest, registered.manifest);
        assert_eq!(projected.default, registered.default);
    }
    assert!(snapshot.tools.iter().any(|entry| {
        entry.manifest.name == "fs_write" && entry.default == ToolPermissionDefault::Ask
    }));
    assert_eq!(snapshot.remembered_grants.len(), 1);
    assert_eq!(snapshot.remembered_grants[0].class, EffectClass::FsWrite);
    assert_eq!(
        snapshot.remembered_grants[0].scope,
        RememberedGrantScope::Class
    );
}

/// CU-2 root/child law: screen authority never enters the default delegated
/// grant. A parent must explicitly grant the tool and both dynamic effects.
#[test]
fn computer_is_absent_from_default_child_grant() {
    let grant = crate::worker::default_child_grant();
    assert!(!grant.tools.iter().any(|tool| tool == "computer"));
    assert!(!grant.effect_ceiling.contains(&EffectClass::ScreenObserve));
    assert!(!grant.effect_ceiling.contains(&EffectClass::ScreenControl));
}

#[test]
fn mobile_is_absent_from_default_child_grant() {
    let grant = crate::worker::default_child_grant();
    assert!(!grant.tools.iter().any(|tool| tool == "mobile"));
    assert!(!grant.effect_ceiling.contains(&EffectClass::ReadSms));
    assert!(!grant.effect_ceiling.contains(&EffectClass::MobileObserve));
    assert!(!grant.effect_ceiling.contains(&EffectClass::MobileControl));
}

/// MUTATION CHECK: ignore terminal freshness or use first-write-wins during
/// the durable scan. Expected RUNTIME failure: the literal latest digests are
/// missing or the older digest survives.
#[tokio::test]
async fn durable_tool_state_reduces_latest_freshness_per_session() {
    let store = MemoryStore::new();
    let session_id = SessionId::new("freshness-session");
    let mut events = [
        envelope(
            &session_id,
            "fresh-old",
            EventPayload::Effect(EffectPhase::Outcome {
                effect: EffectId::new("read-old"),
                outcome: EffectOutcome::Ok,
                freshness: Some(FileFreshness {
                    path: "src/lib.rs".into(),
                    digest: "blake3:old-literal".into(),
                }),
                workspace_mutation: None,
            }),
        ),
        envelope(
            &session_id,
            "fresh-new",
            EventPayload::Effect(EffectPhase::Outcome {
                effect: EffectId::new("edit-new"),
                outcome: EffectOutcome::Ok,
                freshness: Some(FileFreshness {
                    path: "src/lib.rs".into(),
                    digest: "blake3:new-literal".into(),
                }),
                workspace_mutation: None,
            }),
        ),
        envelope(
            &session_id,
            "fresh-other",
            EventPayload::Effect(EffectPhase::Outcome {
                effect: EffectId::new("read-other"),
                outcome: EffectOutcome::Ok,
                freshness: Some(FileFreshness {
                    path: "README.md".into(),
                    digest: "blake3:readme-literal".into(),
                }),
                workspace_mutation: None,
            }),
        ),
    ];
    store.append(&mut events).await.expect("append freshness");

    let state = durable_session_tool_state(&store, &session_id)
        .await
        .expect("durable state");
    assert_eq!(state.freshness.len(), 2);
    assert_eq!(
        state
            .freshness
            .get("src/lib.rs")
            .expect("latest lib freshness")
            .digest,
        "blake3:new-literal"
    );
    assert_eq!(
        state
            .freshness
            .get("README.md")
            .expect("readme freshness")
            .digest,
        "blake3:readme-literal"
    );
}

#[tokio::test]
async fn read_only_terminal_failure_rehydrates_from_committed_tool_result_per_run() {
    let store = MemoryStore::new();
    let session_id = SessionId::new("read-only-terminal-session");
    let run_id = RunId::new("denied-run");
    let result = typed_tool_result(&ToolError::PermissionDenied {
        reason: "registry mutation denied: run is --read-only".into(),
    })
    .expect("typed read-only denial");
    let mut event = envelope(
        &session_id,
        "read-only-tool-result",
        EventPayload::ToolResult {
            call_id: "register-1".into(),
            result,
        },
    );
    event.run_id = Some(run_id.clone());
    store
        .append(&mut [event])
        .await
        .expect("append durable denial");

    let failure = durable_read_only_terminal_failure(&store, &session_id, &run_id, true)
        .await
        .expect("reduce terminal failure")
        .expect("read-only terminal is rehydrated");
    assert_eq!(
        failure.code,
        haider_protocol::error::ErrorCode::PermissionDenied
    );
    assert_eq!(
        failure.message,
        "registry mutation denied: run is --read-only"
    );
    assert!(
        durable_read_only_terminal_failure(&store, &session_id, &RunId::new("other-run"), true,)
            .await
            .expect("reduce unrelated run")
            .is_none()
    );
    assert!(
        durable_read_only_terminal_failure(&store, &session_id, &run_id, false)
            .await
            .expect("ignore colliding reason outside read-only mode")
            .is_none()
    );
}

/// MUTATION CHECK: collapse C1 errors into invalid_argument/path_changed or
/// omit the match count/remedy. Expected RUNTIME failure: one of the literal
/// kind/details assertions fails.
#[test]
fn c1_freshness_and_anchor_errors_are_typed_for_the_model() {
    let unread = typed_tool_result(&ToolError::UnreadFile {
        path: PathBuf::from("unread.txt"),
    })
    .expect("typed unread result");
    let unread: serde_json::Value = serde_json::from_str(&unread.preview).expect("unread JSON");
    assert_eq!(unread["error"]["kind"], "unread_file");

    let stale = typed_tool_result(&ToolError::StaleRead {
        path: PathBuf::from("stale.txt"),
        recorded_digest: "blake3:recorded-literal".into(),
        current_digest: "blake3:current-literal".into(),
    })
    .expect("typed stale result");
    let stale: serde_json::Value = serde_json::from_str(&stale.preview).expect("stale JSON");
    assert_eq!(stale["error"]["kind"], "stale_read");
    assert_eq!(
        stale["error"]["details"]["remedy"],
        "re-read before editing"
    );

    let anchor = typed_tool_result(&ToolError::EditAnchor(FsEditAnchorMismatch {
        path: PathBuf::from("anchor.txt"),
        matches: 7,
        replace_all: false,
        nearest_candidate: None,
    }))
    .expect("typed anchor result");
    let anchor: serde_json::Value = serde_json::from_str(&anchor.preview).expect("anchor JSON");
    assert_eq!(anchor["error"]["kind"], "edit_anchor_count");
    assert_eq!(anchor["error"]["details"]["matches"], 7);
}

fn create_durable_session(store: &Store, session_id: &SessionId) {
    store
        .create_session(&SessionCreateCommand {
            command_id: format!("create-{session_id}"),
            request_digest: format!("create-digest-{session_id}"),
            request_json: format!(r#"{{"cwd":"/tmp","session":"{session_id}"}}"#),
            session_id: session_id.clone(),
            cwd: "/tmp".into(),
            provider: "fake".into(),
            model: "fake-v1".into(),
            max_tokens: 4096,
            permission_overrides: None,
            effort: None,
            fast: false,
            cache_policy: Default::default(),
            system_prompt_version: "test-v1".into(),
            event_id: EventId::new(format!("created-{session_id}")),
            device_id: DeviceId::new("recovery-test"),
        })
        .expect("create session");
}

fn append_permission_checkpoint(
    store: &Store,
    session_id: &SessionId,
    run_id: &RunId,
    branch_id: Option<BranchId>,
    state: RunState,
) {
    let menu_id = match &state {
        RunState::InputRequired { menu } | RunState::PermissionRequired { menu } => menu.clone(),
        other => panic!("checkpoint state required, got {other:?}"),
    };
    let menu = Menu {
        id: menu_id,
        kind: MenuKind::Permission {
            effect_summary: "run exact command".into(),
        },
        title: "process_exec requests approval".into(),
        body: vec!["exact command".into()],
        options: vec![MenuOption {
            key: "approve_once".into(),
            label: "Allow once".into(),
            detail: None,
            decision: Some(DecisionKind::AllowOnce),
        }],
        blocking: true,
        scope: MenuScope::Session,
        origin: "recovery-test".into(),
        ttl_ms: None,
        timeout_option: None,
    };
    let generation = store.worker_generation();
    let item_id = ItemId::new(format!("item-{run_id}"));
    let mut events = [
        EventEnvelope {
            run_id: Some(run_id.clone()),
            worker_generation: generation,
            event_id: EventId::new(format!("user-{run_id}")),
            payload: serde_json::to_value(EventPayload::UserMessage {
                text: "run it".into(),
                attachments: Vec::new(),
                mode: haider_protocol::DeliveryMode::Queue,
            })
            .expect("user payload")
            .into(),
            ..envelope(session_id, "template-user", EventPayload::IdleDecayed)
        },
        EventEnvelope {
            run_id: Some(run_id.clone()),
            worker_generation: generation,
            event_id: EventId::new(format!("item-{run_id}")),
            payload: serde_json::to_value(EventPayload::Item(ItemEvent::Started {
                item_id,
                item: TurnItem::ToolCall {
                    call_id: format!("call-{run_id}"),
                    name: "exec".into(),
                    args: serde_json::json!({"command":"printf old"}),
                    status: ToolStatus::InProgress,
                },
            }))
            .expect("item payload")
            .into(),
            ..envelope(session_id, "template-item", EventPayload::IdleDecayed)
        },
        EventEnvelope {
            run_id: Some(run_id.clone()),
            worker_generation: generation,
            event_id: EventId::new(format!("menu-{run_id}")),
            payload: serde_json::to_value(EventPayload::MenuOpened(menu))
                .expect("menu payload")
                .into(),
            ..envelope(session_id, "template-menu", EventPayload::IdleDecayed)
        },
        EventEnvelope {
            run_id: Some(run_id.clone()),
            worker_generation: generation,
            event_id: EventId::new(format!("state-{run_id}")),
            payload: serde_json::to_value(EventPayload::RunState(state))
                .expect("state payload")
                .into(),
            ..envelope(session_id, "template-state", EventPayload::IdleDecayed)
        },
    ];
    for event in &mut events {
        event.branch_id = branch_id.clone();
    }
    store.append(&mut events).expect("append checkpoint");
}

#[test]
fn autonomous_recovery_cas_selects_typed_allow_once() {
    let menu = Menu {
        id: MenuId::new("autonomous-recovery-menu"),
        kind: MenuKind::Permission {
            effect_summary: "write exact file".into(),
        },
        title: "Permission".into(),
        body: Vec::new(),
        options: vec![
            MenuOption {
                key: "reject-first".into(),
                label: "Reject".into(),
                detail: None,
                decision: Some(DecisionKind::RejectOnce),
            },
            MenuOption {
                key: "allow-typed".into(),
                label: "Allow".into(),
                detail: None,
                decision: Some(DecisionKind::AllowOnce),
            },
        ],
        blocking: true,
        scope: MenuScope::Session,
        origin: "effect_broker".into(),
        ttl_ms: None,
        timeout_option: None,
    };
    let checkpoint = RequestInputCheckpoint {
        menu: menu.clone(),
        request_seq: 41,
        opening_generation: 7,
        tool_item_id: ItemId::new("recovered-tool"),
        call_id: "recovered-call".into(),
        tool_name: "fs_write".into(),
        args: "{}".into(),
    };
    let command = autonomous_permission_resolution_command(
        &SessionId::new("autonomous-session"),
        &DeviceId::new("daemon"),
        8,
        &checkpoint,
    )
    .expect("enumerated AllowOnce resolution");
    assert_eq!(command.request_seq, 41);
    assert_eq!(command.worker_generation, 7);
    assert!(command.allow_prior_generation);
    assert_eq!(command.answer.option_key.as_deref(), Some("allow-typed"));
    assert_eq!(command.answer.option_index, 1);
    assert_eq!(command.answer.via, AnswerVia::Hook);

    let unavailable = RequestInputCheckpoint {
        menu: Menu {
            options: vec![menu.options[0].clone()],
            ..menu
        },
        ..checkpoint
    };
    let error = autonomous_permission_resolution_command(
        &SessionId::new("autonomous-session"),
        &DeviceId::new("daemon"),
        8,
        &unavailable,
    )
    .expect_err("malformed autonomous permission menu must not park");
    assert_eq!(
        error.code,
        haider_protocol::error::ErrorCode::PermissionDenied
    );
    assert!(error.message.contains("has no AllowOnce resolution"));
}

/// MUTATION CHECK: accept only `PermissionRequired` or only historical
/// `InputRequired + Permission`. Expected runtime failure: one checkpoint is
/// terminalized instead of returning as a recovered waiter at runtime.
#[tokio::test]
async fn recovery_dual_reads_historical_and_canonical_permission_states() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("store");
    let old_session = SessionId::new("old-permission-session");
    let new_session = SessionId::new("new-permission-session");
    create_durable_session(&store, &old_session);
    create_durable_session(&store, &new_session);
    append_permission_checkpoint(
        &store,
        &old_session,
        &RunId::new("old-run"),
        None,
        RunState::InputRequired {
            menu: MenuId::new("old-menu"),
        },
    );
    append_permission_checkpoint(
        &store,
        &new_session,
        &RunId::new("new-run"),
        Some(BranchId::new("checkpoint-branch")),
        RunState::PermissionRequired {
            menu: MenuId::new("new-menu"),
        },
    );
    drop(store);

    // Bounded StoreLocked retry: drop() can return before the profile
    // lock fully releases under parallel suite load (gate27 hygiene
    // precedent, third fixture in this class).
    let recovered = {
        let mut attempt = 0;
        loop {
            match SqliteStoreHandle::open(root.path()).await {
                Ok(store) => break store,
                Err(error) if error.retryable && attempt < 40 => {
                    attempt += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                Err(error) => panic!("reopen store: {error:?}"),
            }
        }
    };
    let work = recover_interrupted_turns(&recovered, &DeviceId::new("restart"))
        .await
        .expect("recover checkpoints");
    let mut recovered_menus = work
        .into_iter()
        .filter_map(|work| match work {
            RecoveredWork::Checkpoint(checkpoint) => {
                Some((checkpoint.checkpoint.menu.id, checkpoint.accepted.branch_id))
            }
            RecoveredWork::Queued(_)
            | RecoveredWork::Retry(_)
            | RecoveredWork::PartialStream(_)
            | RecoveredWork::RouteWait(_)
            | RecoveredWork::ChildWait(_)
            | RecoveredWork::AdmissionRetry(_)
            | RecoveredWork::WorkflowContinuation(_)
            | RecoveredWork::DelegationMirror(_) => None,
        })
        .collect::<Vec<_>>();
    recovered_menus.sort_by(|left, right| left.0.as_str().cmp(right.0.as_str()));
    assert_eq!(
        recovered_menus,
        [
            (
                MenuId::new("new-menu"),
                Some(BranchId::new("checkpoint-branch"))
            ),
            (MenuId::new("old-menu"), None)
        ]
    );
    for session_id in [&old_session, &new_session] {
        let events = recovered.read(session_id, 0, 64).await.expect("read");
        assert!(!events.into_iter().any(|event| {
            event.payload.decode_event().is_ok_and(
                |payload| matches!(payload, EventPayload::RunState(state) if state.is_terminal()),
            )
        }));
    }
    recovered.close().await.expect("close store");
}

/// MUTATION CHECK: keep a description/bound in the stub, forget to recurse into
/// items/properties, or leave a top-level combinator. Expected RUNTIME failure:
/// removing nested structure, bounds, or parameter semantics would leave the
/// caller without its action constraints now that the manual is gone.
#[test]
fn native_schema_keeps_structure_parameter_semantics_and_bounds() {
    let full = serde_json::json!({
        "type": "object",
        "properties": {
            "path": {"type": "string", "minLength": 1, "description": "the file path"},
            "mode": {"type": "string", "enum": ["a", "b"], "description": "the mode"},
            "edits": {
                "type": "array",
                "minItems": 1,
                "items": {
                    "type": "object",
                    "properties": {
                        "old": {"type": "string", "minLength": 1, "description": "anchor"},
                        "new": {"type": "string"}
                    },
                    "required": ["old", "new"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["path"],
        "additionalProperties": false,
        "anyOf": [{"required": ["path"]}]
    });
    let stub = stub_schema(&full);
    // Structure a provider validates against is kept, recursively.
    assert_eq!(stub["type"], "object");
    assert_eq!(stub["required"], serde_json::json!(["path"]));
    assert_eq!(stub["properties"]["path"]["type"], "string");
    assert_eq!(
        stub["properties"]["mode"]["enum"],
        serde_json::json!(["a", "b"])
    );
    assert_eq!(stub["properties"]["edits"]["type"], "array");
    assert_eq!(
        stub["properties"]["edits"]["items"]["required"],
        serde_json::json!(["old", "new"])
    );
    assert_eq!(
        stub["properties"]["edits"]["items"]["properties"]["old"]["type"],
        "string"
    );
    assert_eq!(
        stub, full,
        "call constraints must survive without a system manual"
    );
}

#[test]
fn native_action_parameters_preserve_the_former_manual_unique_constraints() {
    let factory: Arc<dyn TurnToolFactory> = Arc::new(BrokerToolFactory);
    let tools =
        advertised_tool_definitions(&factory, None, "fake", WebCapabilityDegrade::default());
    let schema = |name: &str| {
        &tools
            .iter()
            .find(|tool| tool.name == name)
            .expect("tool")
            .input_schema
    };
    for side in ["before", "after"] {
        assert_eq!(
            schema("fs_search")["properties"]["context"]["properties"][side]["maximum"],
            5
        );
    }
    assert!(
        schema("fs_search")["properties"]["multiline"]["description"]
            .as_str()
            .expect("description")
            .contains("physical line")
    );
    assert!(
        schema("fs_edit")["properties"]["edits"]["items"]["properties"]["old"]["description"]
            .as_str()
            .expect("description")
            .contains("uniquely unless replace_all")
    );
    assert_eq!(
        schema("fs_path")["properties"]["destination"]["description"],
        "Required for move and copy"
    );
    assert_eq!(
        schema("fs_glob")["properties"]["pattern"]["description"],
        ".git paths are always excluded"
    );
}

/// MUTATION CHECK: restore a duplicate system manual or blank a native tool's
/// semantics. Discovery must leave the system prefix byte-identical.
#[test]
fn every_advertised_tool_has_native_semantics_without_a_duplicate_manual() {
    let factory: Arc<dyn TurnToolFactory> = Arc::new(BrokerToolFactory);
    let root = advertised_tool_definitions(&factory, None, "fake", WebCapabilityDegrade::default());
    assert!(
        root.iter().any(|tool| tool.name == "computer"),
        "root pack should include computer"
    );
    for tool in &root {
        assert!(
            !tool.description.is_empty(),
            "missing native description: {}",
            tool.name
        );
    }
    use crate::worker::SystemPromptBuilder;
    assert_eq!(
        SystemPromptBuilder::shared_immutable_base(&root, "scope"),
        SystemPromptBuilder::shared_immutable_base(&[], "scope")
    );
    assert!(
        root.iter()
            .find(|tool| tool.name == "todo_write")
            .expect("todo")
            .description
            .contains("REPLACE the whole todo list")
    );
    // Lane 967-P1 owner decision: the model must see the foreground ownership
    // boundary, both default bounds, and the durable background alternative.
    let process = &root
        .iter()
        .find(|tool| tool.name == "process_exec")
        .expect("process")
        .description;
    for required in [
        "60 s / 1 MiB",
        "in either local mode, normal leader exit closes inherited output",
        "descendants (including shell &) unmanaged",
        "daemon shutdown will not reclaim them after ownership detaches",
        "supervision failure while the leader is live",
        "sweep only this invocation's group with TERM → 2 s grace → KILL",
        "background=true",
        "task_output/task_kill",
    ] {
        assert!(
            process.contains(required),
            "process_exec manual omitted `{required}`: {process}"
        );
    }
}

/// ACTBIAS MUTATION CHECK: blank, generalize, or swap any action-critical
/// native description. Expected RUNTIME failure: the provider schema no longer
/// tells a weak model what the search/mutation tool does and when to use it.
#[test]
fn search_and_mutation_tool_schema_descriptions_are_pinned() {
    let factory: Arc<dyn TurnToolFactory> = Arc::new(BrokerToolFactory);
    let root = advertised_tool_definitions(&factory, None, "fake", WebCapabilityDegrade::default());
    let expected = [
        (
            "fs_glob",
            "List workspace paths matching a repository-aware glob; use it to find files by name or extension before reading them",
        ),
        (
            "fs_search",
            "Search workspace file contents; use it to locate symbols or text before reading or editing a file",
        ),
        (
            "fs_write",
            "Create or replace one UTF-8 file; use it for a new file or a complete rewrite",
        ),
        (
            "fs_edit",
            "Apply anchored replacements to one UTF-8 file; use it for focused changes after reading the current contents",
        ),
        (
            "fs_path",
            "Move, copy, or delete a workspace path; use it when the requested change affects filesystem structure",
        ),
    ];
    for (name, description) in expected {
        let tool = root
            .iter()
            .find(|tool| tool.name == name)
            .unwrap_or_else(|| panic!("missing advertised tool `{name}`"));
        assert_eq!(
            tool.description, description,
            "native prose drifted for `{name}`"
        );
    }
}

/// MUTATION CHECK: expand the default coding catalog, duplicate native prose
/// in the policy, or change schema bytes without deliberate release accounting.
#[test]
fn instruct_pipe_shrinks_the_advertised_wire_pack() {
    // Before economydiet on merged wave-970: 29 registered / 26 advertised,
    // 725 policy + 5_162 manual + 8_390 name/native/schema = 14_277 bytes.
    // The provider-dialect JSON framing is measured separately by AHRB.
    const PRE_DIET_INSTRUCT_PIPE_BYTES: usize = 13_552;
    // Measured v5 core pack: seven coding tools + list_tools, native prose
    // once and no system manual, preserving parameter constraints and semantics.
    // 13_552 -> 5_670 (-58.2%); do not trim validation bounds to save tokens.
    const EXPECTED_INSTRUCT_PIPE_BYTES: usize = 5_670;
    let factory: Arc<dyn TurnToolFactory> = Arc::new(BrokerToolFactory);
    let authorized =
        advertised_tool_definitions(&factory, None, "fake", WebCapabilityDegrade::default());
    let mut config = haider_core::HarnessConfig::for_session(
        SessionId::new("prompt-byte-pin"),
        DeviceId::new("test-device"),
        0,
        0,
    );
    config.tools = authorized;
    config.enable_tool_discovery(Vec::new());
    let tools = config.tool_definitions();
    assert_eq!(registered_tools().len(), 30);
    assert_eq!(
        tools.len(),
        8,
        "seven coding tools and one discovery primitive"
    );
    let tool_bytes: usize = tools
        .iter()
        .map(|tool| {
            tool.name.len()
                + tool.description.len()
                + serde_json::to_vec(&tool.input_schema)
                    .expect("schema bytes")
                    .len()
        })
        .sum();
    use crate::worker::SystemPromptBuilder;
    let policy =
        SystemPromptBuilder::shared_immutable_base(&[], SystemPromptBuilder::UNSCOPED_GRANT_SCOPE);
    let system = SystemPromptBuilder::shared_immutable_base(
        tools,
        SystemPromptBuilder::UNSCOPED_GRANT_SCOPE,
    );
    assert_eq!(policy.len(), 606);
    let manual_bytes = system.len() - policy.len();
    assert_eq!(
        manual_bytes, 0,
        "native descriptions are the sole tool manual"
    );
    let pipe_bytes = tool_bytes + manual_bytes;
    assert_eq!(pipe_bytes, EXPECTED_INSTRUCT_PIPE_BYTES);
    assert!(pipe_bytes * 2 <= PRE_DIET_INSTRUCT_PIPE_BYTES);
    assert_eq!(
        system.len() + tool_bytes,
        606 + EXPECTED_INSTRUCT_PIPE_BYTES
    );
}

/// C1 MUTATION CHECK: drop the node walk or the agent-type/task rendering.
/// Expected RUNTIME failure: the volatile Loom tail stops describing the
/// daemon-owned specialist binding for each node.
#[test]
fn loom_run_tail_teaches_typed_nodes() {
    use haider_protocol::loom::{LoomTypeSig, compile_pipe, parse_pipe};
    let source = "clip: SourceURL -> Transcript\nresearch @researcher \"pull and transcribe\" :cmd\npublish \"approve\" :human";
    let workflow = compile_pipe(&parse_pipe(source), |id| {
        (id == "researcher").then(|| LoomTypeSig {
            in_type: "SourceURL".into(),
            out_type: "Transcript".into(),
        })
    })
    .expect("compiles");
    let tail = loom_run_tail(&workflow);
    assert!(tail.contains("loom clip rev 1"), "{tail}");
    assert!(tail.contains("SourceURL -> Transcript"), "{tail}");
    assert!(
        tail.contains("research@researcher \"pull and transcribe\""),
        "{tail}"
    );
    assert!(tail.contains("→ publish \"approve\""), "{tail}");
    assert!(tail.contains("daemon-scoped typed nodes"), "{tail}");
}

/// Review round 2 MUTATION CHECK: put the marker outside the cap again.
/// Expected RUNTIME failure: a long workflow's tail exceeds 1200 bytes.
#[test]
fn loom_run_tail_cap_includes_the_ellipsis() {
    use haider_protocol::loom::{compile_pipe, parse_pipe};
    let mut source = String::from("cappy: A -> A\n");
    for index in 0..40 {
        source.push_str(&format!(
            "node{index} \"{}\" :cmd\n",
            "task words repeated over and over ".repeat(4).trim()
        ));
    }
    let workflow = compile_pipe(&parse_pipe(&source), |_| None).expect("compiles");
    let tail = loom_run_tail(&workflow);
    assert!(
        tail.len() <= 1_200,
        "cap must be honest: {} bytes",
        tail.len()
    );
    assert!(tail.contains("\"haider_elision_v1\""), "{tail}");
    assert!(tail.contains("\"scope\":\"loom_workflow_tail\""), "{tail}");
}

/// Review round 2 MUTATION CHECK: drop the chaining check or the first-token
/// membership from [`cli_scope_admits`], or the `@type · ` parse from
/// [`loom_task_type_id`]. Expected RUNTIME failure below — the fence is what
/// keeps a declared-CLI grant from being generic shell (and `curl` from
/// bypassing the API host ceiling).
#[test]
fn typed_cli_fence_admits_only_declared_programs() {
    let clis = vec!["ffmpeg".to_owned(), "yt-dlp".to_owned()];
    assert!(cli_scope_admits(&clis, "ffmpeg -i in.mp4 out.webm").is_ok());
    assert!(cli_scope_admits(&clis, "yt-dlp https://example.com/v").is_ok());
    // Round 3 exact-token law: paths never ride a declared bare name — an
    // attacker-writable ./ffmpeg or /tmp/ffmpeg is not the granted program.
    assert!(cli_scope_admits(&clis, "/opt/homebrew/bin/ffmpeg -version").is_err());
    assert!(cli_scope_admits(&clis, "./ffmpeg -version").is_err());
    // Undeclared programs and every chaining/substitution shape refuse.
    assert!(cli_scope_admits(&clis, "curl https://evil.example").is_err());
    assert!(cli_scope_admits(&clis, "ffmpeg -i a.mp4 b.webm; curl e").is_err());
    assert!(cli_scope_admits(&clis, "ffmpeg $(curl e) out.webm").is_err());
    assert!(cli_scope_admits(&clis, "ffmpeg -i <(curl e) out.webm").is_err());
    assert!(cli_scope_admits(&clis, "ffmpeg -o >(curl e) x").is_err());
    assert!(cli_scope_admits(&clis, "ffmpeg | curl e").is_err());
    assert!(cli_scope_admits(&clis, "ffmpeg `curl e`").is_err());
    assert!(
        cli_scope_admits(&[], "ffmpeg -version").is_err(),
        "deny-all"
    );
    // The daemon-stamped task prefix is the type-id source of truth.
    assert_eq!(
        loom_task_type_id("@researcher · pull the transcript").as_deref(),
        Some("researcher")
    );
    assert_eq!(loom_task_type_id("plain untyped task"), None);
    assert_eq!(loom_task_type_id("@not a prefix"), None);
    // The redirect fence's host scope: Some only for host-scoped grants —
    // a family wildcard or an empty ceiling stays unscoped.
    use haider_protocol::agent::Grant;
    let scoped = Grant {
        tools: Vec::new(),
        effect_ceiling: vec![EffectClass::Network {
            host: "api.fal.ai".into(),
        }],
    };
    assert_eq!(
        scoped_network_hosts(&scoped),
        Some(vec!["api.fal.ai".to_owned()])
    );
    let wildcard = Grant {
        tools: Vec::new(),
        effect_ceiling: vec![
            EffectClass::Network {
                host: String::new(),
            },
            EffectClass::Network {
                host: "api.fal.ai".into(),
            },
        ],
    };
    assert_eq!(scoped_network_hosts(&wildcard), None, "wildcard = unscoped");
    let no_network = Grant {
        tools: Vec::new(),
        effect_ceiling: vec![EffectClass::FsRead],
    };
    assert_eq!(scoped_network_hosts(&no_network), None);
}

/// B2/B3 MUTATION CHECK: widen the typed grant (spawn tools, family network,
/// exec without CLIs), or let a scoped grant admit foreign hosts. Expected
/// RUNTIME failure below.
#[test]
fn typed_child_grants_are_least_privilege_and_host_scoped() {
    use haider_protocol::loom::LoomAgentType;
    let record = |clis: &[&str], apis: &[&str]| LoomAgentType {
        id: "spec".into(),
        name: "Spec".into(),
        job: "do one thing".into(),
        in_type: "A".into(),
        out_type: "B".into(),
        clis: clis.iter().map(|s| s.to_string()).collect(),
        apis: apis.iter().map(|s| s.to_string()).collect(),
        denials: Vec::new(),
        skills: Vec::new(),
        scripts: Vec::new(),
        color: String::new(),
        glyph: String::new(),
        rev: 1,
    };

    // A CLI specialist: exec yes, network no, spawning never.
    let cli_grant = typed_child_grant(&record(&["ffmpeg"], &[]));
    assert!(cli_grant.tools.iter().any(|t| t == "process_exec"));
    assert!(!cli_grant.tools.iter().any(|t| t == "web_fetch"));
    assert!(!cli_grant.tools.iter().any(|t| t == "spawn_subagent"));
    assert!(!cli_grant.tools.iter().any(|t| t == "message_subagent"));
    assert!(cli_grant.effect_ceiling.contains(&EffectClass::ProcessExec));
    assert!(
        !cli_grant
            .effect_ceiling
            .iter()
            .any(|e| matches!(e, EffectClass::Network { .. }))
    );

    // An API specialist: web_fetch admitted, network HOST-SCOPED, no exec.
    let api_grant = typed_child_grant(&record(&[], &["fal.ai"]));
    assert!(api_grant.tools.iter().any(|t| t == "web_fetch"));
    assert!(!api_grant.tools.iter().any(|t| t == "process_exec"));
    assert!(api_grant.effect_ceiling.iter().any(|e| matches!(
        e,
        EffectClass::Network { host } if host == "fal.ai"
    )));

    // Admission: web_fetch's FAMILY manifest effect is admitted by the scoped
    // ceiling (the tool is available)…
    assert!(grant_admits_manifest_effect(
        &api_grant,
        &EffectClass::Network {
            host: String::new()
        }
    ));
    // …but the USE site is bounded to the declared host.
    assert!(web_fetch_host_allowed(&api_grant, "fal.ai"));
    assert!(!web_fetch_host_allowed(&api_grant, "evil.example"));
    // A family (untyped) grant keeps today's allow-any behavior.
    assert!(web_fetch_host_allowed(
        &crate::worker::default_child_grant(),
        "anything.example"
    ));
    // And a grant with NO network at all admits neither family nor host.
    assert!(!grant_admits_manifest_effect(
        &cli_grant,
        &EffectClass::Network {
            host: String::new()
        }
    ));
}

/// E1 MUTATION CHECK: drop the inventory (return None on a populated
/// registry), leak more than names+signatures, or lose the byte cap.
/// Expected RUNTIME failure below.
#[test]
fn loom_inventory_rides_the_tail_bounded() {
    use haider_protocol::loom::{LoomAgentType, LoomTypeSig, compile_pipe, parse_pipe};
    assert_eq!(
        loom_inventory_line(&[], &[]),
        None,
        "empty registry = no tail"
    );
    let record = |id: &str| LoomAgentType {
        id: id.into(),
        name: id.into(),
        job: "job text never leaks into the inventory".into(),
        in_type: "SourceURL".into(),
        out_type: "Transcript".into(),
        clis: Vec::new(),
        apis: Vec::new(),
        denials: Vec::new(),
        skills: Vec::new(),
        scripts: Vec::new(),
        color: "#c2701c".into(),
        glyph: "▲".into(),
        rev: 1,
    };
    let workflow = compile_pipe(
        &parse_pipe("clip: SourceURL -> Transcript\nresearch @researcher \"pull\" :cmd"),
        |_| {
            Some(LoomTypeSig {
                in_type: "SourceURL".into(),
                out_type: "Transcript".into(),
            })
        },
    )
    .expect("compiles");
    let line = loom_inventory_line(&[record("researcher")], std::slice::from_ref(&workflow))
        .expect("populated registry teaches");
    assert!(
        line.contains("@researcher SourceURL -> Transcript"),
        "{line}"
    );
    assert!(line.contains("@clip"), "{line}");
    assert!(line.contains("spawn_subagent(workflow="), "{line}");
    assert!(line.contains("loom_register"), "{line}");
    assert!(
        !line.contains("job text"),
        "jobs never ride the tail: {line}"
    );
    // The cap holds against a large registry.
    let many: Vec<LoomAgentType> = (0..64)
        .map(|index| record(&format!("specialist-number-{index:02}")))
        .collect();
    let capped = loom_inventory_line(&many, &[workflow]).expect("still teaches");
    assert!(capped.len() <= 700, "cap must hold: {} bytes", capped.len());
    assert!(capped.contains("\"haider_elision_v1\""), "{capped}");
    assert!(
        capped.contains("\"scope\":\"loom_registry_inventory\""),
        "{capped}"
    );
}

/// E2 MUTATION CHECK: let loom_register through without a presented,
/// automatically accepted plan containing the registration (drop a needle,
/// or admit any body). Expected RUNTIME failure below — the plan body is the
/// durable content gate.
#[test]
fn loom_register_binds_to_an_accepted_plan() {
    let source = "clip: SourceURL -> Transcript\nresearch @researcher \"pull\" :cmd";
    let accepted = vec![format!(
        "# Register the clip workflow\n\n```\n{source}\n```\nWhy: automation."
    )];
    assert!(plan_gate_admits(&accepted, &[source]));
    // A plan that never showed this source does not satisfy the content gate.
    assert!(!plan_gate_admits(
        &["# Some other proposal entirely".to_owned()],
        &[source]
    ));
    assert!(
        !plan_gate_admits(&[], &[source]),
        "no accepted plans = no gate"
    );
    // Agent types bind by id + job + signature — ALL must appear.
    let needles = [
        "researcher",
        "Pull a source and transcribe it.",
        "SourceURL -> Transcript",
    ];
    let card = "## New specialist: researcher\nJob: Pull a source and transcribe it.\nSignature: SourceURL -> Transcript".to_owned();
    assert!(plan_gate_admits(std::slice::from_ref(&card), &needles));
    assert!(
        !plan_gate_admits(
            &[card],
            &["researcher", "a DIFFERENT job", "SourceURL -> Transcript"]
        ),
        "every needle must bind"
    );
    // Empty needles never admit (a vacuous gate is an open gate).
    assert!(!plan_gate_admits(&["anything".to_owned()], &[]));
    assert!(!plan_gate_admits(&["anything".to_owned()], &[" "]));
}

/// W-flow inline identity: the bound type's tail line names the id, the
/// typed signature, and the job — bounded, with the truncation marked.
///
/// MUTATION CHECK: drop the 700-char cap or the ellipsis. Expected
/// runtime failure: the oversized job below rides the tail whole or
/// truncates silently.
#[test]
fn agent_type_identity_line_is_bounded_and_names_the_signature() {
    let mut record = haider_protocol::loom::LoomAgentType {
        id: "scout".into(),
        name: "Scout".into(),
        job: "find the seams".into(),
        in_type: "Brief".into(),
        out_type: "Map".into(),
        clis: Vec::new(),
        apis: Vec::new(),
        denials: Vec::new(),
        skills: Vec::new(),
        scripts: Vec::new(),
        color: "#7aa2f7".into(),
        glyph: "⌖".into(),
        rev: 1,
    };
    let line = crate::worker::agent_type_identity_line(&record);
    assert_eq!(
        line,
        "session agent type: @scout (Brief -> Map) — find the seams"
    );
    record.job = "j".repeat(2_000);
    let line = crate::worker::agent_type_identity_line(&record);
    assert!(line.contains("\"haider_elision_v1\""), "{line}");
    assert!(
        line.contains("\"scope\":\"loom_agent_type_identity\""),
        "{line}"
    );
    assert!(
        line.len() <= 800,
        "the tail line stays bounded: {}",
        line.len()
    );
}
