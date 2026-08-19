#![allow(clippy::expect_used)]

use crate::turn_recovery::{RecoveredWork, recover_interrupted_turns};
use crate::worker::{
    BrokerToolFactory, PendingShellExec, RegisteredToolRoute, TurnToolFactory,
    WebCapabilityDegrade, advertised_tool_definitions, cli_scope_admits, defer_shell_handoff,
    durable_session_tool_state, effective_permission_defaults, explicit_computer_auto_grant_value,
    explicit_computer_use_intent, grant_admits_manifest_effect, loom_run_tail, loom_task_type_id,
    registered_tool_route, registered_tools, scoped_network_hosts, stub_schema,
    tool_inventory_snapshot, tool_manual, tool_manual_line, typed_child_grant, typed_tool_result,
    web_fetch_host_allowed,
};
use haider_core::{MemoryStore, SqliteStoreHandle, StoreHandle};
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
use haider_protocol::session::{SessionMetadataV1, SessionPermissionOverridesV1};
use haider_protocol::state::RunState;
use haider_protocol::tool::{RememberedGrantScope, ToolPermissionDefault};
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
fn session_permission_overrides_replace_only_write_and_exec_ask_defaults() {
    let metadata = |permission_overrides| SessionMetadataV1 {
        cwd: "/tmp".into(),
        provider: "fake".into(),
        model: "fake-model".into(),
        max_tokens: 4096,
        permission_overrides,
        system_prompt_version: None,
        title: None,
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        created_at_ms: 1,
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
        decision(&baseline, EffectClass::ScreenObserve),
        ToolPermissionDefault::Ask
    );
    assert_eq!(
        decision(&baseline, EffectClass::ScreenControl),
        ToolPermissionDefault::Ask
    );

    let writes = metadata(Some(SessionPermissionOverridesV1 {
        allow_writes: true,
        allow_exec: false,
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
        allow_writes: false,
        allow_exec: true,
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
        decision(&exec, EffectClass::ScreenObserve),
        ToolPermissionDefault::Ask,
        "allow_exec must never imply screen observation"
    );
    assert_eq!(
        decision(&exec, EffectClass::ScreenControl),
        ToolPermissionDefault::Ask,
        "allow_exec must never imply screen control"
    );
}

/// MUTATION CHECK: make `auto_allow` a no-op, scope it to only writes/exec, or
/// let it promote a `NotApplicable` class into an effect. Expected RUNTIME
/// failure: a class still on `Ask` under auto-allow (computer/screen/fetch), or
/// a non-effect class fabricated as `Allow`.
#[test]
fn auto_allow_promotes_every_ask_class_including_computer_and_fetch() {
    let metadata = |permission_overrides| SessionMetadataV1 {
        cwd: "/tmp".into(),
        provider: "fake".into(),
        model: "fake-model".into(),
        max_tokens: 4096,
        permission_overrides,
        system_prompt_version: None,
        title: None,
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        created_at_ms: 1,
    };
    let defaults = effective_permission_defaults(&metadata(Some(SessionPermissionOverridesV1 {
        allow_writes: false,
        allow_exec: false,
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
        .split_once(
            "Some(SupervisorCommand::ShellExec(pending)) => {\n                            // Store admission",
        )
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
        payload: serde_json::to_value(payload).expect("payload serializes"),
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
            "request_input",
            "plan",
            "todo_write",
            "graph_evidence",
            "fs_read",
            "fs_glob",
            "fs_search",
            "fs_write",
            "fs_edit",
            "fs_path",
            "process_exec",
            "spawn_subagent",
            "message_subagent",
            "task_output",
            "task_kill",
            "web_fetch",
            "web_search",
            "computer",
        ]
    );
    // M2e: `workflow_author` is a GATED child capability, excluded from the
    // general inventory snapshot (see `tool_inventory_snapshot`) — the standard
    // projection is the registry MINUS that gated tool. This session holds an
    // FsWrite grant, not the workflow-author grant, so it must not surface.
    let registry: Vec<_> = registered_tools()
        .into_iter()
        .filter(|entry| entry.manifest.name != "workflow_author")
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
            .expect("user payload"),
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
            .expect("item payload"),
            ..envelope(session_id, "template-item", EventPayload::IdleDecayed)
        },
        EventEnvelope {
            run_id: Some(run_id.clone()),
            worker_generation: generation,
            event_id: EventId::new(format!("menu-{run_id}")),
            payload: serde_json::to_value(EventPayload::MenuOpened(menu)).expect("menu payload"),
            ..envelope(session_id, "template-menu", EventPayload::IdleDecayed)
        },
        EventEnvelope {
            run_id: Some(run_id.clone()),
            worker_generation: generation,
            event_id: EventId::new(format!("state-{run_id}")),
            payload: serde_json::to_value(EventPayload::RunState(state)).expect("state payload"),
            ..envelope(session_id, "template-state", EventPayload::IdleDecayed)
        },
    ];
    for event in &mut events {
        event.branch_id = branch_id.clone();
    }
    store.append(&mut events).expect("append checkpoint");
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
            | RecoveredWork::ChildWait(_) => None,
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
            serde_json::from_value::<EventPayload>(event.payload).is_ok_and(
                |payload| matches!(payload, EventPayload::RunState(state) if state.is_terminal()),
            )
        }));
    }
    recovered.close().await.expect("close store");
}

