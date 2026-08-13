//! Convergence Graph M1 durability and reduction laws.

#![allow(clippy::expect_used, clippy::panic)]

use haider_protocol::EventPayload;
use haider_protocol::error::ErrorCode;
use haider_protocol::graph::{
    EvidenceVerdict, GraphBlockReason, GraphNodeName, GraphPhase, SHIP_LOOP_TEMPLATE,
    evidence_fingerprint,
};
use haider_protocol::ids::{DeviceId, EventId, GraphId, RunId, SessionId};
use haider_protocol::menu::{AnswerVia, MenuAnswer};
use haider_store::{
    EventStore, GraphAbandonCommand, GraphAbandonOutcome, GraphEvidenceCommand,
    GraphEvidenceOutcome, GraphPinCommand, GraphPinOutcome, MenuResolutionCommand,
    MenuResolutionOutcome, SessionCreateCommand, Store,
};

fn create_session(store: &Store, name: &str) -> SessionId {
    let session_id = SessionId::new(name);
    store
        .create_session(&SessionCreateCommand {
            command_id: format!("create-{name}"),
            request_digest: format!("create-digest-{name}"),
            request_json: format!(r#"{{"session":"{name}"}}"#),
            session_id: session_id.clone(),
            cwd: "/tmp".into(),
            provider: "fake".into(),
            model: "fake-v1".into(),
            max_tokens: 4096,
            permission_overrides: None,
            effort: None,
            fast: false,
            cache_policy: Default::default(),
            system_prompt_version: "graph-test-v1".into(),
            event_id: EventId::new(format!("created-{name}")),
            device_id: DeviceId::new("graph-test"),
        })
        .expect("create typed session");
    session_id
}

fn pin_command(store: &Store, session_id: &SessionId, suffix: &str) -> GraphPinCommand {
    GraphPinCommand {
        command_id: format!("pin-{suffix}"),
        request_digest: format!("pin-digest-{suffix}"),
        request_json: format!(r#"{{"template":"ship-loop","suffix":"{suffix}"}}"#),
        session_id: session_id.clone(),
        worker_generation: store.worker_generation(),
        graph_id: GraphId::new(format!("graph-{suffix}")),
        template: SHIP_LOOP_TEMPLATE.into(),
        device_id: DeviceId::new("graph-test"),
    }
}

fn pin(store: &Store, session_id: &SessionId, suffix: &str) -> GraphId {
    let GraphPinOutcome::Committed { pinned, .. } = store
        .pin_graph(&pin_command(store, session_id, suffix))
        .expect("pin graph")
    else {
        panic!("fresh pin must commit");
    };
    pinned.graph_id
}

fn evidence_command(
    store: &Store,
    session_id: &SessionId,
    serial: usize,
    node: GraphNodeName,
    verdict: EvidenceVerdict,
    detail: &str,
) -> GraphEvidenceCommand {
    GraphEvidenceCommand {
        command_id: format!("evidence-{serial}"),
        request_digest: format!("evidence-digest-{serial}"),
        request_json: format!(r#"{{"serial":{serial}}}"#),
        session_id: session_id.clone(),
        worker_generation: store.worker_generation(),
        run_id: RunId::new(format!("run-{serial}")),
        call_id: format!("call-{serial}"),
        node,
        verdict,
        detail: detail.into(),
        device_id: DeviceId::new("graph-test"),
    }
}

fn record(
    store: &Store,
    session_id: &SessionId,
    serial: usize,
    node: GraphNodeName,
    verdict: EvidenceVerdict,
    detail: &str,
) -> GraphEvidenceOutcome {
    store
        .record_graph_evidence(&evidence_command(
            store, session_id, serial, node, verdict, detail,
        ))
        .expect("record evidence")
}

fn advance_to_verify(store: &Store, session_id: &SessionId, serial: usize) {
    record(
        store,
        session_id,
        serial,
        GraphNodeName::Build,
        EvidenceVerdict::Green,
        "build command passed",
    );
    let status = store
        .graph_status(session_id)
        .expect("status")
        .expect("graph");
    assert_eq!(status.current_node, Some(GraphNodeName::Verify));
}

fn exhaust_verify_epoch(store: &Store, session_id: &SessionId, serial: &mut usize, epoch: u32) {
    for round in 0..8 {
        record(
            store,
            session_id,
            *serial,
            GraphNodeName::Verify,
            EvidenceVerdict::Red,
            &format!("verify failure epoch {epoch} round {round}"),
        );
        *serial += 1;
    }
}

#[test]
fn pin_is_idempotent_and_one_active_graph_is_enforced() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("open store");
    let session_id = create_session(&store, "pin-laws");
    let command = pin_command(&store, &session_id, "one");
    let GraphPinOutcome::Committed { pinned, envelopes } =
        store.pin_graph(&command).expect("first pin")
    else {
        panic!("first pin must commit");
    };
    assert_eq!(envelopes.len(), 2);
    assert_eq!(
        store.pin_graph(&command).expect("lost response replay"),
        GraphPinOutcome::IdempotentReplay {
            pinned: pinned.clone()
        }
    );
    let head = store.latest_seq(&session_id).expect("head");
    let error = store
        .pin_graph(&pin_command(&store, &session_id, "two"))
        .expect_err("second active graph must reject");
    assert_eq!(error.code, ErrorCode::GraphAlreadyActive);
    assert_eq!(store.latest_seq(&session_id).expect("head"), head);
}

#[test]
fn abandon_is_receipt_idempotent_and_exits_the_active_graph() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("open store");
    let session_id = create_session(&store, "abandon-laws");
    let graph_id = pin(&store, &session_id, "abandon-laws");
    let command = GraphAbandonCommand {
        command_id: "abandon-one".into(),
        request_digest: "abandon-digest-one".into(),
        request_json: r#"{"why":"operator stopped"}"#.into(),
        session_id: session_id.clone(),
        worker_generation: store.worker_generation(),
        why: "operator stopped".into(),
        device_id: DeviceId::new("graph-test"),
    };
    let GraphAbandonOutcome::Committed {
        abandoned,
        envelopes,
    } = store.abandon_graph(&command).expect("first abandon")
    else {
        panic!("first abandon must commit");
    };
    assert_eq!(abandoned.graph_id, graph_id);
    assert_eq!(envelopes.len(), 1);
    assert_eq!(
        store.abandon_graph(&command).expect("lost response replay"),
        GraphAbandonOutcome::IdempotentReplay {
            abandoned: abandoned.clone()
        }
    );
    let status = store
        .graph_status(&session_id)
        .expect("status")
        .expect("graph");
    assert_eq!(status.phase, GraphPhase::Abandoned);
    let error = store
        .abandon_graph(&GraphAbandonCommand {
            command_id: "abandon-two".into(),
            request_digest: "abandon-digest-two".into(),
            request_json: r#"{"why":"again"}"#.into(),
            session_id,
            worker_generation: store.worker_generation(),
            why: "again".into(),
            device_id: DeviceId::new("graph-test"),
        })
        .expect_err("completed abandonment is no longer active");
    assert_eq!(error.code, ErrorCode::GraphNotActive);
}

#[test]
fn evidence_validates_open_node_normalizes_detail_and_stamps_fingerprint() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("open store");
    let session_id = create_session(&store, "evidence-validation");
    pin(&store, &session_id, "validation");
    let error = store
        .record_graph_evidence(&evidence_command(
            &store,
            &session_id,
            1,
            GraphNodeName::Verify,
            EvidenceVerdict::Green,
            "wrong node",
        ))
        .expect_err("non-current node rejects");
    assert_eq!(error.code, ErrorCode::GraphWrongNode);

    let GraphEvidenceOutcome::Committed {
        recorded,
        envelopes,
    } = record(
        &store,
        &session_id,
        2,
        GraphNodeName::Build,
        EvidenceVerdict::Red,
        "  cargo   test\n failed  ",
    )
    else {
        panic!("fresh evidence must commit");
    };
    assert_eq!(
        recorded.fingerprint,
        evidence_fingerprint("cargo test failed")
    );
    let EventPayload::EvidenceRecorded(fact) =
        serde_json::from_value(envelopes[0].payload.clone()).expect("decode fact")
    else {
        panic!("first graph-evidence fact must be EvidenceRecorded");
    };
    assert_eq!(fact.detail, "cargo test failed");
    assert_eq!(fact.fingerprint, recorded.fingerprint);
    assert_eq!(fact.attempt, 1);
}

