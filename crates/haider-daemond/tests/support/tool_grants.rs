//! Session discovery regressions through the production SQLite/RPC path.

use super::*;
use haider_protocol::tool::{ToolResultData, ToolResultStatus};

#[tokio::test]
async fn discovered_monitor_is_callable_on_the_next_turn_without_relisting() {
    monitor_across_turns(false).await;
}

#[tokio::test]
async fn discovered_monitor_is_callable_after_daemon_restart_and_session_attach() {
    monitor_across_turns(true).await;
}

async fn monitor_across_turns(restart: bool) {
    let root = test_root("tool-grants-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let mut script = tool_round(
        "discover",
        "list_tools",
        serde_json::json!({"filter": "monitor"}),
        "discovered",
    );
    script.extend(tool_round(
        "register",
        "monitor",
        serde_json::json!({
            "operation": "register",
            "source": {"kind": "timer", "interval_ms": 86_400_000},
            "action": {"report": true},
        }),
        "registered",
    ));
    let (dependencies, fake) = fake_dependencies(script);
    let dependencies = dependencies.with_tool_exposure(Some(Vec::new()));
    let config = DaemonConfig::new(
        "tool-grants",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let mut task = ready_with_dependencies(&config, dependencies.clone()).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "tool-grants",
        "first-client",
        ClientKind::Headless,
    )
    .await;
    let (session, mut generation) = create_and_attach(
        &mut client,
        &config,
        &workspace,
        "fake",
        "fake-v1",
        None,
        None,
    )
    .await;
    let run = submit_turn(
        &mut client,
        &config,
        "discover-turn",
        session.clone(),
        generation,
        "discover monitor",
    )
    .await;
    let events = events_until_terminal(&mut client, &run).await;
    assert!(events.iter().any(|event| matches!(
        event,
        EventPayload::ToolResult { call_id, result }
            if call_id == "discover"
                && result.status == ToolResultStatus::Completed
                && matches!(&result.data, Some(ToolResultData::ToolsDiscovered { promoted })
                    if promoted == &["monitor"])
    )));

    if restart {
        drop(client);
        task.shutdown_handle()
            .request("restart for discovery replay");
        task.join().await.expect("first daemon joins");
        task = ready_with_dependencies(&config, dependencies).await;
        client = UdsClient::connect_control(
            &config.endpoint_path(),
            config.frame_limit,
            "tool-grants",
            "restarted-client",
            ClientKind::Headless,
        )
        .await;
        send_request(
            &mut client,
            &config,
            "list-after-restart",
            RequestBody::SessionList {
                cursor: None,
                limit: 10,
                order: Default::default(),
            },
        )
        .await;
        let next_generation = match next_response(&mut client).await {
            WireFrame::Response {
                body: ResponseBody::SessionList { sessions, .. },
                ..
            } => {
                sessions
                    .iter()
                    .find(|row| row.session_id == session)
                    .expect("recovered session")
                    .worker_generation
            }
            other => panic!("expected session.list, got {other:?}"),
        };
        assert!(next_generation > generation);
        generation = next_generation;
        let replay = attach_existing(&mut client, &config, session.clone(), "reattach").await;
        assert!(replay.iter().any(|event| matches!(
            event.payload.decode_event(),
            Ok(EventPayload::ToolResult { result, .. })
                if matches!(result.data, Some(ToolResultData::ToolsDiscovered { .. }))
        )));
    }
    let run = submit_turn(
        &mut client,
        &config,
        "register-turn",
        session.clone(),
        generation,
        "register the monitor",
    )
    .await;
    let events = events_until_terminal(&mut client, &run).await;
    let result = events
        .iter()
        .find_map(|event| match event {
            EventPayload::ToolResult { call_id, result } if call_id == "register" => Some(result),
            _ => None,
        })
        .expect("monitor result");
    assert_eq!(result.status, ToolResultStatus::Completed, "{result:?}");
    assert!(continuation_seen(&events, "registered"));
    assert!(!events.iter().any(|event| matches!(
        event, EventPayload::Item(ItemEvent::Started {
            item: TurnItem::ToolCall { name, .. }, ..
        }) if name == "list_tools"
    )));
    let requests = fake.requests();
    assert_eq!(requests.len(), 4);
    assert!(!requests[0].tools.iter().any(|tool| tool.name == "monitor"));
    assert!(requests[2].tools.iter().any(|tool| tool.name == "monitor"));
    assert_eq!(requests[1].tools, requests[2].tools);
    assert_eq!(requests[2].tools, requests[3].tools);
    let journal = read_session(&mut client, &config, session, "read-registration").await;
    assert_eq!(
        journal
            .iter()
            .filter(|event| event.payload["type"] == "monitor_registered")
            .count(),
        1
    );
    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

#[tokio::test]
async fn economy_without_promotions_keeps_request_count_and_coding_schemas() {
    let root = test_root("tool-grants-economy-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    fs::write(workspace.join("note.txt"), "economy fixture").expect("fixture");
    let script = ["read-1", "read-2"]
        .into_iter()
        .flat_map(|call_id| {
            tool_round(
                call_id,
                "fs_read",
                serde_json::json!({"path": "note.txt"}),
                "read",
            )
        })
        .collect();
    let (dependencies, fake) = fake_dependencies(script);
    let config = DaemonConfig::new(
        "tool-grants-economy",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let task =
        ready_with_dependencies(&config, dependencies.with_tool_exposure(Some(Vec::new()))).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "tool-grants",
        "economy-client",
        ClientKind::Headless,
    )
    .await;
    let (session, generation) = create_and_attach(
        &mut client,
        &config,
        &workspace,
        "fake",
        "fake-v1",
        None,
        None,
    )
    .await;
    for call in ["read-1", "read-2"] {
        let run = submit_turn(
            &mut client,
            &config,
            call,
            session.clone(),
            generation,
            "read note.txt",
        )
        .await;
        let events = events_until_terminal(&mut client, &run).await;
        assert!(tool_payload(&events, call).contains("economy fixture"));
    }
    let requests = fake.requests();
    assert_eq!(requests.len(), 4, "one tool hop and one finish per turn");
    assert_eq!(
        requests[0]
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        [
            "list_tools",
            "todo_write",
            "fs_read",
            "fs_glob",
            "fs_search",
            "fs_write",
            "fs_edit",
            "process_exec",
            "spawn_subagent",
        ]
    );
    for request in &requests[1..] {
        assert_eq!(request.tools, requests[0].tools);
        assert_eq!(request.system_prompt, requests[0].system_prompt);
    }
    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

#[tokio::test]
async fn discovered_monitor_cannot_bypass_an_explicit_process_deny() {
    let root = test_root("tool-grants-deny-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let mut script = tool_round(
        "discover",
        "list_tools",
        serde_json::json!({"filter": "monitor"}),
        "discovered",
    );
    script.extend(tool_round(
        "denied-registration",
        "monitor",
        serde_json::json!({
            "operation": "register",
            "source": {"kind": "process", "command": "denied-executable"},
            "action": {"report": true},
        }),
        "denial observed",
    ));
    let (dependencies, fake) = fake_dependencies(script);
    let config = DaemonConfig::new(
        "tool-grants-deny",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let task =
        ready_with_dependencies(&config, dependencies.with_tool_exposure(Some(Vec::new()))).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "tool-grants",
        "deny-client",
        ClientKind::Headless,
    )
    .await;
    let (session, generation) = create_and_attach(
        &mut client,
        &config,
        &workspace,
        "fake",
        "fake-v1",
        Some(SessionPermissionOverridesV1 {
            read_only: true,
            ..Default::default()
        }),
        None,
    )
    .await;
    let first = submit_turn(
        &mut client,
        &config,
        "discover",
        session.clone(),
        generation,
        "discover monitor",
    )
    .await;
    events_until_terminal(&mut client, &first).await;
    let second = submit_turn(
        &mut client,
        &config,
        "denied-registration",
        session.clone(),
        generation,
        "try the denied command",
    )
    .await;
    let (_, _, events) = events_until_any_terminal(&mut client, &second).await;
    assert!(
        fake.requests()[2]
            .tools
            .iter()
            .any(|tool| tool.name == "monitor")
    );
    let result = events
        .iter()
        .find_map(|event| match event {
            EventPayload::ToolResult { call_id, result } if call_id == "denied-registration" => {
                Some(result)
            }
            _ => None,
        })
        .expect("denied monitor receipt");
    assert_eq!(result.status, ToolResultStatus::Rejected, "{result:?}");
    assert_eq!(
        result.reason.as_deref(),
        Some("process execution denied: run is --read-only")
    );
    let journal = read_session(&mut client, &config, session, "read-denied-session").await;
    assert!(
        !journal
            .iter()
            .any(|event| event.payload["type"] == "monitor_registered")
    );
    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}
