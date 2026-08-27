#![allow(clippy::expect_used)]

use super::*;
use crate::session_hub::SessionHubConfig;
use haider_core::SqliteStoreHandle;

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
