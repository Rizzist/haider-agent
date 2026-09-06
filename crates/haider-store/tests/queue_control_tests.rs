#![allow(clippy::expect_used)]

use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
use haider_protocol::error::ErrorCode;
use haider_protocol::ids::{DeviceId, EventId, RunId, SessionId};
use haider_protocol::queue::QueueChange;
use haider_protocol::state::RunState;
use haider_protocol::{DeliveryMode, EventPayload};
use haider_store::{
    EventStore, QueueConsumeCommand, QueuePromoteCommand, QueueRemoveCommand, SessionCreateCommand,
    Store, TurnAcceptCommand, TurnAcceptOutcome,
};
use std::sync::{Arc, Barrier};

fn create(store: &Store, session_id: &SessionId) {
    store
        .create_session(&SessionCreateCommand {
            command_id: format!("create-{session_id}"),
            request_digest: "create-digest".into(),
            request_json: r#"{"cwd":"/tmp","max_tokens":4096,"model":"fake-v1","provider":"fake"}"#
                .into(),
            session_id: session_id.clone(),
            cwd: "/tmp".into(),
            provider: "fake".into(),
            model: "fake-v1".into(),
            max_tokens: 4096,
            permission_overrides: None,
            effort: None,
            fast: false,
            cache_policy: Default::default(),
            system_prompt_version: "test-system-v1".into(),
            event_id: EventId::new(format!("created-{session_id}")),
            device_id: DeviceId::new("test-daemon"),
        })
        .expect("create session");
}

fn submit(
    store: &Store,
    session_id: &SessionId,
    suffix: &str,
    text: &str,
    mode: DeliveryMode,
) -> haider_store::AcceptedTurn {
    let command = TurnAcceptCommand {
        command_id: format!("submit-{suffix}"),
        request_digest: format!("submit-digest-{suffix}"),
        request_json: format!(r#"{{"mode":"queue","text":{text:?}}}"#),
        session_id: session_id.clone(),
        worker_generation: store.worker_generation(),
        run_id: RunId::new(format!("run-{suffix}")),
        agent_id: None,
        branch_id: None,
        text: text.into(),
        attachments: Vec::new(),
        mode,
        queued_event_id: EventId::new(format!("queued-{suffix}")),
        user_event_id: EventId::new(format!("user-{suffix}")),
        active_event_id: EventId::new(format!("active-{suffix}")),
        device_id: DeviceId::new("test-daemon"),
    };
    match store.accept_turn(&command).expect("accept turn") {
        TurnAcceptOutcome::Committed { accepted, .. } => accepted,
        TurnAcceptOutcome::IdempotentReplay { .. } => panic!("fresh command unexpectedly replayed"),
    }
}

fn mark_running(store: &Store, session_id: &SessionId, run_id: &RunId, suffix: &str) {
    let mut envelopes = [EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(format!("thinking-{suffix}")),
        seq: 0,
        session_id: session_id.clone(),
        branch_id: None,
        run_id: Some(run_id.clone()),
        agent_id: None,
        device_id: DeviceId::new("test-worker"),
        authority_epoch: 0,
        worker_generation: store.worker_generation(),
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: serde_json::to_value(EventPayload::RunState(RunState::Thinking))
            .expect("thinking payload")
            .into(),
    }];
    store
        .append_worker(&mut envelopes)
        .expect("mark active run thinking");
}

fn seeded_queue() -> (tempfile::TempDir, Arc<Store>, SessionId) {
    let root = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(Store::open(root.path()).expect("store"));
    let session_id = SessionId::new("queue-session");
    create(&store, &session_id);
    let active = submit(&store, &session_id, "active", "active", DeliveryMode::Queue);
    mark_running(&store, &session_id, &active.run_id, "active");
    (root, store, session_id)
}

