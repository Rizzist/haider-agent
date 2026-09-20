#![allow(clippy::expect_used)]

use super::*;
use haider_core::{BranchCreateCommand, BranchCreateOutcome, SessionCreateCommand};
use haider_protocol::EventPayload;
use haider_protocol::envelope::{
    EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION, write_envelope_messagepack,
};
use haider_protocol::history::{CompactionResume, NodeKind, TreeNode};
use haider_protocol::ids::{ArtifactRef, BranchId, DeviceId, EventId, ItemId, NodeId, RunId};
use haider_protocol::item::{ItemEvent, ToolStatus, TurnItem};
use haider_protocol::state::RunState;

fn state_envelope(session_id: &SessionId, ordinal: u64) -> RawEnvelope {
    EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(format!("{session_id}-pipe-head-{ordinal}")),
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

/// MUTATION CHECK: discard the verified cached projector on a head-only wake.
/// The suffix-row assertion stays zero because the old path reconstructs a
/// full join window instead of consuming exactly the two missed commits.
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
    writer.cached_suffix_reads.store(0, Ordering::Relaxed);
    writer.cached_suffix_rows.store(0, Ordering::Relaxed);
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
    assert_eq!(writer.journal_head_reads.load(Ordering::Relaxed), 0);
    assert_eq!(writer.cached_suffix_reads.load(Ordering::Relaxed), 2);
    assert_eq!(writer.cached_suffix_rows.load(Ordering::Relaxed), 2);

    drop(writer);
    store.close().await.expect("store closes");
}

fn projected_envelope(session_id: &SessionId, ordinal: u64, payload: EventPayload) -> RawEnvelope {
    let mut envelope = state_envelope(session_id, ordinal);
    envelope.seq = ordinal;
    *envelope.payload = serde_json::to_value(payload).expect("payload serializes");
    envelope
}

fn user_node_envelope(
    session_id: &SessionId,
    ordinal: u64,
    branch_id: Option<BranchId>,
    text: &str,
) -> RawEnvelope {
    let mut envelope = projected_envelope(
        session_id,
        ordinal,
        EventPayload::NodeCommitted(TreeNode {
            node: NodeId::new(format!("pipe-user-node-{ordinal}")),
            parent: None,
            kind: NodeKind::UserTurn {
                text: text.into(),
                attachments: Vec::new(),
            },
        }),
    );
    envelope.branch_id = branch_id;
    envelope
}

