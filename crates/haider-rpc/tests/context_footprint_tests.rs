#![allow(clippy::expect_used)]

use haider_protocol::context::{ContextFootprint, ContextFootprintTruth};
use haider_rpc::{SeqRange, SessionReadResult};

fn exact_footprint() -> ContextFootprint {
    ContextFootprint {
        input_tokens: 100,
        output_tokens: 20,
        cached_input_tokens: 30,
        used_tokens: 150,
        context_window: Some(200),
        reserved_output_tokens: 30,
        soft_threshold_tokens: Some(170),
        estimated_turns_to_threshold: Some(1),
        truth: ContextFootprintTruth::Exact,
    }
}

/// MUTATION CHECK: make the read payload require a footprint or omit the
/// additive latest snapshot. Expected runtime failure: the legacy payload no
/// longer decodes, or the current payload loses the exact snapshot on wire.
#[test]
fn session_read_latest_context_footprint_is_additive_and_optional() {
    let legacy = serde_json::json!({
        "session_id": "session-a",
        "range": {"start_seq": 1, "end_seq": 1},
        "head_seq": 9,
        "envelopes": []
    });
    let decoded: SessionReadResult =
        serde_json::from_value(legacy).expect("legacy session.read payload decodes");
    assert_eq!(decoded.latest_context_footprint, None);

    let current = SessionReadResult {
        session_id: haider_protocol::ids::SessionId::new("session-a"),
        range: SeqRange {
            start_seq: 1,
            end_seq: 1,
        },
        head_seq: 9,
        metadata: None,
        latest_context_footprint: Some(exact_footprint()),
        envelopes: Vec::new(),
    };
    let wire = serde_json::to_value(&current).expect("current session.read payload serializes");
    assert_eq!(wire["latest_context_footprint"]["truth"], "exact");
    assert_eq!(wire["latest_context_footprint"]["used_tokens"], 150);
    assert_eq!(
        serde_json::from_value::<SessionReadResult>(wire)
            .expect("current session.read payload decodes")
            .latest_context_footprint,
        Some(exact_footprint())
    );
}
