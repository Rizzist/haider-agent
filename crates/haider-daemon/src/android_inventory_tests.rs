#![allow(clippy::expect_used, clippy::unwrap_used)]
use super::*;

const EXCLUDED: &[&str] = &[
    "process_exec",
    "exec",
    "task_output",
    "task_kill",
    "workflow_author",
    "computer",
    "peer_list",
    "peer_send",
    "ssh_list",
    "ssh_shell",
];

#[tokio::test]
async fn android_standalone_inventory_golden_and_route_ceiling() {
    use haider_core::{MemoryStore, StoreHandle};
    let store = MemoryStore::new();
    let session = SessionId::new("android-inventory");
    let inactive = tool_inventory_snapshot(&store, &session).await.unwrap();
    let event = super::mobile_runtime_tests::memory_envelope(
        &session,
        "activation",
        "/mobile-use synthetic",
        None,
    );
    store.append(&mut [event]).await.unwrap();
    let active = tool_inventory_snapshot(&store, &session).await.unwrap();
    let factory: Arc<dyn TurnToolFactory> = Arc::new(AndroidToolFactory);
    let child = default_child_grant();
    let restricted = authorized_tool_definitions(&factory, Some(&child), false);
    let fixture = serde_json::json!({
        "scenario":"root inventory from durable journal, mobile activation, restricted child provider definitions",
        "root_active": active,
        "root_inactive": inactive,
        "restricted_child_definitions": restricted,
    });
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/fixtures/android_standalone_tool_inventory.json");
    if std::env::var_os("HAIDER_GENERATE_ANDROID_FIXTURE").is_some() {
        std::fs::write(
            &path,
            format!("{}\n", serde_json::to_string_pretty(&fixture).unwrap()),
        )
        .unwrap();
    }
    let expected: serde_json::Value =
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(fixture, expected);
    for name in EXCLUDED {
        assert!(registered_tool_route(name).is_none(), "{name}");
        assert!(registered_tool_by_name(name).is_none(), "{name}");
        assert!(!child.tools.iter().any(|tool| tool == name), "{name}");
    }
    for name in [
        "write",
        "edit",
        "fs_path",
        "mobile",
        "monitor",
        "loom_register",
    ] {
        assert!(registered_tool_route(name).is_some(), "retained {name}");
    }
}

#[tokio::test]
async fn android_excluded_dispatch_never_journals_an_effect_even_with_auto_allow() {
    let fixture = super::mobile_runtime_tests::mobile_dispatcher_fixture_with_grant(
        "android-denials",
        "use mobile",
        Arc::new(haider_tools::UnavailableMobileBackend),
        None,
    )
    .await;
    for (index, name) in EXCLUDED.iter().enumerate() {
        assert!(!fixture.dispatcher.platform_tool_supported(name));
        fixture
            .dispatcher
            .preflight_tool_call(name)
            .await
            .expect("platform denial precedes workflow preflight");
        let result = fixture
            .dispatcher
            .execute(
                &fixture.run_id,
                &ItemId::new(format!("denied-{index}")),
                &format!("denied-{index}"),
                name,
                serde_json::json!({"command":"touch should-never-exist"}),
                &CancelToken::new(),
            )
            .await
            .expect("typed result");
        let ToolDispatchResult::Completed(result) = result else {
            panic!("excluded dispatch must complete immediately");
        };
        assert!(
            result
                .preview
                .contains(&format!("unsupported tool `{name}`")),
            "{name}: {}",
            result.preview
        );
        assert!(result.effects.is_empty());
    }
    let events = fixture
        .store
        .read(&fixture.session_id, 0, 512)
        .await
        .expect("journal");
    assert!(!events.into_iter().any(|event| {
        event
            .payload
            .decode_event()
            .is_ok_and(|payload| matches!(payload, EventPayload::Effect(_)))
    }));
    super::mobile_runtime_tests::close_fixture(fixture).await;
}

#[tokio::test]
async fn android_recovered_desktop_grants_do_not_reappear_in_inventory() {
    use haider_core::{MemoryStore, StoreHandle};
    use haider_protocol::menu::{AnswerVia, DecisionKind, MenuScope};
    let store = MemoryStore::new();
    let session = SessionId::new("android-recovered-grants");
    for (index, class) in crate::android_policy::HARD_DENIED_EFFECTS
        .iter()
        .chain(std::iter::once(&EffectClass::FsWrite))
        .enumerate()
    {
        let effect = haider_protocol::ids::EffectId::new(format!("recovered-effect-{index}"));
        let menu = MenuId::new(format!("recovered-menu-{index}"));
        let payloads = [
            EventPayload::Effect(EffectPhase::Intent(EffectIntent {
                effect: effect.clone(),
                class: class.clone(),
                summary: "synthetic recovered grant".into(),
                args_digest: format!("synthetic-shape-{index}"),
                workspace_revision: None,
            })),
            EventPayload::Effect(EffectPhase::Authorized {
                effect,
                verdict: AuthorizationVerdict::Ask { menu: menu.clone() },
            }),
            EventPayload::MenuOpened(Menu {
                id: menu.clone(),
                kind: MenuKind::Permission {
                    effect_summary: "synthetic grant".into(),
                },
                title: "Synthetic prior permission".into(),
                body: vec![],
                options: vec![haider_protocol::menu::MenuOption {
                    key: "allow".into(),
                    label: "Allow".into(),
                    detail: None,
                    decision: Some(DecisionKind::AllowAlways),
                }],
                blocking: true,
                scope: MenuScope::Session,
                origin: "synthetic recovery".into(),
                ttl_ms: None,
                timeout_option: None,
            }),
            EventPayload::MenuAnswered(MenuAnswer {
                menu,
                option_index: 0,
                option_key: Some("allow".into()),
                value: None,
                via: AnswerVia::Rpc,
            }),
        ];
        let mut envelopes: Vec<_> = payloads
            .into_iter()
            .enumerate()
            .map(|(offset, payload)| {
                let mut envelope = super::mobile_runtime_tests::memory_envelope(
                    &session,
                    &format!("recovered-{index}-{offset}"),
                    "unused",
                    None,
                );
                envelope.payload =
                    haider_protocol::envelope::RawPayload::from_event(payload).unwrap();
                envelope
            })
            .collect();
        store.append(&mut envelopes).await.unwrap();
    }
    let recovered = durable_session_tool_state(&store, &session).await.unwrap();
    assert_eq!(
        recovered.grants.len(),
        crate::android_policy::HARD_DENIED_EFFECTS.len() + 1,
        "fixture must actually reconstruct every historical grant"
    );
    let inventory = tool_inventory_snapshot(&store, &session).await.unwrap();
    assert_eq!(inventory.remembered_grants.len(), 1);
    assert_eq!(inventory.remembered_grants[0].class, EffectClass::FsWrite);
    assert!(
        !inventory
            .tools
            .iter()
            .any(|tool| EXCLUDED.contains(&tool.manifest.name.as_str()))
    );
}
