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
        payload: payload.into(),
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

/// MUTATION CHECK: let a later effect-unknown state replace the blocking menu
/// that the live reducer acts on first, or classify its resulting cancellation
/// as ordinary cancellation. Either mutation changes the established live
/// terminal bytes.
#[test]
fn first_blocking_cause_survives_later_effect_and_cancelled_terminal() {
    let mut facts = DurableHeadlessRunFacts {
        configured: true,
        request_deadline_unix_ms: Some(20),
        ..DurableHeadlessRunFacts::default()
    };
    record_headless_menu(
        &mut facts,
        &Menu {
            id: MenuId::new("question"),
            kind: MenuKind::Question,
            title: "Need input".into(),
            body: Vec::new(),
            options: Vec::new(),
            blocking: true,
            scope: haider_protocol::menu::MenuScope::Session,
            origin: "test".into(),
            ttl_ms: None,
            timeout_option: None,
        },
    );
    record_headless_run_state(&mut facts, &RunState::EffectOutcomeUnknown, 5);
    record_headless_run_state(&mut facts, &RunState::Cancelling, 10);

    assert_eq!(facts.blocking_error_code, Some("input_required"));
    assert_eq!(facts.cancellation_intent_at_ms, Some(10));
    assert_eq!(
        durable_run_terminal_v1(
            RunState::Cancelled,
            None,
            false,
            false,
            facts.blocking_error_code,
        ),
        Some(haider_protocol::headless::DurableRunTerminalV1 {
            terminal_kind: "failure",
            error_code: Some("input_required"),
        })
    );
}

/// MUTATION CHECK: ignore the permission menu's enumerated choices or its
/// durable close. The live client would report a typed blocker that the
/// journal writer failed to retain.
#[test]
fn permission_blocker_codes_mirror_the_headless_reducer() {
    let mut unavailable = DurableHeadlessRunFacts {
        configured: true,
        ..DurableHeadlessRunFacts::default()
    };
    let mut permission = Menu {
        id: MenuId::new("permission"),
        kind: MenuKind::Permission {
            effect_summary: "write".into(),
        },
        title: "Permission".into(),
        body: Vec::new(),
        options: Vec::new(),
        blocking: true,
        scope: haider_protocol::menu::MenuScope::Session,
        origin: "test".into(),
        ttl_ms: None,
        timeout_option: None,
    };
    record_headless_menu(&mut unavailable, &permission);
    assert_eq!(
        unavailable.blocking_error_code,
        Some("permission_allow_unavailable")
    );

    let mut conflict = DurableHeadlessRunFacts {
        configured: true,
        ..DurableHeadlessRunFacts::default()
    };
    permission.options.push(haider_protocol::menu::MenuOption {
        key: "allow_once".into(),
        label: "Allow".into(),
        detail: None,
        decision: Some(DecisionKind::AllowOnce),
    });
    permission.options.push(haider_protocol::menu::MenuOption {
        key: "reject_once".into(),
        label: "Reject".into(),
        detail: None,
        decision: Some(DecisionKind::RejectOnce),
    });
    record_headless_menu(&mut conflict, &permission);
    record_headless_menu_closed(&mut conflict, &permission.id);
    assert_eq!(
        conflict.blocking_error_code,
        Some("permission_resolution_conflict")
    );
}

/// MUTATION CHECK: infer timeout from terminal commit time. A user interrupt
/// durably requested before the configured deadline may settle afterward and
/// must remain cancellation; a cancellation intent at/after the deadline is
/// the durable timeout discriminator.
#[test]
fn cancellation_intent_time_preserves_interrupt_timeout_precedence() {
    let before = DurableHeadlessRunFacts {
        configured: true,
        request_deadline_unix_ms: Some(20),
        cancellation_intent_at_ms: Some(19),
        ..DurableHeadlessRunFacts::default()
    };
    assert!(!headless_deadline_won(&before));

    let after = DurableHeadlessRunFacts {
        cancellation_intent_at_ms: Some(20),
        ..before.clone()
    };
    assert!(headless_deadline_won(&after));

    let blocked = DurableHeadlessRunFacts {
        blocking_error_code: Some("input_required"),
        ..after
    };
    assert!(!headless_deadline_won(&blocked));
}