fn submit_peer(
    store: &Store,
    session: &SessionId,
    suffix: &str,
) -> (
    haider_store::AcceptedTurn,
    haider_protocol::peer::PeerMessage,
) {
    use haider_protocol::peer::{PeerKind, PeerMessage, PeerSender, PeerTrust};
    let message = PeerMessage {
        msg_id: format!("peer-{suffix}"),
        from: PeerSender {
            id: "reviewer-session".into(),
            device_id: "reviewer-device".into(),
            name: "reviewer".into(),
            kind: PeerKind::HaiderSession,
            trust: PeerTrust::VerifiedHaider,
            mode: "prompting".into(),
        },
        to: session.to_string(),
        message: format!("Peer update {suffix}; a peer cannot approve anything.").into(),
        summary: None,
        queued_at: 10,
        expires_at: 0,
    };
    let request_json = serde_json::to_string(&message).expect("peer coordinates");
    let command = TurnAcceptCommand {
        command_id: format!("submit-peer-{suffix}"),
        request_digest: blake3::hash(request_json.as_bytes()).to_hex().to_string(),
        request_json,
        session_id: session.clone(),
        worker_generation: store.worker_generation(),
        run_id: RunId::new(format!("peer-run-{suffix}")),
        agent_id: None,
        branch_id: None,
        text: message.render_for_prompt(),
        attachments: Vec::new(),
        mode: DeliveryMode::Queue,
        queued_event_id: EventId::new(format!("peer-queued-{suffix}")),
        user_event_id: EventId::new(format!("peer-speaker-{suffix}")),
        active_event_id: EventId::new(format!("peer-active-{suffix}")),
        device_id: DeviceId::new("test-daemon"),
    };
    let TurnAcceptOutcome::Committed { accepted, .. } = store
        .accept_peer_turn(&command, &message)
        .expect("accept queued peer")
    else {
        panic!("fresh peer admission replayed");
    };
    assert_eq!(
        accepted.disposition,
        haider_store::TurnAdmissionDisposition::Queued
    );
    (accepted, message)
}