/// MUTATION CHECK: keep a description/bound in the stub, forget to recurse into
/// items/properties, or leave a top-level combinator. Expected RUNTIME failure:
/// the stub still carries prose/bounds, or drops the nested structure a native
/// tool call is validated against.
#[test]
fn stub_schema_keeps_structure_drops_prose_and_bounds() {
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
    // Prose and every daemon-re-enforced bound/combinator are gone everywhere.
    let text = serde_json::to_string(&stub).expect("serialize stub");
    for banned in [
        "description",
        "minLength",
        "minItems",
        "additionalProperties",
        "anyOf",
        "the file path",
        "anchor",
    ] {
        assert!(!text.contains(banned), "stub must drop `{banned}`: {text}");
    }
}

/// MUTATION CHECK: add a tool without a manual line, or leave a description on
/// the wire. Expected RUNTIME failure: an advertised tool has no signature the
/// model can read, or a wire ToolDefinition still carries a description.
#[test]
fn every_advertised_tool_is_manual_described_and_wire_is_description_free() {
    let factory: Arc<dyn TurnToolFactory> = Arc::new(BrokerToolFactory);
    let root = advertised_tool_definitions(&factory, None, "fake", WebCapabilityDegrade::default());
    assert!(
        root.iter().any(|tool| tool.name == "computer"),
        "root pack should include computer"
    );
    for tool in &root {
        assert!(
            tool_manual_line(&tool.name).is_some(),
            "advertised tool `{}` has no manual line — instruct-pipe drift",
            tool.name
        );
        assert!(
            tool.description.is_empty(),
            "wire tool `{}` still carries a description — semantics belong in the manual",
            tool.name
        );
    }
    let manual = tool_manual(&root);
    for signature in ["fs_read(", "process_exec(", "computer(", "todo_write("] {
        assert!(manual.contains(signature), "manual missing `{signature}`");
    }
    // The whole-list-replace teaching that used to live on the todo_write wire
    // description now lives in the manual.
    assert!(manual.contains("REPLACE the whole todo list"));
}

/// MUTATION CHECK: stop stubbing, stop emptying wire descriptions, or move
/// nothing into the manual. Expected RUNTIME failure: the instruct pipe stops
/// being a real (>1/3) net reduction of the advertised prefix.
#[test]
fn instruct_pipe_shrinks_the_advertised_wire_pack() {
    let factory: Arc<dyn TurnToolFactory> = Arc::new(BrokerToolFactory);
    let stubbed =
        advertised_tool_definitions(&factory, None, "fake", WebCapabilityDegrade::default());
    let registry = registered_tools();
    // Original prefix: each tool's name + full description + full schema.
    let full_prefix: usize = stubbed
        .iter()
        .map(|tool| {
            let manifest = registry
                .iter()
                .find(|entry| entry.manifest.name == tool.name)
                .expect("advertised tool has a registry manifest");
            tool.name.len()
                + manifest.manifest.description.len()
                + serde_json::to_string(&manifest.manifest.input_schema)
                    .expect("serialize full")
                    .len()
        })
        .sum();
    // Instruct-pipe prefix: name + (empty) wire description + stub schema, plus
    // the one shared manual carried once in the system prompt.
    let stub_wire: usize = stubbed
        .iter()
        .map(|tool| {
            tool.name.len()
                + tool.description.len()
                + serde_json::to_string(&tool.input_schema)
                    .expect("serialize stub")
                    .len()
        })
        .sum();
    let new_total = stub_wire + tool_manual(&stubbed).len();
    assert!(full_prefix > 0 && new_total < full_prefix);
    assert!(
        full_prefix - new_total > full_prefix / 3,
        "instruct pipe must cut the advertised prefix by >1/3 (new {new_total} vs full {full_prefix})"
    );
}

/// C1 MUTATION CHECK: drop the node walk or the agent-type/task rendering.
/// Expected RUNTIME failure: the volatile Loom tail stops teaching the model
/// which specialist runs which node.
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
    assert!(tail.contains("spawn_subagent(agent_type"), "{tail}");
}

/// Review round 2 MUTATION CHECK: put the ellipsis OUTSIDE the cap again.
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
    assert!(tail.ends_with('…'), "long tail must mark truncation");
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
    assert!(cli_scope_admits(&clis, "/opt/homebrew/bin/ffmpeg -version").is_ok());
    assert!(cli_scope_admits(&clis, "yt-dlp https://example.com/v").is_ok());
    // Undeclared programs and every chaining/substitution shape refuse.
    assert!(cli_scope_admits(&clis, "curl https://evil.example").is_err());
    assert!(cli_scope_admits(&clis, "ffmpeg -i a.mp4 b.webm; curl e").is_err());
    assert!(cli_scope_admits(&clis, "ffmpeg $(curl e) out.webm").is_err());
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
