//! v0.0.936 pipe compat marker: an `assistant_commit` text row whose run
//! ALSO committed item events repeats content the item stream carries, so
//! it serializes with `"compat": true` and an item-canonical client drops
//! it unconditionally. Empty turn-start rows are always marked (dropping an
//! empty row loses nothing on any journal); pre-item journals stay
//! unmarked byte-for-byte; user rows are never marked.

#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::envelope::{EventEnvelope, PromptRender, RawEnvelope, RenderTargets};
use haider_protocol::history::{NodeKind, TreeNode};
use haider_protocol::ids::{DeviceId, EventId, ItemId, NodeId, RunId, SessionId};
use haider_protocol::item::{ItemEvent, TurnItem};
use haider_protocol::pipe::TranscriptProjector;
use haider_protocol::verify::VerifyVerdict;

fn envelope(seq: u64, run: &str, payload: serde_json::Value) -> RawEnvelope {
    EventEnvelope {
        schema_version: haider_protocol::envelope::SCHEMA_VERSION,
        event_id: EventId::new(format!("compat-{seq}")),
        seq,
        session_id: SessionId::new("session-compat"),
        branch_id: None,
        run_id: Some(RunId::new(run)),
        agent_id: None,
        device_id: DeviceId::new("compat-test"),
        authority_epoch: 0,
        worker_generation: 1,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: seq,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload,
    }
}

fn assistant_commit(seq: u64, run: &str, text: &str) -> RawEnvelope {
    envelope(
        seq,
        run,
        serde_json::to_value(EventPayload::NodeCommitted(TreeNode {
            node: NodeId::new(format!("node-{seq}")),
            parent: None,
            kind: NodeKind::AssistantCommit {
                text: text.to_owned(),
                verdict: VerifyVerdict::Unverified,
            },
        }))
        .expect("node serializes"),
    )
}

fn item_completed(seq: u64, run: &str, text: &str) -> RawEnvelope {
    envelope(
        seq,
        run,
        serde_json::to_value(EventPayload::Item(ItemEvent::Completed {
            item_id: ItemId::new(format!("item-{seq}")),
            item: TurnItem::AgentMessage {
                text: text.to_owned(),
            },
        }))
        .expect("item serializes"),
    )
}

fn user_turn(seq: u64, run: &str, text: &str) -> RawEnvelope {
    envelope(
        seq,
        run,
        serde_json::to_value(EventPayload::NodeCommitted(TreeNode {
            node: NodeId::new(format!("node-{seq}")),
            parent: None,
            kind: NodeKind::UserTurn {
                text: text.to_owned(),
                attachments: Vec::new(),
            },
        }))
        .expect("node serializes"),
    )
}

fn rows_json(
    projector: &mut TranscriptProjector,
    envelope: &RawEnvelope,
) -> Vec<serde_json::Value> {
    projector
        .push(envelope)
        .into_iter()
        .map(|row| serde_json::to_value(&row).expect("row serializes"))
        .collect()
}

/// MUTATION CHECK (executed): stop populating the projector's item-run set
/// and the duplicate-answer marking fails; drop the `skip_serializing_if`
/// and the pre-item byte-stability half fails.
#[test]
fn item_repeating_and_empty_assistant_rows_are_marked_compat() {
    let mut projector = TranscriptProjector::default();

    // Empty turn-start commit BEFORE the run's items: always-safe, marked.
    let rows = rows_json(&mut projector, &assistant_commit(14, "run-1", ""));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["compat"], true, "empty turn-start row is marked");

    // The item stream carries the answer...
    assert!(rows_json(&mut projector, &item_completed(295, "run-1", "the answer")).is_empty());
    // ...so the assistant_commit repeating it is marked.
    let rows = rows_json(
        &mut projector,
        &assistant_commit(296, "run-1", "the answer"),
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0]["compat"], true,
        "item-repeating answer row is marked"
    );
    assert_eq!(rows[0]["text"], "the answer");

    // User rows are never marked, even in item-speaking runs.
    let rows = rows_json(&mut projector, &user_turn(300, "run-1", "next ask"));
    assert_eq!(rows.len(), 1);
    assert!(
        rows[0].get("compat").is_none(),
        "user rows never carry the marker"
    );
}

#[test]
fn pre_item_journals_keep_their_exact_bytes() {
    let mut projector = TranscriptProjector::default();
    // A legacy run with NO item events: the non-empty assistant row must
    // serialize WITHOUT the marker key at all — byte-for-byte the old shape.
    let rows = rows_json(
        &mut projector,
        &assistant_commit(7, "legacy-run", "old text"),
    );
    assert_eq!(rows.len(), 1);
    assert!(
        rows[0].get("compat").is_none(),
        "pre-item rows must not gain any key: {}",
        rows[0]
    );
    assert_eq!(
        rows[0],
        serde_json::json!({
            "role": "assistant",
            "text": "old text",
            "at_ms": 7,
            "seq": 7,
            "ordinal": 0,
        }),
        "the legacy row shape is byte-identical"
    );
}
