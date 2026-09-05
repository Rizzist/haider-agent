#![allow(clippy::expect_used)]

use super::*;
use crate::session_hub::SessionHubConfig;
use haider_core::SqliteStoreHandle;

#[test]
fn started_command_summary_keeps_identity_tail_and_machine_marker() {
    let command = format!(
        "cargo test --locked {} -- final-diagnostic",
        "very-long-argument ".repeat(200)
    );
    let first = bounded_task_command(&command);
    let second = bounded_task_command(&command);
    assert_eq!(first, second);
    assert!(first.len() <= TASK_COMMAND_SUMMARY_BYTES);
    assert!(first.starts_with("cargo test --locked"));
    assert!(first.ends_with(" -- final-diagnostic"));
    assert!(first.contains("\"haider_elision_v1\""));
    assert!(first.contains("\"scope\":\"background_task_command\""));
}

fn running_entry(task: &TaskId, output: SharedTaskOutput) -> TaskEntry {
    TaskEntry {
        task: task.clone(),
        name: "buffer-test".into(),
        pid: 1,
        started_at_ms: 1,
        state: TaskLiveState::Running,
        run_id: RunId::new("buffer-test-run"),
        branch_id: None,
        agent_id: None,
        output: Some(output),
        kill: None,
        terminal_fact: None,
    }
}