#[test]
fn command_green_advances_build_without_any_model_selected_successor() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("open store");
    let session_id = create_session(&store, "command-green");
    pin(&store, &session_id, "command-green");
    let GraphEvidenceOutcome::Committed { envelopes, .. } = record(
        &store,
        &session_id,
        1,
        GraphNodeName::Build,
        EvidenceVerdict::Green,
        "cargo build passed",
    ) else {
        panic!("fresh evidence must commit");
    };
    let facts = envelopes
        .iter()
        .map(|envelope| serde_json::from_value(envelope.payload.clone()).expect("fact"))
        .collect::<Vec<EventPayload>>();
    assert!(matches!(facts[0], EventPayload::EvidenceRecorded(_)));
    assert!(matches!(facts[1], EventPayload::GraphGateSatisfied(_)));
    assert!(matches!(facts[2], EventPayload::GraphAdvanced(_)));
    assert!(matches!(facts[3], EventPayload::GraphAttemptOpened(_)));
    let status = store
        .graph_status(&session_id)
        .expect("status")
        .expect("graph");
    assert_eq!(status.current_node, Some(GraphNodeName::Verify));
    assert_eq!(status.attempt, 1);
}

#[test]
fn all_of_three_requires_three_greens_after_the_latest_red() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("open store");
    let session_id = create_session(&store, "all-of-three");
    pin(&store, &session_id, "all-of-three");
    advance_to_verify(&store, &session_id, 1);
    record(
        &store,
        &session_id,
        2,
        GraphNodeName::Verify,
        EvidenceVerdict::Green,
        "test a green",
    );
    record(
        &store,
        &session_id,
        3,
        GraphNodeName::Verify,
        EvidenceVerdict::Green,
        "test b green",
    );
    record(
        &store,
        &session_id,
        4,
        GraphNodeName::Verify,
        EvidenceVerdict::Red,
        "test c red",
    );
    for serial in 5..7 {
        record(
            &store,
            &session_id,
            serial,
            GraphNodeName::Verify,
            EvidenceVerdict::Green,
            "retest green",
        );
    }
    let open = store
        .graph_status(&session_id)
        .expect("status")
        .expect("graph");
    assert_eq!(open.current_node, Some(GraphNodeName::Verify));
    let verify = open
        .nodes
        .iter()
        .find(|node| node.node == GraphNodeName::Verify)
        .expect("verify");
    assert_eq!(verify.evidence.green, 4);
    assert_eq!(verify.evidence.red, 1);
    assert_eq!(verify.evidence.effective_green, 2);
    assert_eq!(verify.evidence.standing_red, 0);
    record(
        &store,
        &session_id,
        7,
        GraphNodeName::Verify,
        EvidenceVerdict::Green,
        "third retest green",
    );
    let ship = store
        .graph_status(&session_id)
        .expect("status")
        .expect("graph");
    assert_eq!(ship.current_node, Some(GraphNodeName::Ship));
    assert!(ship.pending_menu.is_some());
}