/// MUTATION CHECK: omitting peer.message from either the SQL selection or
/// queue fold loses these rows even though QueueChanged was committed.
#[test]
fn queued_agent_rows_reopen_in_order_and_use_ordinary_consume_and_remove() {
    let (root, store, session) = seeded_queue();
    let (first, first_message) = submit_peer(&store, &session, "first");
    submit(
        &store,
        &session,
        "human-between",
        "human between peers",
        DeliveryMode::Queue,
    );
    let (_, last_message) = submit_peer(&store, &session, "last");
    let snapshot = store.queue_snapshot(&session).expect("mixed queue");
    assert_eq!(snapshot.rows.len(), 3);
    assert_eq!(snapshot.rows[0].text, first_message.render_for_prompt());
    assert_eq!(snapshot.rows[1].text, "human between peers");
    assert_eq!(snapshot.rows[2].text, last_message.render_for_prompt());
    assert_eq!(
        snapshot
            .rows
            .iter()
            .map(|row| row.ordinal)
            .collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert!(
        snapshot
            .rows
            .iter()
            .all(|row| row.mode == DeliveryMode::Queue)
    );
    let checkpoint = store
        .session_projection_checkpoint(&session, "run_heads_v1", "session")
        .expect("recovery checkpoint")
        .expect("run heads");
    let projection: serde_json::Value =
        rmp_serde::from_slice(&checkpoint.payload).expect("checkpoint payload");
    let peer_head = projection["heads"]
        .as_array()
        .expect("heads")
        .iter()
        .find(|head| head["run_id"] == first.run_id.as_str())
        .expect("peer recovery head");
    assert_eq!(
        peer_head["accepted_seq"], first.accepted_seq,
        "restart recovery must retain the peer's runnable coordinate"
    );
    drop(store);
    let store = Store::open(root.path()).expect("reopen queued peers");
    assert_eq!(
        store.queue_snapshot(&session).expect("replayed queue"),
        snapshot
    );
    let consume = QueueConsumeCommand {
        session_id: session.clone(),
        run_id: first.run_id,
        delta_event_id: EventId::new("consume-peer-first"),
        device_id: DeviceId::new("worker"),
    };
    let consumed = store
        .queue_consume(&consume)
        .expect("consume peer")
        .expect("held peer");
    assert_eq!(consumed.id, snapshot.rows[0].id);
    assert!(
        store
            .queue_consume(&consume)
            .expect("idempotent consumption")
            .is_none()
    );
    let remaining = store.queue_snapshot(&session).expect("remaining queue");
    assert_eq!(remaining.rows.len(), 2);
    store
        .queue_remove(&QueueRemoveCommand {
            session_id: session.clone(),
            id: snapshot.rows[2].id.clone(),
            revision: remaining.revision,
            cancelling_event_id: EventId::new("remove-peer-last-cancelling"),
            delta_event_id: EventId::new("remove-peer-last-delta"),
            device_id: DeviceId::new("test-daemon"),
        })
        .expect("remove queued peer");
    let remaining = store.queue_snapshot(&session).expect("human remains");
    assert_eq!(remaining.rows.len(), 1);
    assert_eq!(remaining.rows[0].id, snapshot.rows[1].id);
    let journal = store.read(&session, 0, 256).expect("speaker history");
    assert_eq!(journal.iter().filter(|event| matches!(event.payload.decode_event(),
        Ok(EventPayload::NodeCommitted(node)) if matches!(node.kind, haider_protocol::history::NodeKind::Agent { .. })
    )).count(), 2, "consume/remove never rewrite the durable agent speaker");
}

#[test]
fn promoting_queued_agent_preserves_typed_speaker_in_preview_delivery_and_replay() {
    let (root, store, session) = seeded_queue();
    let (_, message) = submit_peer(&store, &session, "promote");
    let snapshot = store.queue_snapshot(&session).expect("held agent");
    let command = QueuePromoteCommand {
        session_id: session.clone(),
        id: snapshot.rows[0].id.clone(),
        revision: snapshot.revision,
        expected_active_run_id: Some(RunId::new("run-active")),
        cancelling_event_id: EventId::new("promote-peer-cancelling"),
        delivery_event_id: EventId::new("promote-peer-delivery"),
        delta_event_id: EventId::new("promote-peer-delta"),
        device_id: DeviceId::new("test-daemon"),
    };
    let preview = store.queue_promote_preview(&command).expect("peer preview");
    assert_eq!(preview.peer_message.as_ref(), Some(&message));
    assert_eq!(preview.text, message.render_for_prompt());
    let promoted = store.queue_promote_steer(&command).expect("promote agent");
    assert_eq!(promoted.peer_message.as_ref(), Some(&message));
    assert_eq!(promoted.active_run_id, RunId::new("run-active"));
    assert!(promoted.envelopes.iter().any(|event| event.seq == promoted.delivery_seq
        && event.run_id.as_ref() == Some(&promoted.active_run_id) && !event.render.ui
        && matches!(event.payload.decode_event(), Ok(EventPayload::PeerMessage(peer)) if peer == message)));
    assert!(
        !promoted.envelopes.iter().any(|event| matches!(
            event.payload.decode_event(),
            Ok(EventPayload::UserMessage { .. } | EventPayload::MenuAnswered(_))
        )),
        "a promoted agent is still neither human input nor an approval"
    );
    assert!(
        store
            .queue_snapshot(&session)
            .expect("promoted queue")
            .rows
            .is_empty()
    );
    drop(store);
    let store = Store::open(root.path()).expect("reopen promotion");
    assert!(
        store
            .queue_snapshot(&session)
            .expect("replayed promotion")
            .rows
            .is_empty()
    );
    let replay = store
        .read(&session, promoted.delivery_seq - 1, 1)
        .expect("replayed speaker");
    assert!(
        matches!(replay[0].payload.decode_event(), Ok(EventPayload::PeerMessage(peer)) if peer == message)
    );
}

#[test]
fn turn_ordinals_are_session_monotonic_and_same_run_steers_keep_identity() {
    let (_root, store, session_id) = seeded_queue();
    let active_run = RunId::new("run-active");
    assert_eq!(
        store
            .turn_ordinal(&session_id, &active_run)
            .expect("active ordinal"),
        Some(1)
    );
    let queued = submit(
        &store,
        &session_id,
        "ordinal-two",
        "second",
        DeliveryMode::Queue,
    );
    assert_eq!(queued.turn_ordinal, 2);

    let command = TurnAcceptCommand {
        command_id: "submit-active-steer".into(),
        request_digest: "submit-active-steer-digest".into(),
        request_json: r#"{"mode":"steer","text":"follow up"}"#.into(),
        session_id: session_id.clone(),
        worker_generation: store.worker_generation(),
        run_id: active_run.clone(),
        agent_id: None,
        branch_id: None,
        text: "follow up".into(),
        attachments: Vec::new(),
        mode: DeliveryMode::Steer,
        queued_event_id: EventId::new("active-steer-queued"),
        user_event_id: EventId::new("active-steer-user"),
        active_event_id: EventId::new("active-steer-active"),
        device_id: DeviceId::new("test-daemon"),
    };
    let steered = match store.accept_turn(&command).expect("accept same-run steer") {
        TurnAcceptOutcome::Committed { accepted, .. } => accepted,
        TurnAcceptOutcome::IdempotentReplay { .. } => panic!("fresh steer replayed"),
    };
    assert_eq!(steered.turn_ordinal, 1);
    assert_eq!(
        steered.disposition,
        haider_store::TurnAdmissionDisposition::SteerPending
    );
    assert_eq!(
        store
            .turn_ordinal(&session_id, &active_run)
            .expect("stable ordinal"),
        Some(1)
    );
    assert_eq!(
        store
            .turn_ordinal(&session_id, &queued.run_id)
            .expect("queued ordinal"),
        Some(2)
    );
}

#[test]
fn queue_rows_keep_stable_ids_and_render_complete_text_across_lists() {
    let (_root, store, session_id) = seeded_queue();
    submit(
        &store,
        &session_id,
        "one",
        "  preserve\nthis exactly  ",
        DeliveryMode::Queue,
    );
    submit(&store, &session_id, "two", "second", DeliveryMode::Steer);

    let first = store.queue_snapshot(&session_id).expect("first list");
    let second = store.queue_snapshot(&session_id).expect("second list");
    assert_eq!(first, second);
    assert_eq!(first.rows.len(), 2);
    assert_eq!(first.rows[0].id, EventId::new("user-one"));
    assert_eq!(first.rows[0].text, "  preserve\nthis exactly  ");
    assert_eq!(first.rows[0].mode, DeliveryMode::Queue);
    assert_eq!(first.rows[0].ordinal, 1);
    assert!(first.rows[0].created_at_ms > 0);
    assert_eq!(first.rows[1].id, EventId::new("user-two"));
    assert_eq!(first.rows[1].mode, DeliveryMode::Steer);
    assert_eq!(first.rows[1].ordinal, 2);
}

#[test]
fn stale_remove_reports_current_revision_and_mutates_nothing() {
    let (_root, store, session_id) = seeded_queue();
    submit(&store, &session_id, "one", "one", DeliveryMode::Queue);
    submit(&store, &session_id, "two", "two", DeliveryMode::Queue);
    let snapshot = store.queue_snapshot(&session_id).expect("list");
    let removed = store
        .queue_remove(&QueueRemoveCommand {
            session_id: session_id.clone(),
            id: EventId::new("user-one"),
            revision: snapshot.revision,
            cancelling_event_id: EventId::new("remove-one-cancelling"),
            delta_event_id: EventId::new("remove-one-delta"),
            device_id: DeviceId::new("test-daemon"),
        })
        .expect("remove first");
    let before_stale = store.latest_seq(&session_id).expect("head");
    let error = store
        .queue_remove(&QueueRemoveCommand {
            session_id: session_id.clone(),
            id: EventId::new("user-two"),
            revision: snapshot.revision,
            cancelling_event_id: EventId::new("stale-cancelling"),
            delta_event_id: EventId::new("stale-delta"),
            device_id: DeviceId::new("test-daemon"),
        })
        .expect_err("stale fence refuses");
    assert_eq!(error.code, ErrorCode::RevisionConflict);
    assert_eq!(
        error.details.as_ref().and_then(|details| details
            .get("current_revision")
            .and_then(serde_json::Value::as_u64)),
        Some(removed.revision)
    );
    assert_eq!(store.latest_seq(&session_id).expect("head"), before_stale);
    let after = store.queue_snapshot(&session_id).expect("list after stale");
    assert_eq!(after.revision, removed.revision);
    assert_eq!(after.rows.len(), 1);
    assert_eq!(after.rows[0].id, EventId::new("user-two"));
}

#[test]
fn two_same_revision_removes_race_to_exactly_one_commit() {
    let (_root, store, session_id) = seeded_queue();
    submit(&store, &session_id, "race", "race", DeliveryMode::Queue);
    let revision = store.queue_snapshot(&session_id).expect("list").revision;
    let barrier = Arc::new(Barrier::new(3));
    let mut threads = Vec::new();
    for suffix in ["a", "b"] {
        let store = Arc::clone(&store);
        let session_id = session_id.clone();
        let barrier = Arc::clone(&barrier);
        threads.push(std::thread::spawn(move || {
            barrier.wait();
            store.queue_remove(&QueueRemoveCommand {
                session_id,
                id: EventId::new("user-race"),
                revision,
                cancelling_event_id: EventId::new(format!("race-cancelling-{suffix}")),
                delta_event_id: EventId::new(format!("race-delta-{suffix}")),
                device_id: DeviceId::new("test-daemon"),
            })
        }));
    }
    barrier.wait();
    let results = threads
        .into_iter()
        .map(|thread| thread.join().expect("remove thread"))
        .collect::<Vec<_>>();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    let loser = results
        .iter()
        .find_map(|result| result.as_ref().err())
        .expect("one loser");
    assert_eq!(loser.code, ErrorCode::RevisionConflict);
    assert!(
        store
            .queue_snapshot(&session_id)
            .expect("final list")
            .rows
            .is_empty()
    );
}

#[test]
fn promote_preserves_text_as_one_durable_steer_and_cannot_double_deliver() {
    let (_root, store, session_id) = seeded_queue();
    let text = "  exact\nsteer text  ";
    submit(&store, &session_id, "promote", text, DeliveryMode::Queue);
    let snapshot = store.queue_snapshot(&session_id).expect("list");
    let command = QueuePromoteCommand {
        session_id: session_id.clone(),
        id: EventId::new("user-promote"),
        revision: snapshot.revision,
        expected_active_run_id: None,
        cancelling_event_id: EventId::new("promote-cancelling"),
        delivery_event_id: EventId::new("promote-delivery"),
        delta_event_id: EventId::new("promote-delta"),
        device_id: DeviceId::new("test-daemon"),
    };
    let promoted = store
        .queue_promote_steer(&command)
        .expect("promote commits");
    assert_eq!(promoted.text, text);
    assert!(
        store
            .queue_snapshot(&session_id)
            .expect("list after promote")
            .rows
            .is_empty()
    );
    let delivery = &promoted.envelopes[1];
    assert!(!delivery.render.ui);
    assert!(matches!(
        serde_json::from_value::<EventPayload>(delivery.payload.clone().into()).expect("delivery payload"),
        EventPayload::UserMessage { text: delivered, mode: DeliveryMode::Steer, .. }
            if delivered == text
    ));
    let error = store
        .queue_promote_steer(&command)
        .expect_err("same snapshot cannot promote twice");
    assert_eq!(error.code, ErrorCode::RevisionConflict);
}

#[test]
fn consumed_row_disappears_with_a_revision_bearing_delta() {
    let (_root, store, session_id) = seeded_queue();
    let queued = submit(
        &store,
        &session_id,
        "consume",
        "consume me",
        DeliveryMode::Queue,
    );
    let consumed = store
        .queue_consume(&QueueConsumeCommand {
            session_id: session_id.clone(),
            run_id: queued.run_id,
            delta_event_id: EventId::new("consume-delta"),
            device_id: DeviceId::new("test-worker"),
        })
        .expect("consume")
        .expect("row was held");
    assert!(consumed.revision > 0);
    assert!(matches!(
        serde_json::from_value::<EventPayload>(consumed.envelope.payload.clone().into())
            .expect("delta payload"),
        EventPayload::QueueChanged(delta)
            if delta.revision == consumed.revision
                && matches!(&delta.change, QueueChange::Consumed { id }
                    if id == &EventId::new("user-consume"))
    ));
    assert!(
        store
            .queue_snapshot(&session_id)
            .expect("next list")
            .rows
            .is_empty()
    );
}

#[test]
fn reserved_queue_changed_payload_without_revision_cannot_commit() {
    let (_root, store, session_id) = seeded_queue();
    let before = store
        .latest_seq(&session_id)
        .expect("head before malformed");
    let mut malformed = [EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new("malformed-queue-delta"),
        seq: 0,
        session_id: session_id.clone(),
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: DeviceId::new("test-daemon"),
        authority_epoch: 0,
        worker_generation: store.worker_generation(),
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: false,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: serde_json::json!({
            "type": "queue_changed",
            "change": {"kind": "removed", "id": "some-row"}
        })
        .into(),
    }];
    let error = store
        .append(&mut malformed)
        .expect_err("reserved payload is fail-closed");
    assert_eq!(error.code, ErrorCode::InvalidArgument);
    assert_eq!(
        store.latest_seq(&session_id).expect("head after malformed"),
        before
    );
}
