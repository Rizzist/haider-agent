#![allow(clippy::expect_used)]

use crate::turn_recovery::{RecoveredWork, recover_interrupted_turns};
use crate::worker::{
    BrokerToolFactory, PendingShellExec, RegisteredToolRoute, TurnToolFactory, defer_shell_handoff,
    durable_session_tool_state, effective_permission_defaults, registered_tool_route,
    registered_tools, tool_inventory_snapshot, typed_tool_result,
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
        registered_tool_route("message_subagent"),
        Some(RegisteredToolRoute::MessageSubagent)
    );
}

/// MUTATION CHECK: drop any C1 registry entry or change its typed route.
/// Expected RUNTIME failure: the literal manifest name has no matching route.
#[test]
fn advertised_equals_dispatchable_for_all_three_c1_tools() {
    let registry = registered_tools();
    for (name, route) in [
        ("fs_search", RegisteredToolRoute::FsSearch),
        ("fs_glob", RegisteredToolRoute::FsGlob),
        ("fs_edit", RegisteredToolRoute::FsEdit),
    ] {
        assert!(
            registry.iter().any(|entry| entry.manifest.name == name),
            "{name} must be advertised"
        );
        assert_eq!(registered_tool_route(name), Some(route));
    }
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

    let writes = metadata(Some(SessionPermissionOverridesV1 {
        allow_writes: true,
        allow_exec: false,
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
    }));
    assert_eq!(
        decision(&exec, EffectClass::FsWrite),
        ToolPermissionDefault::Ask
    );
    assert_eq!(
        decision(&exec, EffectClass::ProcessExec),
        ToolPermissionDefault::Allow
    );
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
    let source = include_str!("worker.rs");
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
    let registry = registered_tools();
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
            RecoveredWork::Queued(_) | RecoveredWork::ChildWait(_) => None,
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
