//! Recording HTTP fixture: real adapter, daemon, IPC, tool execution and restart.
use super::*;
use haider_protocol::completion::{CompletionEvent, CompletionParkReason, CompletionStatus};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn stream_response(step: usize, name: &str, args: Value) -> String {
    let block = if name.is_empty() {
        json!({"type":"text","text":"Follow-up handled."})
    } else {
        json!({"type":"tool_use","id":format!("fixture-{step}"),"name":name,"input":{}})
    };
    let mut values = vec![
        json!({"type":"message_start","message":{"content":[],"usage":{"input_tokens":10}}}),
        json!({"type":"content_block_start","index":0,"content_block":block}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":if name.is_empty(){"end_turn"}else{"tool_use"}},"usage":{"output_tokens":10}}),
        json!({"type":"message_stop"}),
    ];
    if !name.is_empty() {
        values.insert(2, json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":args.to_string()}}));
    }
    values
        .iter()
        .map(|v| {
            format!(
                "event: {}\ndata: {v}\n\n",
                v["type"].as_str().expect("event type")
            )
        })
        .collect()
}

fn action_evidence(value: &Value) -> Option<String> {
    if let Some(rows) = value.get("action_evidence").and_then(Value::as_array) {
        return rows
            .iter()
            .find(|r| r["call_id"] == "fixture-8")
            .and_then(|r| r["event_id"].as_str())
            .map(str::to_owned);
    }
    match value {
        Value::String(s) => serde_json::from_str::<Value>(s)
            .ok()
            .and_then(|v| action_evidence(&v)),
        Value::Array(rows) => rows.iter().rev().find_map(action_evidence),
        Value::Object(map) => map.values().find_map(action_evidence),
        _ => None,
    }
}

async fn observe_completion(
    client: &mut UdsClient,
    config: &DaemonConfig,
    session: &SessionId,
) -> haider_rpc::SessionObserveDigest {
    send_request(
        client,
        config,
        "observe-completion",
        RequestBody::SessionObserve {
            session_id: session.clone(),
            last_event_limit: 100,
            metadata_only: false,
        },
    )
    .await;
    loop {
        if let WireFrame::Response {
            body: ResponseBody::SessionObserve { digest },
            ..
        } = client.next().await
        {
            return digest;
        }
    }
}

// Local evidence can additionally exercise the built front-door CLI against
// this live isolated daemon. CI's RPC regression does not depend on a sibling
// executable; an explicitly supplied missing/broken binary fails this check.
async fn cli_snapshot(
    config: &DaemonConfig,
    runtime_root: &std::path::Path,
    session: &SessionId,
    expected_pending: usize,
) -> Option<Value> {
    let binary = std::env::var_os("HAIDER_COMPLETION_CLI")?;
    let output = tokio::process::Command::new(binary)
        .args(["session", session.as_str(), "--json", "--no-spawn"])
        .env("HAIDER_PROFILE_DIR", &config.store_dir)
        .env("HAIDER_RUNTIME_DIR", runtime_root)
        .env("HAIDER_DISCOVERY_DISABLED", "1")
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(support::DEADLINE, output)
        .await
        .expect("CLI snapshot deadline")
        .expect("execute actual CLI");
    assert!(
        output.status.success(),
        "CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let snapshot: Value = serde_json::from_slice(&output.stdout).expect("CLI JSON");
    assert_eq!(
        snapshot["session"]["pending_follow_ups"]
            .as_array()
            .map_or(0, Vec::len),
        expected_pending,
        "CLI obligation projection: {snapshot}"
    );
    Some(snapshot)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_shot_http_400_restart_explicit_resume_marker_and_handled_receipt() {
    let root = test_root("completion-http-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("fixture listener");
    let base = format!(
        "http://{}/v1",
        listener.local_addr().expect("fixture address")
    );
    let requests = Arc::new(StdMutex::new(Vec::<Value>::new()));
    let recorded = requests.clone();
    let responses = Arc::new(StdMutex::new(Vec::<Value>::new()));
    let recorded_responses = responses.clone();
    let obligation = Arc::new(StdMutex::new(String::new()));
    let fixture_obligation = obligation.clone();
    let server = tokio::spawn(async move {
        for step in 0..12 {
            let (mut socket, _) = listener.accept().await.expect("HTTP accept");
            let mut bytes = Vec::new();
            let (header_end, length) = loop {
                let mut buf = [0u8; 8192];
                let n = socket.read(&mut buf).await.expect("HTTP read");
                assert!(n > 0, "incomplete request");
                bytes.extend_from_slice(&buf[..n]);
                assert!(bytes.len() < 8 * 1024 * 1024, "bounded HTTP fixture");
                if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&bytes[..end]);
                    let length = headers
                        .lines()
                        .find_map(|l| {
                            let (key, v) = l.split_once(':')?;
                            key.eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse::<usize>().expect("length"))
                        })
                        .expect("content length");
                    if bytes.len() >= end + 4 + length {
                        break (end + 4, length);
                    }
                }
            };
            let body: Value = serde_json::from_slice(&bytes[header_end..header_end + length])
                .expect("recorded JSON");
            recorded.lock().expect("requests").push(body.clone());
            let id = fixture_obligation.lock().expect("obligation").clone();
            let (status, content_type, response) = if step == 0 {
                ("400 Bad Request", "application/json", json!({"type":"error","error":{"type":"invalid_request_error","message":"synthetic completion wake request rejected"}}).to_string())
            } else {
                let (name, args) = match step {
                    1 => ("list_tools", json!({"filter":"monitor"})),
                    2 => ("list_tools", json!({"filter":"fs_write"})),
                    3 => (
                        "monitor",
                        json!({"operation":"follow_up","obligation_id":id,"attempt":1,"completion_action":"claim"}),
                    ),
                    4 => (
                        "fs_write",
                        json!({"path":"completion-marker.txt","content":"one harmless action\n"}),
                    ),
                    6 => ("list_tools", json!({"filter":"monitor"})),
                    7 => ("list_tools", json!({"filter":"fs_read"})),
                    8 => ("fs_read", json!({"path":"completion-marker.txt"})),
                    9 => ("monitor", json!({"operation":"list"})),
                    10 => (
                        "monitor",
                        json!({"operation":"follow_up","obligation_id":id,"attempt":3,"completion_action":"handled","evidence_event_ids":[action_evidence(&body).expect("actual reconciliation result in monitor list")]}),
                    ),
                    _ => ("", json!({})),
                };
                (
                    "200 OK",
                    "text/event-stream",
                    stream_response(step, name, args),
                )
            };
            recorded_responses
                .lock()
                .expect("responses")
                .push(json!({"status":status,"content_type":content_type,"body":response}));
            socket.write_all(format!("HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",response.len()).as_bytes()).await.expect("HTTP response");
        }
    });
    let vault = MemoryVault::default();
    let alias = CredentialAlias::new("synthetic-completion-no-auth");
    vault
        .put(&alias, b"synthetic-not-a-credential")
        .expect("fixture secret");
    let provider: Arc<dyn Provider> = Arc::new(
        AnthropicProvider::new_custom_no_auth(
            vault.resolve(&alias).expect("fixture secret"),
            "claude-fable-5",
            &base,
        )
        .expect("real adapter"),
    );
    let runtime_root = root.path().join("runtime");
    let profile = haider_client::resolve_profile(&haider_client::ProfileEnv {
        profile_dir: Some(root.path().join("store")),
        runtime_dir: Some(runtime_root.clone()),
        ..Default::default()
    })
    .expect("isolated CLI-compatible profile");
    let config = DaemonConfig::new(profile.profile_id, profile.store_dir, profile.runtime_dir);
    let task = ready_with_dependencies(
        &config,
        dependencies([("completion-http".into(), provider.clone())], []),
    )
    .await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        &config.profile_id,
        "completion-client",
        ClientKind::Headless,
    )
    .await;
    let (session, generation) = create_and_attach(
        &mut client,
        &config,
        &workspace,
        "completion-http",
        "claude-fable-5",
        Some(SessionPermissionOverridesV1 {
            read_only: false,
            allow_writes: true,
            allow_exec: false,
            allow_mobile: false,
            auto_allow: false,
        }),
        None,
    )
    .await;
    send_request(
        &mut client,
        &config,
        "register",
        RequestBody::MonitorRegister {
            command_id: CommandId::new("completion-register"),
            session_id: session.clone(),
            worker_generation: generation,
            source: haider_rpc::MonitorSourceWire::Timer {
                interval_ms: 86400000,
            },
            filter: None,
            action: haider_rpc::MonitorActionWire {
                report: true,
                follow_up: Some(
                    "Write completion-marker.txt once, then provide an explicit handled receipt"
                        .into(),
                ),
            },
            occurrence: haider_rpc::MonitorOccurrenceWire::Once,
            lifetime: haider_rpc::MonitorLifetimeWire::Session,
        },
    )
    .await;
    let registered = tokio::time::timeout(support::DEADLINE, next_response(&mut client))
        .await
        .expect("monitor register receipt");
    let monitor_id = match registered {
        WireFrame::Response {
            body: ResponseBody::MonitorRegister { receipt },
            ..
        } => match receipt.outcome {
            haider_rpc::MonitorRegisterOutcomeWire::Registered { monitor } => monitor.monitor_id,
            other => panic!("register: {other:?}"),
        },
        other => panic!("register response: {other:?}"),
    };
    send_request(
        &mut client,
        &config,
        "trigger",
        RequestBody::MonitorMutate {
            command_id: CommandId::new("completion-trigger"),
            session_id: session.clone(),
            worker_generation: generation,
            mutation: haider_rpc::MonitorMutationWire::Trigger { monitor_id },
        },
    )
    .await;
    let mut failed_events = Vec::new();
    tokio::time::timeout(support::DEADLINE, async {
        loop {
            if let WireFrame::Event { envelope, .. } = client.next().await {
                let terminal = matches!(
                    envelope.payload.decode_event(),
                    Ok(EventPayload::RunState(RunState::Errored))
                );
                failed_events.push(envelope);
                if terminal {
                    break;
                }
            }
        }
    })
    .await
    .expect("HTTP 400 reaches terminal");
    let before = observe_completion(&mut client, &config, &session).await;
    let cli_before = cli_snapshot(&config, &runtime_root, &session, 1).await;
    assert_eq!(before.pending_follow_ups.len(), 1);
    let pending = &before.pending_follow_ups[0];
    assert_eq!(pending.status, CompletionStatus::Parked);
    assert_eq!(
        pending.park_reason,
        Some(CompletionParkReason::RequestRepair)
    );
    assert!(!workspace.join("completion-marker.txt").exists());
    *obligation.lock().expect("obligation") = pending.obligation_id.clone();
    drop(client);
    task.shutdown_handle().request("restart boundary");
    task.join().await.expect("first daemon joins");
    let task = ready_with_dependencies(
        &config,
        dependencies([("completion-http".into(), provider.clone())], []),
    )
    .await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        &config.profile_id,
        "restart-client",
        ClientKind::Headless,
    )
    .await;
    let replay = attach_existing(&mut client, &config, session.clone(), "reattach").await;
    let after = observe_completion(&mut client, &config, &session).await;
    let cli_restart = cli_snapshot(&config, &runtime_root, &session, 1).await;
    assert_eq!(after.pending_follow_ups, before.pending_follow_ups);
    assert_eq!(
        requests.lock().expect("requests").len(),
        1,
        "unchanged HTTP 400 is parked across restart"
    );
    let run=submit_turn(&mut client,&config,"explicit-resume",session.clone(),after.worker_generation,"The request is repaired. Claim the pending obligation, perform its action and record the handled receipt.").await;
    let _ = events_until_terminal(&mut client, &run).await;
    let after_action = observe_completion(&mut client, &config, &session).await;
    assert_eq!(after_action.pending_follow_ups.len(), 1);
    assert_eq!(
        after_action.pending_follow_ups[0].status,
        CompletionStatus::AwaitingReceipt,
        "successful action and turn still need a handled receipt"
    );
    assert_eq!(
        fs::read(workspace.join("completion-marker.txt")).expect("marker before crash"),
        b"one harmless action\n"
    );
    drop(client);
    task.crash().await;
    let task = ready_with_dependencies(
        &config,
        dependencies([("completion-http".into(), provider.clone())], []),
    )
    .await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        &config.profile_id,
        "action-restart-client",
        ClientKind::Headless,
    )
    .await;
    let action_replay =
        attach_existing(&mut client, &config, session.clone(), "action-reattach").await;
    let final_state = tokio::time::timeout(support::DEADLINE, async {
        loop {
            let state = observe_completion(&mut client, &config, &session).await;
            if state.pending_follow_ups.is_empty()
                && state.run_state == haider_rpc::ObserveRunStateWire::Idle
            {
                break state;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("restart rearmed the unhandled obligation and reconciled the existing marker");
    assert_eq!(
        fs::read(workspace.join("completion-marker.txt")).expect("real marker"),
        b"one harmless action\n"
    );
    assert_eq!(requests.lock().expect("requests").len(), 12);
    let cli_final = cli_snapshot(&config, &runtime_root, &session, 0).await;
    let journal = read_session(&mut client, &config, session.clone(), "final-journal").await;
    assert_eq!(journal.iter().filter(|e| matches!(e.payload.decode_event(),Ok(EventPayload::ToolResult { ref call_id,ref result }) if call_id=="fixture-4" && result.status.is_completed())).count(),1);
    assert_eq!(
        journal
            .iter()
            .filter(|e| matches!(
                e.payload.decode::<CompletionEvent>(),
                Ok(CompletionEvent::CompletionHandled { .. })
            ))
            .count(),
        1
    );
    assert_eq!(journal.iter().filter(|e| matches!(e.payload.decode_event(), Ok(EventPayload::ToolResult { ref call_id, ref result }) if call_id == "fixture-8" && result.status.is_completed())).count(), 1, "recovery reads the existing marker rather than repeating the write");
    assert_eq!(
        journal
            .iter()
            .filter(|e| matches!(
                e.payload.decode::<CompletionEvent>(),
                Ok(CompletionEvent::CompletionAttempt { .. })
            ))
            .count(),
        3
    );
    drop(client);
    task.crash().await;
    let task = ready_with_dependencies(
        &config,
        dependencies([("completion-http".into(), provider)], []),
    )
    .await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        &config.profile_id,
        "ack-restart-client",
        ClientKind::Headless,
    )
    .await;
    let acknowledged = observe_completion(&mut client, &config, &session).await;
    assert!(
        acknowledged.pending_follow_ups.is_empty(),
        "handled receipt survives daemon crash"
    );
    assert_eq!(
        requests.lock().expect("requests").len(),
        12,
        "handled effects are not rearmed"
    );
    if let Ok(path) = std::env::var("HAIDER_COMPLETION_EVIDENCE_DIR") {
        let path = std::path::PathBuf::from(path);
        fs::create_dir_all(&path).expect("evidence dir");
        for (name, value) in [
            (
                "http-requests.json",
                json!(*requests.lock().expect("requests")),
            ),
            (
                "http-responses.json",
                json!(*responses.lock().expect("responses")),
            ),
            ("failed-events.json", json!(failed_events)),
            ("restart-replay.json", json!(replay)),
            ("journal.json", json!(journal)),
            ("session-before.json", json!(before)),
            ("session-after-restart.json", json!(after)),
            ("session-after-action.json", json!(after_action)),
            ("action-restart-replay.json", json!(action_replay)),
            ("session-final.json", json!(final_state)),
            ("session-after-ack-restart.json", json!(acknowledged)),
            ("cli-before.json", json!(cli_before)),
            ("cli-restart.json", json!(cli_restart)),
            ("cli-final.json", json!(cli_final)),
            (
                "marker.json",
                json!({"successful_action_count":1,"blake3":blake3::hash(&fs::read(workspace.join("completion-marker.txt")).expect("hash actual marker")).to_hex().to_string()}),
            ),
        ] {
            fs::write(
                path.join(name),
                serde_json::to_vec_pretty(&value).expect("evidence JSON"),
            )
            .expect("evidence write");
        }
    }
    drop(client);
    task.shutdown_handle().request("fixture complete");
    task.join().await.expect("fourth daemon joins");
    server.await.expect("HTTP fixture joins");
}
