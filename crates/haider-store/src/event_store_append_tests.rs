#![allow(clippy::expect_used)]

use super::*;

fn raw_with_payload(payload: serde_json::Value) -> RawEnvelope {
    EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new("append-decode-error"),
        seq: 0,
        session_id: SessionId::new("append-decode-errors"),
        branch_id: None,
        run_id: Some(RunId::new("append-decode-errors-run")),
        agent_id: None,
        device_id: DeviceId::new("append-decode-errors-device"),
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
        payload,
    }
}

/// MUTATION CHECK: turn the workflow kind gate into fail-open handling or
/// remap its borrowed decode failure. Reserved malformed graph facts must keep
/// the store-corruption code used before append decode coalescing.
#[test]
fn malformed_workflow_payload_keeps_store_corrupt_code() {
    let error = workflow_graph_journal_event(&serde_json::json!({
        "type": "workflow_graph_started"
    }))
    .expect_err("reserved malformed workflow payload is rejected");
    assert_eq!(error.code, ErrorCode::StoreCorrupt);
    assert!(
        error
            .message
            .starts_with("malformed workflow activation journal event:")
    );
}

/// MUTATION CHECK: make the cheap queue kind gate skip its typed decode or
/// remap the failure. The producer-facing invalid-argument code is part of the
/// append contract and differs intentionally from workflow corruption.
#[test]
fn malformed_queue_payload_keeps_invalid_argument_code() {
    let mut envelope = raw_with_payload(serde_json::json!({"type": "queue_changed"}));
    let error =
        stamp_queue_delta(&mut envelope).expect_err("reserved malformed queue payload is rejected");
    assert_eq!(error.code, ErrorCode::InvalidArgument);
    assert!(
        error
            .message
            .starts_with("invalid reserved queue_changed payload:")
    );
}

/// MUTATION CHECK: replace the hook-outbox workflow decode with only a kind
/// gate. The later malformed queue fact then wins, changing the batch's
/// historical StoreCorrupt result to InvalidArgument.
#[test]
fn malformed_workflow_precedes_later_malformed_queue_in_append() {
    let root = tempfile::tempdir().expect("temp profile");
    let store = Store::open(root.path()).expect("store opens");
    let workflow = raw_with_payload(serde_json::json!({
        "type": "workflow_graph_started"
    }));
    let mut queue = raw_with_payload(serde_json::json!({"type": "queue_changed"}));
    queue.event_id = EventId::new("append-decode-error-queue");
    let mut envelopes = [workflow, queue];

    let error = store
        .append(&mut envelopes)
        .expect_err("malformed workflow batch is rejected");

    assert_eq!(error.code, ErrorCode::StoreCorrupt);
    assert_eq!(store.latest_seq(&envelopes[0].session_id).expect("head"), 0);
}

/// MUTATION CHECK: allow an undersized capacity estimate to proceed or map it
/// to a new ad-hoc code. Fork preflight must reuse the existing typed
/// `store_full` contract before the copy transaction can publish anything.
#[test]
fn fork_capacity_preflight_is_typed_store_full() {
    let error = ensure_session_fork_storage_available(1_001, 1_000)
        .expect_err("insufficient capacity must fail the fork preflight");
    assert_eq!(error.code, ErrorCode::StoreFull);
    assert!(error.retryable);
    assert!(error.message.contains("journal and Pipe growth"));
    ensure_session_fork_storage_available(1_000, 1_000)
        .expect("an exact-capacity estimate is admissible");
}