#[test]
fn eighth_unsatisfied_epoch_blocks_rounds_exhausted_without_back_edge_facts() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("open store");
    let session_id = create_session(&store, "attempt-cap");
    pin(&store, &session_id, "attempt-cap");
    let mut serial = 1;
    for epoch in 1..=8 {
        advance_to_verify(&store, &session_id, serial);
        serial += 1;
        exhaust_verify_epoch(&store, &session_id, &mut serial, epoch);
    }
    let status = store
        .graph_status(&session_id)
        .expect("status")
        .expect("graph");
    assert_eq!(status.phase, GraphPhase::Blocked);
    assert_eq!(status.attempt, 8);
    assert_eq!(
        status.blocked_reason,
        Some(GraphBlockReason::RoundsExhausted)
    );
    let history = store.journal_replay(&session_id).expect("history");
    let backwards_advanced = history.iter().any(|envelope| {
        matches!(
            serde_json::from_value::<EventPayload>(envelope.payload.clone()),
            Ok(EventPayload::GraphAdvanced(advanced))
                if advanced.from_node == GraphNodeName::Verify
                    && advanced.to_node == GraphNodeName::Build
        )
    });
    assert!(
        !backwards_advanced,
        "a retry is never a traversed back-edge"
    );
}

#[test]
fn repeated_red_fingerprint_in_the_next_epoch_blocks_no_progress() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("open store");
    let session_id = create_session(&store, "no-progress");
    pin(&store, &session_id, "no-progress");
    advance_to_verify(&store, &session_id, 1);
    for serial in 2..9 {
        record(
            &store,
            &session_id,
            serial,
            GraphNodeName::Verify,
            EvidenceVerdict::Red,
            &format!("other failure {serial}"),
        );
    }
    record(
        &store,
        &session_id,
        9,
        GraphNodeName::Verify,
        EvidenceVerdict::Red,
        "same persistent failure",
    );
    advance_to_verify(&store, &session_id, 10);
    record(
        &store,
        &session_id,
        11,
        GraphNodeName::Verify,
        EvidenceVerdict::Red,
        "  same   persistent\n failure ",
    );
    let status = store
        .graph_status(&session_id)
        .expect("status")
        .expect("graph");
    assert_eq!(status.phase, GraphPhase::Blocked);
    assert_eq!(status.attempt, 2);
    assert_eq!(status.blocked_reason, Some(GraphBlockReason::NoProgress));
}

