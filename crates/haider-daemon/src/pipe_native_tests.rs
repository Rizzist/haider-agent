#![allow(clippy::expect_used)]

use super::*;
use haider_protocol::EventPayload;
use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
use haider_protocol::history::{NodeKind, TreeNode};
use haider_protocol::ids::{DeviceId, EventId, ItemId, NodeId, RunId};
use haider_protocol::item::{ItemEvent, ToolStatus, TurnItem};
use haider_protocol::state::RunState;

fn state_envelope(session_id: &SessionId, ordinal: u64) -> RawEnvelope {
    EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(format!("pipe-head-{ordinal}")),
        seq: 0,
        session_id: session_id.clone(),
        branch_id: None,
        run_id: Some(RunId::new("pipe-head-run")),
        agent_id: None,
        device_id: DeviceId::new("pipe-head-device"),
        authority_epoch: 1,
        worker_generation: 1,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: serde_json::to_value(EventPayload::RunState(RunState::Streaming))
            .expect("run state serializes")
            .into(),
    }
}

async fn append_one(
    store: &SqliteStoreHandle,
    session_id: &SessionId,
    ordinal: u64,
) -> RawEnvelope {
    let mut envelopes = [state_envelope(session_id, ordinal)];
    store.append(&mut envelopes).await.expect("append succeeds");
    envelopes.into_iter().next().expect("one envelope")
}