fn projected_sidecar_values(
    writer: &PipeNativeWriter,
    session_id: &SessionId,
) -> Vec<serde_json::Value> {
    let base = writer.sidecar_path(session_id).expect("sidecar path");
    reachable_sidecar_paths(&base, session_id)
        .expect("reachable sidecar chain")
        .into_iter()
        .flat_map(|path| {
            std::fs::read_to_string(path)
                .expect("sidecar segment reads")
                .lines()
                .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("sidecar JSON"))
                .filter(|value| {
                    value.get("pipe").is_none()
                        && value.get("coverage").is_none()
                        && value.get("segment_end").is_none()
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

fn remove_sidecar_chain(writer: &PipeNativeWriter, session_id: &SessionId) {
    let base = writer.sidecar_path(session_id).expect("sidecar path");
    let paths = reachable_sidecar_paths(&base, session_id).expect("reachable sidecar chain");
    for path in paths {
        std::fs::remove_file(path).expect("remove derived sidecar segment");
    }
}

async fn create_branch(
    store: &SqliteStoreHandle,
    session_id: &SessionId,
    branch_id: BranchId,
    source_branch_id: Option<BranchId>,
    fork_node_id: NodeId,
    fork_seq: u64,
) {
    let command_id = format!("create-{branch_id}");
    let request_json = serde_json::json!({
        "session_id": session_id,
        "worker_generation": store.worker_generation(),
        "source_branch_id": source_branch_id,
        "fork_node_id": fork_node_id,
        "fork_seq": fork_seq,
    })
    .to_string();
    let result = store
        .create_branch(BranchCreateCommand {
            command_id: command_id.clone(),
            request_digest: blake3::hash(request_json.as_bytes()).to_hex().to_string(),
            request_json,
            session_id: session_id.clone(),
            worker_generation: store.worker_generation(),
            branch_id,
            source_branch_id,
            fork_node_id,
            fork_seq,
            name: None,
            event_id: EventId::new(format!("{session_id}-{command_id}")),
            device_id: DeviceId::new("pipe-head-device"),
        })
        .await
        .expect("create branch");
    assert!(matches!(result, BranchCreateOutcome::Committed { .. }));
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

#[tokio::test]
async fn incremental_projection_matches_cold_oracle_across_branch_compaction_fork_and_reopen() {
    let root = tempfile::tempdir().expect("temp profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let writer = PipeNativeWriter::new(root.path());
    let session_id = SessionId::new("pipe-incremental-oracle");
    let branch_a = BranchId::new("pipe-branch-a");
    let branch_b = BranchId::new("pipe-branch-b");
    store
        .create_session(SessionCreateCommand {
            command_id: "create-pipe-incremental-oracle".into(),
            request_digest: "create-pipe-incremental-oracle-digest".into(),
            request_json: r#"{"session":"pipe-incremental-oracle"}"#.into(),
            session_id: session_id.clone(),
            cwd: root.path().to_string_lossy().into_owned(),
            provider: "fake".into(),
            model: "fake-model".into(),
            max_tokens: 4_096,
            permission_overrides: None,
            effort: None,
            fast: false,
            cache_policy: Default::default(),
            system_prompt_version: "pipe-native-test-v1".into(),
            event_id: EventId::new("pipe-incremental-oracle-created"),
            device_id: DeviceId::new("pipe-head-device"),
        })
        .await
        .expect("create owned session");

    let mut first = vec![
        user_node_envelope(&session_id, 1, None, "parent branch one"),
        projected_envelope(&session_id, 2, EventPayload::RunState(RunState::Done)),
    ];
    store.append(&mut first).await.expect("append first branch");
    writer
        .maintain(&store, &session_id, &first, first[1].seq)
        .await
        .expect("initial projection");

    create_branch(
        &store,
        &session_id,
        branch_a.clone(),
        None,
        NodeId::new("pipe-user-node-1"),
        first[0].seq,
    )
    .await;
    let mut branch_a_suffix = vec![
        user_node_envelope(&session_id, 4, Some(branch_a.clone()), "parent branch two"),
        projected_envelope(&session_id, 5, EventPayload::RunState(RunState::Done)),
    ];
    for envelope in &mut branch_a_suffix {
        envelope.run_id = Some(RunId::new("pipe-branch-a-run"));
    }
    branch_a_suffix[1].branch_id = Some(branch_a.clone());
    store
        .append(&mut branch_a_suffix)
        .await
        .expect("append first named branch");
    create_branch(
        &store,
        &session_id,
        branch_b.clone(),
        Some(branch_a),
        NodeId::new("pipe-user-node-4"),
        branch_a_suffix[0].seq,
    )
    .await;

    let mut suffix = vec![
        projected_envelope(
            &session_id,
            7,
            EventPayload::NodeCommitted(TreeNode {
                node: NodeId::new("pipe-compaction-node"),
                parent: None,
                kind: NodeKind::Compaction {
                    covers_from: NodeId::new("pipe-user-node-1"),
                    covers_to: NodeId::new("pipe-user-node-4"),
                    summary_artifact: ArtifactRef::new("pipe-summary-artifact"),
                    tokens_before: 100,
                    tokens_after: 10,
                    resume_cause: CompactionResume::AutoMidTurn,
                },
            }),
        ),
        projected_envelope(&session_id, 8, EventPayload::RunState(RunState::Done)),
    ];
    for envelope in &mut suffix {
        envelope.run_id = Some(RunId::new("pipe-branch-b-run"));
    }
    suffix[0].branch_id = Some(branch_b.clone());
    suffix[1].branch_id = Some(branch_b);
    store
        .append(&mut suffix)
        .await
        .expect("append branch suffix");
    writer
        .maintain(&store, &session_id, &[], suffix[1].seq)
        .await
        .expect("missed-commit suffix projection");
    let hot = projected_sidecar_values(&writer, &session_id);

    writer.release_clean(&session_id);
    remove_sidecar_chain(&writer, &session_id);
    let oracle = PipeNativeWriter::new(root.path());
    oracle
        .maintain(&store, &session_id, &[], suffix[1].seq)
        .await
        .expect("from-scratch oracle projection");
    assert_eq!(projected_sidecar_values(&oracle, &session_id), hot);

    let child_session = SessionId::new("pipe-incremental-oracle-child");
    let mut child = [user_node_envelope(
        &child_session,
        1,
        None,
        "fork child stays isolated",
    )];
    store.append(&mut child).await.expect("append fork child");
    oracle
        .maintain(&store, &child_session, &child, child[0].seq)
        .await
        .expect("project fork child");
    assert!(
        projected_sidecar_values(&oracle, &child_session)
            .iter()
            .any(|value| value.to_string().contains("fork child stays isolated"))
    );
    assert_eq!(projected_sidecar_values(&oracle, &session_id), hot);

    oracle.release_clean(&session_id);
    oracle.release_clean(&child_session);
    store.close().await.expect("close first store");
    let reopened = SqliteStoreHandle::open(root.path())
        .await
        .expect("reopen store");
    let restarted = PipeNativeWriter::new(root.path());
    restarted
        .maintain(&reopened, &session_id, &[], suffix[1].seq)
        .await
        .expect("reopen projection");
    assert_eq!(projected_sidecar_values(&restarted, &session_id), hot);
    drop(restarted);
    reopened.close().await.expect("close reopened store");
}

#[tokio::test]
async fn cached_revision_rebuilds_on_same_sequence_replacement_and_truncation() {
    let root = tempfile::tempdir().expect("temp profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let writer = PipeNativeWriter::new(root.path());
    let session_id = SessionId::new("pipe-revision-fence");
    let mut events = vec![
        user_node_envelope(&session_id, 1, None, "retained one"),
        user_node_envelope(&session_id, 2, None, "retained two"),
        user_node_envelope(&session_id, 3, None, "truncated three"),
    ];
    store
        .append(&mut events)
        .await
        .expect("append revision fixture");
    writer
        .maintain(&store, &session_id, &events, events[2].seq)
        .await
        .expect("initial revision projection");
    let generation = writer
        .confirmed_coverage(&session_id)
        .expect("initial coverage")
        .1;

    store.close().await.expect("close before head replacement");
    let mut replacement = user_node_envelope(&session_id, 3, None, "replacement three");
    replacement.seq = events[2].seq;
    replacement.event_id = EventId::new("pipe-revision-replacement-event");
    replacement.committed_at_ms = events[2].committed_at_ms;
    let mut encoded_replacement = Vec::new();
    write_envelope_messagepack(&mut encoded_replacement, &replacement)
        .expect("encode replacement envelope");
    let raw = rusqlite::Connection::open(root.path().join("store.sqlite"))
        .expect("open raw journal for replacement");
    raw.execute(
        "UPDATE events SET envelope_json = ?3, event_id = ?4
         WHERE session_id = ?1 AND seq = ?2",
        rusqlite::params![
            session_id.as_str(),
            i64::try_from(events[2].seq).expect("test seq fits SQLite"),
            encoded_replacement,
            replacement.event_id.as_str(),
        ],
    )
    .expect("replace journal head at the same sequence");
    drop(raw);
    let replaced = SqliteStoreHandle::open(root.path())
        .await
        .expect("reopen replaced store");
    writer
        .maintain(&replaced, &session_id, &[], replacement.seq)
        .await
        .expect("same-sequence mismatch rebuilds");
    assert!(
        writer
            .confirmed_coverage(&session_id)
            .expect("replacement coverage")
            .1
            > generation
    );
    let projected = projected_sidecar_values(&writer, &session_id);
    assert!(
        projected
            .iter()
            .any(|value| value.to_string().contains("replacement three"))
    );

    replaced.close().await.expect("close before truncation");
    let raw =
        rusqlite::Connection::open(root.path().join("store.sqlite")).expect("open raw journal");
    raw.pragma_update(None, "foreign_keys", false)
        .expect("disable derived-row foreign keys for truncation fixture");
    raw.execute(
        "DELETE FROM events WHERE session_id = ?1 AND seq > 2",
        [session_id.as_str()],
    )
    .expect("truncate journal");
    drop(raw);
    let reopened = SqliteStoreHandle::open(root.path())
        .await
        .expect("reopen truncated store");

    writer
        .maintain(&reopened, &session_id, &[], 2)
        .await
        .expect("truncated authority rebuilds");
    let projected = projected_sidecar_values(&writer, &session_id);
    assert!(
        projected
            .iter()
            .any(|value| value.to_string().contains("retained one"))
    );
    assert!(
        projected
            .iter()
            .any(|value| value.to_string().contains("retained two"))
    );
    assert!(
        projected
            .iter()
            .all(|value| !value.to_string().contains("replacement three"))
    );
    drop(writer);
    reopened.close().await.expect("close truncated store");
}

#[tokio::test]
async fn cached_and_cold_projection_refuse_the_same_corrupt_matching_envelope() {
    let root = tempfile::tempdir().expect("temp profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let writer = PipeNativeWriter::new(root.path());
    let session_id = SessionId::new("pipe-corrupt-suffix");
    let mut first = [user_node_envelope(&session_id, 1, None, "valid prefix")];
    store.append(&mut first).await.expect("append valid prefix");
    writer
        .maintain(&store, &session_id, &first, first[0].seq)
        .await
        .expect("project valid prefix");
    let mut corrupt = [user_node_envelope(&session_id, 2, None, "corrupt suffix")];
    store
        .append(&mut corrupt)
        .await
        .expect("append corrupt target");
    store.close().await.expect("close before corruption");
    let raw =
        rusqlite::Connection::open(root.path().join("store.sqlite")).expect("open raw journal");
    raw.execute(
        "UPDATE events SET envelope_json = X'C1' WHERE event_id = ?1",
        [corrupt[0].event_id.as_str()],
    )
    .expect("corrupt matching envelope");
    drop(raw);
    let reopened = SqliteStoreHandle::open(root.path())
        .await
        .expect("reopen corrupt store");

    let cached_error = writer
        .maintain(&reopened, &session_id, &[], corrupt[0].seq)
        .await
        .expect_err("cached suffix must refuse corruption")
        .into_store_error()
        .expect("cached refusal preserves store error");
    let cold = PipeNativeWriter::new(root.path());
    let cold_error = cold
        .maintain(&reopened, &session_id, &[], corrupt[0].seq)
        .await
        .expect_err("cold replay must refuse corruption")
        .into_store_error()
        .expect("cold refusal preserves store error");
    assert_eq!(cached_error, cold_error);
    drop(cold);
    drop(writer);
    reopened.close().await.expect("close corrupt store");
}

#[tokio::test]
async fn reconciled_projection_cache_is_bounded_and_eviction_replays_authority() {
    let root = tempfile::tempdir().expect("temp profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let writer = PipeNativeWriter::new(root.path());
    let first = SessionId::new("pipe-bounded-00");
    for index in 0..=RECONCILED_SESSION_LIMIT {
        let session_id = SessionId::new(format!("pipe-bounded-{index:02}"));
        let event = append_one(&store, &session_id, 1).await;
        writer
            .maintain(&store, &session_id, std::slice::from_ref(&event), event.seq)
            .await
            .expect("project bounded session");
    }
    assert_eq!(writer.reconciled_count(), RECONCILED_SESSION_LIMIT);
    assert_eq!(
        writer.confirmed_coverage(&first),
        None,
        "oldest entry evicted"
    );
    writer
        .maintain(&store, &first, &[], 1)
        .await
        .expect("evicted entry replays journal authority");
    assert_eq!(writer.reconciled_count(), RECONCILED_SESSION_LIMIT);
    assert_eq!(
        writer.confirmed_coverage(&first).map(|value| value.0),
        Some(1)
    );
    drop(writer);
    store.close().await.expect("close store");
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

#[tokio::test]
async fn retraction_rebuilds_hot_cold_and_crash_reconciled_transcripts() {
    let root = tempfile::tempdir().expect("profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let writer = PipeNativeWriter::new(root.path());
    let session_id = SessionId::new("pipe-retraction");
    let mut original = Vec::new();
    for (ordinal, text) in [(1, "remove this draft"), (3, "keep this draft")] {
        let mut user = state_envelope(&session_id, ordinal);
        user.payload = serde_json::to_value(EventPayload::UserMessage {
            text: text.into(),
            attachments: Vec::new(),
            mode: haider_protocol::DeliveryMode::Steer,
        })
        .expect("user")
        .into();
        original.push(user);
        let mut node = state_envelope(&session_id, ordinal + 1);
        node.payload = serde_json::to_value(EventPayload::NodeCommitted(TreeNode {
            node: NodeId::new(format!("node-{ordinal}")),
            parent: None,
            kind: NodeKind::UserTurn {
                text: text.into(),
                attachments: Vec::new(),
            },
        }))
        .expect("node")
        .into();
        original.push(node);
    }
    store.append(&mut original).await.expect("original journal");
    writer
        .maintain(&store, &session_id, &original, 4)
        .await
        .expect("initial view");
    let path = writer.sidecar_path(&session_id).expect("path");
    assert!(
        std::fs::read_to_string(&path)
            .expect("view")
            .contains("remove this draft")
    );
    let mut fact = state_envelope(&session_id, 5);
    fact.payload = haider_protocol::retraction::PromptRetractedV1 {
        prompt_seq: 1,
        prompt_node_id: NodeId::new("node-1"),
        text: "remove this draft".into(),
        attachments: Vec::new(),
    }
    .to_payload_value()
    .expect("fact")
    .into();
    let mut facts = [fact];
    store.append(&mut facts).await.expect("durable retraction");
    // Simulate a crash after journal commit, before the old writer's wake.
    let boot_writer = PipeNativeWriter::new(root.path());
    let mut boot = boot_writer
        .begin_boot_session(&store, &session_id)
        .await
        .expect("boot");
    let replay = store
        .read(&session_id, boot.scan_start(), 20)
        .await
        .expect("replay");
    boot.fold_page(&replay).await.expect("boot fold");
    boot_writer
        .finish_boot_session(&session_id, boot)
        .await
        .expect("adopt");
    let boot_text = std::fs::read_to_string(&path).expect("boot view");
    assert!(!boot_text.contains("remove this draft"));
    assert!(boot_text.contains("keep this draft"));
    // The old in-memory cursor must also recognize the late retraction wake.
    writer
        .maintain(&store, &session_id, &facts, 5)
        .await
        .expect("hot retract");
    let hot_text = std::fs::read_to_string(&path).expect("hot view");
    assert!(!hot_text.contains("remove this draft"));
    assert!(hot_text.contains("keep this draft"));
    std::fs::remove_file(&path).expect("discard derived view");
    let cold_writer = PipeNativeWriter::new(root.path());
    cold_writer
        .maintain(&store, &session_id, &[], 5)
        .await
        .expect("cold rebuild");
    let cold_text = std::fs::read_to_string(&path).expect("cold view");
    assert!(!cold_text.contains("remove this draft"));
    assert!(cold_text.contains("keep this draft"));
    assert_eq!(
        store
            .read(&session_id, 0, 20)
            .await
            .expect("raw retained")
            .len(),
        5
    );
    drop(cold_writer);
    drop(boot_writer);
    drop(writer);
    store.close().await.expect("close");
}

/// MUTATION CHECK: changing the 256-envelope threshold in either direction
/// fails at 255 or 256. Exercise the hot renderer directly because the hub's
/// asynchronous head-only notifications use journal reconciliation instead.
#[test]
fn native_pipe_coalesces_255_non_rows_and_covers_the_256th() {
    let session_id = SessionId::new("pipe-hot-coverage-threshold");
    let deltas: Vec<_> = (2..=257)
        .map(|seq| {
            projected_envelope(
                &session_id,
                seq,
                EventPayload::Item(ItemEvent::Delta {
                    item_id: ItemId::new("coverage-item"),
                    delta: haider_protocol::item::ItemDelta::Text {
                        text: "delta".into(),
                    },
                }),
            )
        })
        .collect();
    let cursor = SidecarCursor {
        seq: 1,
        pending_seq: 1,
        generation: 1,
        segment: 0,
    };
    let mut projector = TranscriptProjector::default();
    let (before, cursor) = render_hot_batch(&deltas[..255], 256, cursor, &mut projector)
        .expect("255 non-row deltas render");
    assert!(
        before.is_empty(),
        "255 deltas must not emit coverage: {before}"
    );
    assert_eq!(cursor.seq, 1, "coverage remains at the seed");
    assert_eq!(cursor.pending_seq, 256, "all 255 deltas were processed");

    let (after, cursor) = render_hot_batch(&deltas[255..], 257, cursor, &mut projector)
        .expect("256th non-row delta renders");
    assert_eq!(
        after.lines().count(),
        1,
        "exactly one coverage line: {after}"
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&after).expect("coverage JSON"),
        serde_json::json!({"coverage": 257, "generation": 1})
    );
    assert_eq!(cursor.seq, 257);
    assert_eq!(cursor.pending_seq, 257);
}