#[test]
fn no_progress_uses_the_previous_opening_of_the_same_node_when_epochs_skip_it() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("open store");
    let session_id = create_session(&store, "no-progress-skipped-epoch");
    pin(&store, &session_id, "no-progress-skipped-epoch");

    advance_to_verify(&store, &session_id, 1);
    for serial in 2..9 {
        record(
            &store,
            &session_id,
            serial,
            GraphNodeName::Verify,
            EvidenceVerdict::Red,
            &format!("verify epoch one failure {serial}"),
        );
    }
    record(
        &store,
        &session_id,
        9,
        GraphNodeName::Verify,
        EvidenceVerdict::Red,
        "same verify failure",
    );

    // BUILD epoch two exhausts before VERIFY is opened, so VERIFY's previous
    // opening when epoch three arrives is epoch one, not `attempt - 1`.
    for serial in 10..18 {
        record(
            &store,
            &session_id,
            serial,
            GraphNodeName::Build,
            EvidenceVerdict::Red,
            &format!("build epoch two failure {serial}"),
        );
    }
    advance_to_verify(&store, &session_id, 18);
    record(
        &store,
        &session_id,
        19,
        GraphNodeName::Verify,
        EvidenceVerdict::Red,
        " same   verify\n failure ",
    );
    let status = store
        .graph_status(&session_id)
        .expect("status")
        .expect("graph");
    assert_eq!(status.phase, GraphPhase::Blocked);
    assert_eq!(status.attempt, 3);
    assert_eq!(status.blocked_reason, Some(GraphBlockReason::NoProgress));
}