#[tokio::test]
async fn completed_cas_output_is_evicted_and_paged_through_the_facade() {
    let profile = tempfile::tempdir().expect("profile");
    let store = SqliteStoreHandle::open(profile.path())
        .await
        .expect("store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let facade = TaskFacade::new(hub.clone());
    let registry = hub.task_registry();
    let session = SessionId::new("buffer-test-session");
    let task = TaskId::new("buffer-test-task");
    assert!(registry.begin_adoption(&session));
    let output = shared_task_output(TASK_OUTPUT_RETAIN_BYTES, TASK_TAIL_BYTES);
    lock_task_output(&output).append(b"abcdef");
    let live_buffer = Arc::downgrade(&output);
    registry.insert(&session, running_entry(&task, Arc::clone(&output)));
    drop(output);
    let live_page = facade
        .task_output(&session, task.as_str(), Some(2))
        .await
        .expect("live task output page");
    let live_preview: serde_json::Value =
        serde_json::from_str(&live_page.preview).expect("live task output preview");

    facade
        .complete_task(
            &session,
            &task,
            BackgroundExitStatus {
                exit_code: Some(0),
                signal: None,
                killed: false,
                fault: None,
                workspace_mutation: None,
            },
        )
        .await;

    assert!(live_buffer.upgrade().is_none());
    let entry = registry
        .get(&session, &task)
        .expect("completed task remains projected");
    assert!(entry.output.is_none());
    assert!(matches!(entry.state, TaskLiveState::Terminal(_)));
    assert!(
        entry
            .terminal_fact
            .as_ref()
            .and_then(|fact| fact.artifact.as_ref())
            .is_some()
    );

    let page = facade
        .task_output(&session, task.as_str(), Some(2))
        .await
        .expect("CAS-backed task output page");
    let preview: serde_json::Value =
        serde_json::from_str(&page.preview).expect("task output preview");
    assert_eq!(preview["chunk"], live_preview["chunk"]);
    assert_eq!(preview["next_cursor"], live_preview["next_cursor"]);
    assert_eq!(preview["exhausted"], live_preview["exhausted"]);
    assert_eq!(preview["chunk"], "cdef");
    assert_eq!(preview["next_cursor"], 6);
    assert_eq!(preview["exhausted"], true);
    assert!(page.artifact.is_some());

    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}

#[test]
fn cas_pages_preserve_live_buffer_bytes_and_offsets() {
    let output = shared_task_output(TASK_OUTPUT_RETAIN_BYTES, TASK_TAIL_BYTES);
    lock_task_output(&output).append(b"abcdef");
    for cursor in [0, 2, 5, 6, 20] {
        let (live_bytes, live_next) = lock_task_output(&output).read_from(cursor, 2);
        let (cas_bytes, cas_next, exhausted) = read_task_output_page(b"abcdef", cursor, 2);
        assert_eq!(cas_bytes, live_bytes);
        assert_eq!(cas_next, live_next);
        assert_eq!(exhausted, live_next >= 6);
    }
}

#[test]
fn running_output_is_untouched_before_cas_staging() {
    let registry = TaskRegistry::default();
    let session = SessionId::new("live-buffer-test-session");
    let task = TaskId::new("live-buffer-test-task");
    let output = shared_task_output(TASK_OUTPUT_RETAIN_BYTES, TASK_TAIL_BYTES);
    lock_task_output(&output).append(b"in-flight");
    registry.insert(&session, running_entry(&task, Arc::clone(&output)));

    let entry = registry
        .get(&session, &task)
        .expect("running task remains projected");
    let output = entry.output.as_ref().expect("running output remains live");
    assert_eq!(lock_task_output(output).retained(), b"in-flight");
    assert_eq!(entry.state, TaskLiveState::Running);
    assert!(entry.terminal_fact.is_none());
}

#[test]
fn durable_staging_keeps_running_tasks_killable() {
    let registry = TaskRegistry::default();
    let session = SessionId::new("killable-stage-session");
    let task = TaskId::new("killable-stage-task");
    let output = shared_task_output(TASK_OUTPUT_RETAIN_BYTES, TASK_TAIL_BYTES);
    let (kill, _kill_signal) = task_kill_channel();
    let mut entry = running_entry(&task, output);
    entry.kill = Some(kill);
    registry.insert(&session, entry);
    let fact = TaskCompleted {
        task: task.clone(),
        name: "buffer-test".into(),
        state: TaskTerminalState::Completed { exit_code: Some(0) },
        elapsed_ms: 1,
        output_bytes: 6,
        output_sha256: None,
        tail: "abcdef".into(),
        artifact: Some(haider_protocol::ids::ArtifactRef::new("blake3:buffer-test")),
        full_output_unavailable: false,
        truncated: false,
        delivery: TaskCompletionDelivery::DeliveredQueued,
        workspace_mutation: None,
    };

    registry.stage_durable_output(&session, &task, &fact);

    let entry = registry
        .get(&session, &task)
        .expect("staged task remains projected");
    assert_eq!(entry.state, TaskLiveState::Running);
    assert!(entry.output.is_none());
    assert!(entry.kill.is_some());
}

#[tokio::test]
async fn toolshape_task_output_original_hash_survives_completion_eviction_and_adoption() {
    let profile = tempfile::tempdir().expect("profile");
    let store = SqliteStoreHandle::open(profile.path())
        .await
        .expect("store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let facade = TaskFacade::new(hub.clone());
    let registry = hub.task_registry();
    let session = SessionId::new("toolshape-task-session");
    hub.create_internal_session(haider_core::SessionCreateCommand {
        command_id: "toolshape-create-session".into(),
        request_digest: "toolshape-create-session-digest".into(),
        request_json: "{}".into(),
        session_id: session.clone(),
        cwd: profile.path().to_string_lossy().into_owned(),
        provider: "fake".into(),
        model: "fake-model".into(),
        max_tokens: 4096,
        permission_overrides: None,
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        system_prompt_version: crate::worker::SystemPromptBuilder::VERSION.into(),
        event_id: EventId::new("toolshape-session-created"),
        device_id: hub.device_id(),
    })
    .await
    .expect("durable session");
    let task = TaskId::new("toolshape-task");
    let output = shared_task_output(8, 4);
    let original = b"original\0\xff-stream-beyond-retention-tail";
    lock_task_output(&output).append(original);
    let live_buffer = Arc::downgrade(&output);
    assert!(registry.begin_adoption(&session));
    let entry = running_entry(&task, Arc::clone(&output));
    let mut started = [facade.task_fact_envelope(
        &session,
        &entry.run_id,
        None,
        None,
        "toolshape-task-started",
        TaskStarted {
            task: task.clone(),
            name: entry.name.clone(),
            command: "fixture".into(),
            pid: entry.pid,
            started_at_ms: entry.started_at_ms,
        }
        .to_payload_value()
        .expect("start payload"),
        PromptRender::Omit,
    )];
    hub.append(&mut started).await.expect("durable start");
    registry.insert(&session, entry);
    drop(output);
    let mut live = Vec::new();
    for cursor in [None, Some(0), Some(8)] {
        let result = facade
            .task_output(&session, task.as_str(), cursor)
            .await
            .expect("live output");
        let marker = result.truncation.as_ref().expect("live provenance");
        assert_eq!(marker.original_bytes, original.len() as u64);
        assert_eq!(
            marker.sha256,
            "01a8c9fc3f120663a00ea7d97b30d8a701ad198800bf3e4b0bfca2794bb0a5a4"
        );
        assert_eq!(marker.payload_bytes, result.payload_text().len() as u64);
        assert!(result.preview.ends_with(&marker.marker()));
        live.push(result);
    }
    facade
        .complete_task(
            &session,
            &task,
            BackgroundExitStatus {
                exit_code: Some(0),
                signal: None,
                killed: false,
                fault: None,
                workspace_mutation: None,
            },
        )
        .await;
    assert!(live_buffer.upgrade().is_none());
    let scan = facade
        .scan_session_tasks(&session)
        .await
        .expect("durable completed fact");
    let completed = scan.completed.get(&task).expect("completion journaled");
    assert_eq!(
        completed.output_sha256.as_deref(),
        Some("01a8c9fc3f120663a00ea7d97b30d8a701ad198800bf3e4b0bfca2794bb0a5a4")
    );
    assert_eq!(completed.output_bytes, original.len() as u64);
    assert!(completed.truncated);
    let mut terminal = Vec::new();
    for (cursor, live) in [None, Some(0), Some(8)].into_iter().zip(live) {
        let result = facade
            .task_output(&session, task.as_str(), cursor)
            .await
            .expect("terminal output");
        assert_eq!(
            result
                .truncation
                .as_ref()
                .expect("terminal provenance")
                .sha256,
            live.truncation.as_ref().expect("live provenance").sha256
        );
        let mut before: serde_json::Value =
            serde_json::from_str(live.payload_text()).expect("live JSON");
        let mut after: serde_json::Value =
            serde_json::from_str(result.payload_text()).expect("terminal JSON");
        before.as_object_mut().expect("live object").remove("state");
        after
            .as_object_mut()
            .expect("terminal object")
            .remove("state");
        assert_eq!(before, after, "only lifecycle state changes at completion");
        terminal.push(result);
    }
    registry.remove_session(&session);
    for (cursor, expected) in [None, Some(0), Some(8)].into_iter().zip(terminal) {
        assert_eq!(
            facade
                .task_output(&session, task.as_str(), cursor)
                .await
                .expect("readopted output"),
            expected
        );
    }
    let mut legacy_json = completed.to_payload_value().expect("completed JSON");
    legacy_json
        .as_object_mut()
        .expect("completed object")
        .remove("output_sha256");
    let legacy = TaskEventPayload::from_payload_value(&legacy_json).expect("legacy task decode");
    let TaskEventPayload::TaskCompleted(legacy) = legacy else {
        panic!("completion variant")
    };
    assert!(legacy.output_sha256.is_none());
    registry.set_terminal(&session, &task, legacy.state.clone(), legacy);
    let legacy_output = facade
        .task_output(&session, task.as_str(), None)
        .await
        .expect("legacy output");
    assert!(legacy_output.truncated);
    assert!(
        legacy_output.truncation.is_none(),
        "unknown original hash must not be fabricated"
    );
    assert!(serde_json::from_str::<serde_json::Value>(&legacy_output.preview).is_ok());
    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}
