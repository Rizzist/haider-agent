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