/// MUTATION CHECK: restore the unconditional `latest_seq` call in the known
/// sidecar branch. The in-sync assertion observes one store read instead of
/// zero. Remove lag detection and the trailing-cursor assertion observes zero.
#[tokio::test]
async fn hot_batch_uses_stamped_head_unless_the_sidecar_cursor_trails() {
    let root = tempfile::tempdir().expect("temp profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let writer = PipeNativeWriter::new(root.path());
    let session_id = SessionId::new("pipe-stamped-head");

    let first = append_one(&store, &session_id, 1).await;
    writer
        .maintain(&store, &session_id, std::slice::from_ref(&first), first.seq)
        .await
        .expect("first-touch rebuild succeeds");

    writer.journal_head_reads.store(0, Ordering::Relaxed);
    let second = append_one(&store, &session_id, 2).await;
    writer
        .maintain(
            &store,
            &session_id,
            std::slice::from_ref(&second),
            second.seq,
        )
        .await
        .expect("in-sync hot append succeeds");
    assert_eq!(writer.journal_head_reads.load(Ordering::Relaxed), 0);

    let _third = append_one(&store, &session_id, 3).await;
    let fourth = append_one(&store, &session_id, 4).await;
    writer
        .maintain(&store, &session_id, &[], fourth.seq)
        .await
        .expect("coalesced head wake reconciles the trailing cursor");
    assert_eq!(writer.journal_head_reads.load(Ordering::Relaxed), 1);

    drop(writer);
    store.close().await.expect("store closes");
}

fn projected_envelope(session_id: &SessionId, ordinal: u64, payload: EventPayload) -> RawEnvelope {
    let mut envelope = state_envelope(session_id, ordinal);
    envelope.seq = ordinal;
    *envelope.payload = serde_json::to_value(payload).expect("payload serializes");
    envelope
}

/// MUTATION CHECK: pass the current batch head instead of the latest
/// enqueued high-water mark. The unresolved tool row is flushed from the
/// first render even though another committed batch is waiting.
#[test]
fn queued_head_delays_unresolved_tool_eof_flush() {
    let session_id = SessionId::new("pipe-queued-tool-head");
    let item = projected_envelope(
        &session_id,
        1,
        EventPayload::Item(ItemEvent::Completed {
            item_id: ItemId::new("pipe-queued-tool-item"),
            item: TurnItem::ToolCall {
                call_id: "pipe-queued-tool-call".into(),
                name: "shell".into(),
                args: serde_json::json!({"cmd": "printf queued"}),
                status: ToolStatus::Completed,
            },
        }),
    );
    let node = projected_envelope(
        &session_id,
        2,
        EventPayload::NodeCommitted(TreeNode {
            node: NodeId::new("pipe-queued-tool-node"),
            parent: None,
            kind: NodeKind::ToolExchange {
                tool: "shell".into(),
                summary: "queued unresolved tool".into(),
                artifact: None,
            },
        }),
    );
    let later = projected_envelope(&session_id, 3, EventPayload::RunState(RunState::Streaming));
    let cursor = SidecarCursor {
        seq: 0,
        pending_seq: 0,
        generation: 0,
        segment: 0,
    };
    let mut projector = TranscriptProjector::default();

    let (first, cursor) = render_hot_batch(&[item, node], later.seq, cursor, &mut projector)
        .expect("first queued batch renders");
    assert!(!first.contains("\"name\":\"shell\""));

    let (second, _) = render_hot_batch(
        std::slice::from_ref(&later),
        later.seq,
        cursor,
        &mut projector,
    )
    .expect("latest queued batch renders");
    assert!(second.contains("\"name\":\"shell\""));
}

/// Keep the failed writer itself across the failure/retry boundary. A journal
/// append only enqueues asynchronous maintenance, so removing the obstruction
/// immediately after append can race a first-touch generation-1 rebuild. Joining
/// the actor while the obstruction still exists proves the I/O failure happened;
/// reusing the same writer then proves its dirty state forces generation 9 -> 10.
#[tokio::test]
async fn native_pipe_io_failure_never_fails_the_journal_append() {
    use crate::session_hub::{SessionHub, SessionHubConfig};
    use std::sync::Arc;

    let root = tempfile::tempdir().expect("temp profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let writer = Arc::new(PipeNativeWriter::new(root.path()));
    let hub = SessionHub::new_with_pipe_native(
        store.clone(),
        SessionHubConfig::default(),
        Arc::clone(&writer),
    )
    .expect("hub opens");
    let session_id = SessionId::new("native-pipe-io-failure");
    let path = writer.sidecar_path(&session_id).expect("sidecar path");
    std::fs::write(root.path().join("pipe"), b"blocks the sidecar directory")
        .expect("blocking file writes");

    let mut event = state_envelope(&session_id, 1);
    event.worker_generation = store.worker_generation();
    *event.payload = serde_json::to_value(EventPayload::NodeCommitted(TreeNode {
        node: NodeId::new("durable-user-node"),
        parent: None,
        kind: NodeKind::UserTurn {
            text: "durable".into(),
            attachments: Vec::new(),
        },
    }))
    .expect("user payload serializes");
    let mut events = vec![event];
    hub.append(&mut events)
        .await
        .expect("sidecar failure must not fail append");
    assert_eq!(
        store.read(&session_id, 0, 10).await.expect("journal reads"),
        events
    );
    hub.shutdown().await.expect("failed writer joins");
    assert!(!path.exists());
    assert!(
        writer
            .dirty
            .lock()
            .expect("dirty state lock")
            .contains(&session_id),
        "the obstructed writer must actually fail before the obstruction is removed"
    );

    std::fs::remove_file(root.path().join("pipe")).expect("blocking file removes");
    std::fs::create_dir(root.path().join("pipe")).expect("sidecar directory creates");
    // This tail looks current (seq 1 <= journal head 2), so only dirty-state
    // invalidation forces repair; a journal-ahead tail would rebuild even if
    // the dirty branch were accidentally removed.
    std::fs::write(
        &path,
        b"{\"pipe\":\"haider.session.jsonl\",\"version\":6,\"session_id\":\"native-pipe-io-failure\",\"generation\":9}\n{\"role\":\"user\",\"text\":\"stale\",\"at_ms\":999,\"seq\":1}\n{\"coverage\":1,\"generation\":9}\n",
    )
    .expect("stale sidecar writes");
    let hub = SessionHub::new_with_pipe_native(
        store.clone(),
        SessionHubConfig::default(),
        Arc::clone(&writer),
    )
    .expect("hub resumes with the same dirty writer");
    let mut trigger = vec![state_envelope(&session_id, 2)];
    trigger[0].worker_generation = store.worker_generation();
    hub.append(&mut trigger)
        .await
        .expect("retry trigger commits");
    hub.shutdown().await.expect("retry writer joins");

    let mut expected = header_line(&session_id, 10, 0, 0).expect("header renders");
    expected
        .push_str(&haider_protocol::pipe::sidecar_row_line(&events[0]).expect("durable user row"));
    expected.push('\n');
    expected.push_str(&coverage_line(trigger[0].seq, 10).expect("coverage renders"));
    assert_eq!(
        std::fs::read_to_string(&path).expect("settled sidecar reads"),
        expected,
        "a dirty session must rebuild instead of trusting the old numeric tail"
    );
    assert!(
        !writer
            .dirty
            .lock()
            .expect("dirty state lock")
            .contains(&session_id),
        "successful rebuild clears dirty state"
    );
    assert_eq!(
        writer.confirmed_coverage(&session_id),
        Some((trigger[0].seq, 10))
    );
    drop(hub);
    drop(writer);
    store.close().await.expect("store closes");
}

/// Exercise Windows' ambiguous NotFound classification on every host; native
/// Windows additionally reaches this helper through inspect_sidecar_blocking.
#[test]
fn missing_sidecar_does_not_hide_a_non_directory_parent_or_other_io_error() {
    let root = tempfile::tempdir().expect("temp profile");
    let parent = root.path().join("pipe");
    let path = parent.join("session.pipe");
    let missing = || std::io::Error::from(std::io::ErrorKind::NotFound);
    assert!(matches!(
        classify_root_open_error(&path, missing()).expect("missing directory"),
        SidecarState::Missing
    ));
    std::fs::create_dir(&parent).expect("create directory");
    assert!(matches!(
        classify_root_open_error(&path, missing()).expect("missing leaf"),
        SidecarState::Missing
    ));
    std::fs::remove_dir(&parent).expect("remove directory");
    std::fs::write(&parent, "obstruction").expect("obstruct parent");
    let error = classify_root_open_error(&path, missing())
        .err()
        .expect("non-directory parent is an I/O failure");
    assert!(error.to_string().contains("parent is not a directory"));
    assert!(inspect_sidecar_blocking(&path, &SessionId::new("session")).is_err());
    let error = classify_root_open_error(
        &path,
        std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied sentinel"),
    )
    .err()
    .expect("other I/O failures propagate");
    assert!(error.to_string().contains("denied sentinel"));
}