#[test]
fn stale_verify_greens_from_an_older_build_epoch_never_satisfy() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("open store");
    let session_id = create_session(&store, "stale-green");
    pin(&store, &session_id, "stale-green");
    advance_to_verify(&store, &session_id, 1);
    record(
        &store,
        &session_id,
        2,
        GraphNodeName::Verify,
        EvidenceVerdict::Green,
        "old green a",
    );
    record(
        &store,
        &session_id,
        3,
        GraphNodeName::Verify,
        EvidenceVerdict::Green,
        "old green b",
    );
    for serial in 4..10 {
        record(
            &store,
            &session_id,
            serial,
            GraphNodeName::Verify,
            EvidenceVerdict::Red,
            &format!("epoch one failure {serial}"),
        );
    }
    let reopened = store
        .graph_status(&session_id)
        .expect("status")
        .expect("graph");
    assert_eq!(reopened.current_node, Some(GraphNodeName::Build));
    let stale_verify = reopened
        .nodes
        .iter()
        .find(|node| node.node == GraphNodeName::Verify)
        .expect("verify");
    assert_eq!(stale_verify.evidence.green, 0);
    assert_eq!(stale_verify.evidence.red, 0);
    assert!(!stale_verify.satisfied);
    advance_to_verify(&store, &session_id, 10);
    record(
        &store,
        &session_id,
        11,
        GraphNodeName::Verify,
        EvidenceVerdict::Green,
        "new green only",
    );
    let status = store
        .graph_status(&session_id)
        .expect("status")
        .expect("graph");
    assert_eq!(status.attempt, 2);
    assert_eq!(status.current_node, Some(GraphNodeName::Verify));
    let verify = status
        .nodes
        .iter()
        .find(|node| node.node == GraphNodeName::Verify)
        .expect("verify");
    assert_eq!(verify.evidence.green, 1);
    assert_eq!(verify.evidence.effective_green, 1);
    assert!(
        !verify.satisfied,
        "epoch-one greens are stale by construction"
    );
}

fn reach_ship(
    store: &Store,
    session_id: &SessionId,
    serial: usize,
) -> (haider_protocol::ids::MenuId, u64) {
    advance_to_verify(store, session_id, serial);
    for offset in 1..=3 {
        record(
            store,
            session_id,
            serial + offset,
            GraphNodeName::Verify,
            EvidenceVerdict::Green,
            &format!("verify shard {offset}"),
        );
    }
    let status = store
        .graph_status(session_id)
        .expect("status")
        .expect("graph");
    let menu_id = status.pending_menu.expect("SHIP menu");
    let request_seq = store
        .journal_replay(session_id)
        .expect("history")
        .into_iter()
        .find_map(|envelope| {
            serde_json::from_value::<EventPayload>(envelope.payload)
                .ok()
                .and_then(|payload| match payload {
                    EventPayload::MenuOpened(menu) if menu.id == menu_id => Some(envelope.seq),
                    _ => None,
                })
        })
        .expect("menu opening sequence");
    (menu_id, request_seq)
}

fn answer_graph_menu(
    store: &Store,
    session_id: &SessionId,
    menu_id: &haider_protocol::ids::MenuId,
    request_seq: u64,
    command_id: &str,
    option_index: u32,
    option_key: &str,
) -> MenuResolutionCommand {
    MenuResolutionCommand {
        command_id: command_id.into(),
        session_id: session_id.clone(),
        request_seq,
        worker_generation: store.worker_generation(),
        allow_prior_generation: false,
        answer: MenuAnswer {
            menu: menu_id.clone(),
            option_key: Some(option_key.into()),
            option_index,
            value: None,
            via: AnswerVia::Rpc,
        },
        device_id: DeviceId::new("graph-human"),
        input_is_secret_reference: false,
    }
}

