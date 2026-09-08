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

#[test]
fn android_standalone_inventory_golden_and_route_ceiling() {
    let entries = registered_tools();
    let project = |active: bool, grant: Option<&Grant>| {
        entries
            .iter()
            .filter(|entry| active || entry.manifest.name != "mobile")
            .filter(|entry| {
                grant.is_none_or(|grant| {
                    grant_admits_tool_manifest(grant, &entry.manifest.name, &entry.manifest.effects)
                })
            })
            .map(|entry| ToolInventoryEntry {
                manifest: entry.manifest.clone(),
                default: entry.default,
            })
            .collect::<Vec<_>>()
    };
    let child = default_child_grant();
    let fixture = serde_json::json!({
        "scenario":"root, mobile-use activated, no remembered grants",
        "root_active":{"tools":project(true, None), "remembered_grants":[]},
        "root_inactive":{"tools":project(false, None), "remembered_grants":[]},
        "restricted_child":{"tools":project(false, Some(&child)), "remembered_grants":[]}
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
