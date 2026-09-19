//! Direct user shell context and capture paging across a real daemon restart.
use super::*;

/// MUTATION: omit the terminal user CommandExecution from prompt history, skip
/// retain_foreground_capture, or require a ToolResult for direct capture replay.
/// The next actual provider request, page content or restart lookup must fail.
#[tokio::test]
async fn user_shell_context_and_secret_safe_capture_pages_survive_restart() {
    #[cfg(windows)]
    let _windows_process_test = windows_real_process_test_guard(
        "user_shell_context_and_secret_safe_capture_pages_survive_restart",
    )
    .await;
    assert_user_shell_capture_survives_restart(false).await;
}

/// MUTATION: route the worker process signal through actor_for instead of
/// existing_actor. Drain then loses the durable pointer before restart/paging.
#[tokio::test]
async fn draining_in_flight_user_shell_retains_capture_pages_after_restart() {
    #[cfg(windows)]
    let _windows_process_test = windows_real_process_test_guard(
        "draining_in_flight_user_shell_retains_capture_pages_after_restart",
    )
    .await;
    assert_user_shell_capture_survives_restart(true).await;
}

async fn assert_user_shell_capture_survives_restart(drain_in_flight: bool) {
    for lines in [1, 6000] {
        let root = test_root("bang-context-");
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace).expect("workspace");
        let raw = format!(
            "HEAD\n{}api=sk-abcdefghijklmnopQRSTUV\nTAIL\n",
            (0..lines)
                .map(|i| format!("line {i}: command output\n"))
                .collect::<String>()
        );
        fs::write(workspace.join("capture.txt"), &raw).expect("synthetic fixture");
        let safe = haider_tools::redact_output_text(&raw);
        let config = DaemonConfig::new(
            "bang-context",
            root.path().join("store"),
            root.path().join("runtime"),
        );
        let (dependencies, first_fake) = fake_dependencies(Vec::new());
        let first_task = ready_with_dependencies(&config, dependencies).await;
        let mut client = UdsClient::connect_control(
            &config.endpoint_path(),
            config.frame_limit,
            "bang-context",
            "first",
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
        #[cfg(unix)]
        let command = if drain_in_flight {
            "cat capture.txt; sleep 45"
        } else {
            "cat capture.txt"
        };
        #[cfg(windows)]
        let command = if drain_in_flight {
            "[Console]::Out.Write([IO.File]::ReadAllText('capture.txt')); [Console]::Out.Flush(); Start-Sleep -Seconds 45"
        } else {
            "[Console]::Out.Write([IO.File]::ReadAllText('capture.txt'))"
        };
        send_request(
            &mut client,
            &config,
            "bang",
            RequestBody::ShellExec {
                command_id: CommandId::new("bang"),
                session_id: session.clone(),
                worker_generation: generation,
                command: command.into(),
                cwd: None,
            },
        )
        .await;
        let run = match next_response(&mut client).await {
            WireFrame::Response {
                body:
                    ResponseBody::ShellExec {
                        run_id: Some(run), ..
                    },
                ..
            } => run,
            other => panic!("shell receipt: {other:?}"),
        };
        let mut events = Vec::new();
        if drain_in_flight {
            // Gate shutdown on observed output from the live process. The
            // final newline flushes the shared streaming redactor before EOF.
            tokio::time::timeout(support::DEADLINE, async {
                while stdout_bytes(&events).len() < safe.len() {
                    let WireFrame::Event { envelope, .. } = client.next().await else {
                        continue;
                    };
                    if envelope.run_id.as_ref() != Some(&run) {
                        continue;
                    }
                    let payload = envelope.payload.decode_event().expect("shell event");
                    assert!(
                        !matches!(&payload, EventPayload::RunState(state) if state.is_terminal()),
                        "direct command must still be running when drain starts"
                    );
                    events.push(payload);
                }
            })
            .await
            .expect("live direct-command output before drain");
            first_task
                .shutdown_handle()
                .request("drain in-flight direct command");
            let (state, failure, terminal_events) =
                events_until_any_terminal(&mut client, &run).await;
            assert_eq!(
                state,
                RunState::Cancelled,
                "drain cancellation: {failure:?}"
            );
            events.extend(terminal_events);
        } else {
            events = events_until_terminal(&mut client, &run).await;
            first_task.shutdown_handle().request("restart");
        }
        drop(client);
        let outcome = first_task.join().await.expect("first daemon joins");
        assert_eq!(outcome, haider_daemon::ShutdownOutcome::Graceful);
        assert_eq!(stdout_bytes(&events), safe.as_bytes());
        assert!(
            first_fake.requests().is_empty(),
            "user command does not invoke the provider"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, EventPayload::ToolResult { .. })),
            "never invent a model tool result"
        );
        let signal = events
            .iter()
            .find_map(|event| match event {
                EventPayload::ProcessSignalRecorded(signal) => Some(signal),
                _ => None,
            })
            .expect("process signal");
        assert!(
            signal.artifact.is_some(),
            "small and spilled captures are both durable"
        );
        let effect_handle = format!("capture:{}", signal.effect_id);
        let alias = format!("cap:{}", signal.call_id);

        // The fake asks for every page via the real task_output tool after
        // restart. It never supplies page content; CAS + the shared redactor do.
        let mut script = vec![
            FakeStep::EmitToolCall {
                call_id: "discover-capture".into(),
                name: "list_tools".into(),
                args: serde_json::json!({"filter": "task_output"}),
            },
            FakeStep::Finish {
                reason: FinishReason::ToolUse,
            },
            FakeStep::ExpectToolResult {
                call_id: "discover-capture".into(),
            },
        ];
        let page_bytes = haider_tools::ORCHESTRATION_PREVIEW_MAX_BYTES;
        let pages = safe.len().div_ceil(page_bytes);
        for page in 0..pages {
            script.push(FakeStep::EmitToolCall {
                call_id: format!("page-{page}"),
                name: "task_output".into(),
                args: serde_json::json!({"task_id": alias, "cursor": page * page_bytes}),
            });
            script.push(FakeStep::Finish {
                reason: FinishReason::ToolUse,
            });
            script.push(FakeStep::ExpectToolResult {
                call_id: format!("page-{page}"),
            });
        }
        script.push(FakeStep::EmitText {
            text: "capture replay observed".into(),
        });
        script.push(FakeStep::Finish {
            reason: FinishReason::EndTurn,
        });
        let (dependencies, fake) = fake_dependencies(script);
        let task = ready_with_dependencies(&config, dependencies).await;
        let mut client = UdsClient::connect_control(
            &config.endpoint_path(),
            config.frame_limit,
            "bang-context",
            "second",
            ClientKind::Headless,
        )
        .await;
        let replay = attach_existing(&mut client, &config, session.clone(), "resume").await;
        assert!(
            replay.iter().any(|envelope| matches!(
                envelope.payload.decode_event(),
                Ok(EventPayload::ProcessSignalRecorded(replayed)) if &replayed == signal
            )),
            "capture pointer survives durable replay"
        );
        let generation = replay.last().expect("replayed events").worker_generation;
        // Replayed envelopes retain their original generation; session.list
        // supplies the current daemon generation used by the next submission.
        send_request(
            &mut client,
            &config,
            "list",
            RequestBody::SessionList {
                cursor: None,
                limit: 10,
                order: Default::default(),
            },
        )
        .await;
        let current_generation = match next_response(&mut client).await {
            WireFrame::Response {
                body: ResponseBody::SessionList { sessions, .. },
                ..
            } => {
                sessions
                    .iter()
                    .find(|row| row.session_id == session)
                    .expect("session")
                    .worker_generation
            }
            other => panic!("session list: {other:?}"),
        };
        assert!(current_generation > generation);
        let next = submit_turn(
            &mut client,
            &config,
            "after-bang",
            session,
            current_generation,
            "Explain what I just ran and read its full capture",
        )
        .await;
        let events = events_until_terminal(&mut client, &next).await;
        assert!(continuation_seen(&events, "capture replay observed"));
        let requests = fake.requests();
        assert_eq!(requests.len(), pages + 2);
        let records = requests[0]
            .messages
            .iter()
            .filter_map(|message| {
                message.blocks.iter().find_map(|block| match block {
                    Block::Text { text } if text.contains("[user-initiated shell command]") => {
                        Some((message.role, text.to_owned_string()))
                    }
                    _ => None,
                })
            })
            .collect::<Vec<_>>();
        assert_eq!(records.len(), 1, "one first-class user command record");
        assert_eq!(records[0].0, haider_provider::MessageRole::User);
        let record = &records[0].1;
        assert!(record.contains(command));
        assert!(record.contains("origin: user_command"));
        if drain_in_flight {
            assert!(record.contains("status: cancelled"));
        }
        assert!(record.contains("HEAD") && record.contains("TAIL"));
        assert!(
            !record.contains(&effect_handle),
            "provider content must not expose the volatile effect handle"
        );
        if drain_in_flight || lines > 1 {
            assert!(
                record.contains(&alias) && record.contains("task_output("),
                "cancelled or reduced output remains pageable by deterministic alias"
            );
        } else {
            assert!(
                !record.contains(&alias) && !record.contains("task_output("),
                "a fully displayed successful capture needs no paging footer"
            );
        }
        assert!(!record.contains("sk-abcdefghijklmnopQRSTUV"));
        assert!(record.len() < haider_core::USER_COMMAND_OUTPUT_PREVIEW_BYTES + 2048);
        if lines > 1 {
            assert!(record.contains("preview truncated"));
        }
        let mut reconstructed = String::new();
        for event in &events {
            if let EventPayload::ToolResult { call_id, result } = event
                && call_id.starts_with("page-")
            {
                let page: serde_json::Value =
                    serde_json::from_str(result.payload_text()).expect("page JSON");
                reconstructed.push_str(
                    page["chunk"]
                        .as_str()
                        .unwrap_or_else(|| panic!("capture page: {page}")),
                );
                assert_eq!(page["task_id"], alias);
                assert_eq!(page["output_bytes"], safe.len());
                assert_eq!(page["next_cursor"], reconstructed.len());
                assert_eq!(page["exhausted"], reconstructed.len() == safe.len());
            }
        }
        assert_eq!(
            reconstructed, safe,
            "all pages use the complete safe capture"
        );
        eprintln!(
            "bang context restart: drain_in_flight={drain_in_flight} raw_bytes={} safe_bytes={} provider_record_bytes={} capture_pages={pages}",
            raw.len(),
            safe.len(),
            record.len()
        );
        task.shutdown_handle().request("test complete");
        task.join().await.expect("daemon joins");
    }
}