#[test]
fn human_confirm_commits_completion_atomically_and_replays_after_restart() {
    let root = tempfile::tempdir().expect("tempdir");
    let session_id = SessionId::new("human-confirm");
    let (answer, resolution_seq) = {
        let store = Store::open(root.path()).expect("open store");
        create_session(&store, session_id.as_str());
        pin(&store, &session_id, "human-confirm");
        let (menu_id, request_seq) = reach_ship(&store, &session_id, 1);
        let answer = answer_graph_menu(
            &store,
            &session_id,
            &menu_id,
            request_seq,
            "confirm-command",
            0,
            "confirm",
        );
        let MenuResolutionOutcome::Committed {
            envelope,
            follow_up,
            ..
        } = store.resolve_menu(&answer).expect("confirm")
        else {
            panic!("fresh answer must commit");
        };
        assert_eq!(
            follow_up.len(),
            2,
            "gate satisfaction and completion share the answer transaction"
        );
        let status = store
            .graph_status(&session_id)
            .expect("status")
            .expect("graph");
        assert_eq!(status.phase, GraphPhase::Completed);
        assert!(
            status.pending_menu.is_none(),
            "answered-menu projection is cleaned up"
        );
        (answer, envelope.seq)
    };

    let reopened = Store::open(root.path()).expect("reopen after answer commit");
    assert_eq!(
        reopened.resolve_menu(&answer).expect("lost response retry"),
        MenuResolutionOutcome::IdempotentReplay { resolution_seq }
    );
    let status = reopened
        .graph_status(&session_id)
        .expect("status")
        .expect("graph");
    assert_eq!(
        status.phase,
        GraphPhase::Completed,
        "restart needs no actor-side reconciliation"
    );
    assert!(status.pending_menu.is_none());
}

#[test]
fn human_hold_parks_graph_without_completing_it() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("open store");
    let session_id = create_session(&store, "human-hold");
    pin(&store, &session_id, "human-hold");
    let (menu_id, request_seq) = reach_ship(&store, &session_id, 1);
    let answer = answer_graph_menu(
        &store,
        &session_id,
        &menu_id,
        request_seq,
        "hold-command",
        1,
        "hold",
    );
    let MenuResolutionOutcome::Committed { follow_up, .. } =
        store.resolve_menu(&answer).expect("hold")
    else {
        panic!("fresh hold must commit");
    };
    assert_eq!(follow_up.len(), 1);
    let status = store
        .graph_status(&session_id)
        .expect("status")
        .expect("graph");
    assert_eq!(status.phase, GraphPhase::Blocked);
    assert_eq!(status.blocked_reason, Some(GraphBlockReason::HumanHold));
    assert!(status.pending_menu.is_none());
}

#[test]
fn abandoning_ship_closes_its_unanswered_confirmation_menu() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("open store");
    let session_id = create_session(&store, "abandon-ship-menu");
    pin(&store, &session_id, "abandon-ship-menu");
    let (menu_id, request_seq) = reach_ship(&store, &session_id, 1);
    let command = GraphAbandonCommand {
        command_id: "abandon-ship".into(),
        request_digest: "abandon-ship-digest".into(),
        request_json: r#"{"why":"not releasing"}"#.into(),
        session_id: session_id.clone(),
        worker_generation: store.worker_generation(),
        why: "not releasing".into(),
        device_id: DeviceId::new("graph-test"),
    };
    let GraphAbandonOutcome::Committed { envelopes, .. } =
        store.abandon_graph(&command).expect("abandon SHIP")
    else {
        panic!("fresh abandon must commit");
    };
    assert_eq!(envelopes.len(), 2);
    assert!(matches!(
        serde_json::from_value::<EventPayload>(envelopes[1].payload.clone()),
        Ok(EventPayload::MenuClosed { ref menu, .. }) if menu == &menu_id
    ));
    let error = store
        .resolve_menu(&answer_graph_menu(
            &store,
            &session_id,
            &menu_id,
            request_seq,
            "late-confirm-after-abandon",
            0,
            "confirm",
        ))
        .expect_err("abandoned graph menu is durably closed");
    assert_eq!(error.code, ErrorCode::MenuNotFound);
}
