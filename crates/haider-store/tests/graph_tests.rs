//! Convergence Graph M1 durability and reduction laws.

#![allow(clippy::expect_used, clippy::panic)]

use haider_protocol::EventPayload;
use haider_protocol::effect::{EffectClass, EffectIntent, EffectOutcome, EffectPhase};
use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
use haider_protocol::error::ErrorCode;
use haider_protocol::graph::{
    EvidenceAuthority, EvidenceVerdict, GraphAttemptOpened, GraphBlockReason, GraphGateKind,
    GraphNodeName, GraphPhase, GraphPinned, ProcessSignalRecorded, ProcessSignalRef,
    SHIP_LOOP_TEMPLATE, STAGGERED_TEMPLATE, SUPER_SHIP_LOOP_TEMPLATE, SubjectSelector,
    evidence_fingerprint, graph_template, graph_template_catalog, process_signal_subject_digest,
    ship_loop_nodes,
};
use haider_protocol::ids::{DeviceId, EffectId, EventId, GraphId, RunId, SessionId};
use haider_protocol::menu::{AnswerVia, MenuAnswer};
use haider_store::{
    EventStore, GraphAbandonCommand, GraphAbandonOutcome, GraphEvidenceCommand,
    GraphEvidenceOutcome, GraphPinCommand, GraphPinOutcome, GraphSwitchCommand, GraphSwitchOutcome,
    MenuResolutionCommand, MenuResolutionOutcome, ProcessSignalCommand, ProcessSignalOutcome,
    SessionCreateCommand, Store,
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
        graph_id: store
            .graph_status(session_id)
            .expect("graph status")
            .expect("active graph")
            .graph_id,
        node,
        verdict,
        detail: detail.into(),
        slot: None,
        subject_digest: None,
        signal: None,
        device_id: DeviceId::new("graph-test"),
    }
}

fn raw_envelope(
    store: &Store,
    session_id: &SessionId,
    run_id: &RunId,
    event_id: impl Into<String>,
    payload: EventPayload,
) -> haider_protocol::envelope::RawEnvelope {
    EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(event_id),
        seq: 0,
        session_id: session_id.clone(),
        branch_id: None,
        run_id: Some(run_id.clone()),
        agent_id: None,
        device_id: DeviceId::new("graph-test"),
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
        payload: serde_json::to_value(payload).expect("serialize test payload"),
    }
}

fn process_signal_command(
    store: &Store,
    session_id: &SessionId,
    serial: usize,
    exit_code: i32,
    transcript: &str,
) -> (ProcessSignalCommand, ProcessSignalRef, String) {
    process_signal_command_for_args(
        store,
        session_id,
        serial,
        exit_code,
        transcript,
        &format!("command-{serial}"),
    )
}

fn process_signal_command_for_args(
    store: &Store,
    session_id: &SessionId,
    serial: usize,
    exit_code: i32,
    transcript: &str,
    command_args: &str,
) -> (ProcessSignalCommand, ProcessSignalRef, String) {
    let run_id = RunId::new(format!("run-{serial}"));
    let call_id = format!("process-{serial}");
    let effect_id = EffectId::new(format!("effect-{serial}"));
    let command_arg_digest = format!("blake3:{}", blake3::hash(command_args.as_bytes()).to_hex());
    let transcript_digest = format!("blake3:{}", blake3::hash(transcript.as_bytes()).to_hex());
    let subject_digest =
        process_signal_subject_digest(&command_arg_digest, &transcript_digest, None);
    let intent = EffectIntent {
        effect: effect_id.clone(),
        class: EffectClass::ProcessExec,
        summary: format!("run test command {serial}"),
        args_digest: command_arg_digest.clone(),
        workspace_revision: None,
    };
    let outcome = if exit_code == 0 {
        EffectOutcome::Ok
    } else {
        EffectOutcome::Failed {
            error: format!("exit {exit_code}"),
        }
    };
    let mut provenance = vec![
        raw_envelope(
            store,
            session_id,
            &run_id,
            format!("intent-{serial}"),
            EventPayload::Effect(EffectPhase::Intent(intent)),
        ),
        raw_envelope(
            store,
            session_id,
            &run_id,
            format!("outcome-{serial}"),
            EventPayload::Effect(EffectPhase::Outcome {
                effect: effect_id.clone(),
                outcome,
                freshness: None,
            }),
        ),
    ];
    store
        .append(&mut provenance)
        .expect("append process effect provenance");
    let signal_ref = ProcessSignalRef {
        run_id: run_id.clone(),
        call_id: call_id.clone(),
        effect_id: effect_id.clone(),
    };
    (
        ProcessSignalCommand {
            session_id: session_id.clone(),
            worker_generation: store.worker_generation(),
            branch_id: None,
            signal: ProcessSignalRecorded {
                run_id,
                call_id,
                effect_id,
                command_arg_digest,
                exit_code: Some(exit_code),
                transcript_digest,
                workspace_revision: None,
                subject_digest: subject_digest.clone(),
                artifact: None,
            },
            device_id: DeviceId::new("graph-test"),
        },
        signal_ref,
        subject_digest,
    )
}

fn attach_verify_signal(
    store: &Store,
    session_id: &SessionId,
    serial: usize,
    verdict: EvidenceVerdict,
    detail: &str,
    command: &mut GraphEvidenceCommand,
) -> ProcessSignalCommand {
    let exit_code = i32::from(verdict == EvidenceVerdict::Red);
    let (signal_command, signal_ref, subject_digest) =
        process_signal_command(store, session_id, serial, exit_code, detail);
    let outcome = store
        .record_process_signal(&signal_command)
        .expect("record process signal");
    assert!(matches!(outcome, ProcessSignalOutcome::Committed { .. }));
    command.slot = Some(["tests", "lint", "typecheck"][(serial + 1) % 3].into());
    command.subject_digest = Some(subject_digest);
    command.signal = Some(signal_ref);
    signal_command
}

fn record(
    store: &Store,
    session_id: &SessionId,
    serial: usize,
    node: GraphNodeName,
    verdict: EvidenceVerdict,
    detail: &str,
) -> GraphEvidenceOutcome {
    let mut command = evidence_command(store, session_id, serial, node.clone(), verdict, detail);
    if node == haider_protocol::graph::verify_node() {
        attach_verify_signal(store, session_id, serial, verdict, detail, &mut command);
    }
    store
        .record_graph_evidence(&command)
        .expect("record evidence")
}

fn record_verify_slot(
    store: &Store,
    session_id: &SessionId,
    serial: usize,
    slot: &str,
    verdict: EvidenceVerdict,
    detail: &str,
) -> GraphEvidenceOutcome {
    let mut command = evidence_command(
        store,
        session_id,
        serial,
        haider_protocol::graph::verify_node(),
        verdict,
        detail,
    );
    attach_verify_signal(store, session_id, serial, verdict, detail, &mut command);
    command.slot = Some(slot.into());
    store
        .record_graph_evidence(&command)
        .expect("record slotted verify evidence")
}

fn advance_to_verify(store: &Store, session_id: &SessionId, serial: usize) {
    record(
        store,
        session_id,
        serial,
        haider_protocol::graph::build_node(),
        EvidenceVerdict::Green,
        "build command passed",
    );
    let status = store
        .graph_status(session_id)
        .expect("status")
        .expect("graph");
    assert_eq!(
        status.current_node,
        Some(haider_protocol::graph::verify_node())
    );
}

fn exhaust_verify_epoch(store: &Store, session_id: &SessionId, serial: &mut usize, epoch: u32) {
    for round in 0..8 {
        record(
            store,
            session_id,
            *serial,
            haider_protocol::graph::verify_node(),
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
            haider_protocol::graph::verify_node(),
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
        haider_protocol::graph::build_node(),
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
        haider_protocol::graph::build_node(),
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
    assert_eq!(
        status.current_node,
        Some(haider_protocol::graph::verify_node())
    );
    assert_eq!(status.attempt, 1);
}

#[test]
fn all_of_three_requires_each_declared_slot_green() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("open store");
    let session_id = create_session(&store, "all-of-three");
    pin(&store, &session_id, "all-of-three");
    advance_to_verify(&store, &session_id, 1);
    record(
        &store,
        &session_id,
        2,
        haider_protocol::graph::verify_node(),
        EvidenceVerdict::Green,
        "test a green",
    );
    record(
        &store,
        &session_id,
        3,
        haider_protocol::graph::verify_node(),
        EvidenceVerdict::Green,
        "test b green",
    );
    record(
        &store,
        &session_id,
        4,
        haider_protocol::graph::verify_node(),
        EvidenceVerdict::Red,
        "test c red",
    );
    for serial in 5..7 {
        record(
            &store,
            &session_id,
            serial,
            haider_protocol::graph::verify_node(),
            EvidenceVerdict::Green,
            "retest green",
        );
    }
    let open = store
        .graph_status(&session_id)
        .expect("status")
        .expect("graph");
    assert_eq!(
        open.current_node,
        Some(haider_protocol::graph::verify_node())
    );
    let verify = open
        .nodes
        .iter()
        .find(|node| node.node == haider_protocol::graph::verify_node())
        .expect("verify");
    assert_eq!(verify.evidence.green, 4);
    assert_eq!(verify.evidence.red, 1);
    assert_eq!(verify.evidence.effective_green, 2);
    assert_eq!(verify.evidence.standing_red, 1);
    record(
        &store,
        &session_id,
        7,
        haider_protocol::graph::verify_node(),
        EvidenceVerdict::Green,
        "third retest green",
    );
    let ship = store
        .graph_status(&session_id)
        .expect("status")
        .expect("graph");
    assert_eq!(ship.current_node, Some(haider_protocol::graph::ship_node()));
    assert!(ship.pending_menu.is_some());
}

/// M2a LAW 1 — MUTATION CHECK: count raw Green calls instead of replacing a
/// slot frontier. Expected failure: the third `tests` submission advances.
#[test]
fn duplicate_green_attestations_never_fill_distinct_slots() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("open store");
    let session_id = create_session(&store, "distinct-slot-law");
    pin(&store, &session_id, "distinct-slot-law");
    advance_to_verify(&store, &session_id, 1);
    for serial in 2..=4 {
        record_verify_slot(
            &store,
            &session_id,
            serial,
            "tests",
            EvidenceVerdict::Green,
            "tests passed",
        );
    }
    let status = store
        .graph_status(&session_id)
        .expect("status")
        .expect("graph");
    assert_eq!(
        status.current_node,
        Some(haider_protocol::graph::verify_node())
    );
    let verify = status
        .nodes
        .iter()
        .find(|node| node.node == haider_protocol::graph::verify_node())
        .expect("verify");
    assert_eq!(verify.evidence.green, 3, "raw audit count remains visible");
    assert_eq!(verify.evidence.effective_green, 1, "one distinct frontier");

    record_verify_slot(
        &store,
        &session_id,
        5,
        "lint",
        EvidenceVerdict::Green,
        "lint passed",
    );
    record_verify_slot(
        &store,
        &session_id,
        6,
        "typecheck",
        EvidenceVerdict::Green,
        "typecheck passed",
    );
    assert_eq!(
        store
            .graph_status(&session_id)
            .expect("status")
            .expect("graph")
            .current_node,
        Some(haider_protocol::graph::ship_node()),
        "three distinct declared slots satisfy the gate"
    );
}

/// M2a LAW 2 — MUTATION CHECK: append votes instead of replacing the named
/// frontier. Expected failure: Green→Red does not lower effective_green or
/// Red→Green leaves a standing red/second vote.
#[test]
fn resubmitting_a_slot_replaces_its_verdict_both_directions() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("open store");
    let session_id = create_session(&store, "slot-replacement-law");
    pin(&store, &session_id, "slot-replacement-law");
    advance_to_verify(&store, &session_id, 1);
    record_verify_slot(
        &store,
        &session_id,
        2,
        "tests",
        EvidenceVerdict::Green,
        "tests passed",
    );
    record_verify_slot(
        &store,
        &session_id,
        3,
        "tests",
        EvidenceVerdict::Red,
        "tests failed",
    );
    let red = store
        .graph_status(&session_id)
        .expect("status")
        .expect("graph");
    let verify = red
        .nodes
        .iter()
        .find(|node| node.node == haider_protocol::graph::verify_node())
        .expect("verify");
    assert_eq!(verify.evidence.effective_green, 0);
    assert_eq!(verify.evidence.standing_red, 1);
    assert_eq!(
        verify
            .slot_statuses()
            .iter()
            .find(|slot| slot.id == "tests")
            .and_then(|slot| slot.verdict),
        Some(EvidenceVerdict::Red)
    );

    record_verify_slot(
        &store,
        &session_id,
        4,
        "tests",
        EvidenceVerdict::Green,
        "tests passed again",
    );
    let green = store
        .graph_status(&session_id)
        .expect("status")
        .expect("graph");
    let verify = green
        .nodes
        .iter()
        .find(|node| node.node == haider_protocol::graph::verify_node())
        .expect("verify");
    assert_eq!(verify.evidence.effective_green, 1);
    assert_eq!(verify.evidence.standing_red, 0);
    assert_eq!(verify.evidence.green, 2);
    assert_eq!(verify.evidence.red, 1);
}

/// M2a LAW 3 — MUTATION CHECK: trust the model's Green verdict without
/// checking daemon exit truth. Expected failure: evidence commits/advances.
#[test]
fn non_zero_process_exit_claimed_green_is_typed_rejection() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("open store");
    let session_id = create_session(&store, "nonzero-green-law");
    pin(&store, &session_id, "nonzero-green-law");
    advance_to_verify(&store, &session_id, 1);
    let (signal_command, signal_ref, subject_digest) =
        process_signal_command(&store, &session_id, 2, 7, "tests failed");
    store
        .record_process_signal(&signal_command)
        .expect("record failed process signal");
    let head = store.latest_seq(&session_id).expect("head");
    let mut command = evidence_command(
        &store,
        &session_id,
        2,
        haider_protocol::graph::verify_node(),
        EvidenceVerdict::Green,
        "claim tests passed",
    );
    command.slot = Some("tests".into());
    command.subject_digest = Some(subject_digest);
    command.signal = Some(signal_ref);
    let error = store
        .record_graph_evidence(&command)
        .expect_err("non-zero exit cannot prove Green");
    assert_eq!(error.code, ErrorCode::InvalidArgument);
    assert_eq!(
        error
            .details
            .as_ref()
            .and_then(|value| value["kind"].as_str()),
        Some("non_zero_exit_claimed_green")
    );
    assert_eq!(store.latest_seq(&session_id).expect("head"), head);
}

/// M2a LAW 4 — MUTATION CHECK: accept an older signal after a later execution
/// of the same command changed its transcript subject. Expected failure: the
/// old, otherwise self-consistent signal appends evidence.
#[test]
fn stale_process_subject_is_typed_rejection() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("open store");
    let session_id = create_session(&store, "stale-subject-law");
    pin(&store, &session_id, "stale-subject-law");
    advance_to_verify(&store, &session_id, 1);
    let (signal_command, signal_ref, subject_digest) = process_signal_command_for_args(
        &store,
        &session_id,
        2,
        0,
        "tests passed before workspace changed",
        "cargo test",
    );
    store
        .record_process_signal(&signal_command)
        .expect("record process signal");
    let (current_signal, _, _) = process_signal_command_for_args(
        &store,
        &session_id,
        3,
        0,
        "tests passed after workspace changed",
        "cargo test",
    );
    store
        .record_process_signal(&current_signal)
        .expect("record current process subject");
    let mut command = evidence_command(
        &store,
        &session_id,
        2,
        haider_protocol::graph::verify_node(),
        EvidenceVerdict::Green,
        "tests passed",
    );
    command.slot = Some("tests".into());
    command.subject_digest = Some(subject_digest);
    command.signal = Some(signal_ref);
    let error = store
        .record_graph_evidence(&command)
        .expect_err("stale subject must reject");
    assert_eq!(error.code, ErrorCode::RevisionConflict);
    assert_eq!(
        error
            .details
            .as_ref()
            .and_then(|value| value["kind"].as_str()),
        Some("stale_evidence_subject")
    );
}

/// M2a authority contract — MUTATION CHECK: collapse slot/authority/
/// provenance failures into accepted testimony or an untyped daemon error.
#[test]
fn slot_authority_and_signal_provenance_fail_through_typed_errors() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("open store");
    let session_id = create_session(&store, "typed-authority-law");
    pin(&store, &session_id, "typed-authority-law");
    advance_to_verify(&store, &session_id, 1);

    let mut bare = evidence_command(
        &store,
        &session_id,
        2,
        haider_protocol::graph::verify_node(),
        EvidenceVerdict::Green,
        "model says green",
    );
    bare.slot = Some("tests".into());
    let wrong_authority = store
        .record_graph_evidence(&bare)
        .expect_err("daemon slot rejects bare testimony");
    assert_eq!(
        wrong_authority
            .details
            .as_ref()
            .and_then(|value| value["kind"].as_str()),
        Some("wrong_evidence_authority")
    );

    let (signal_command, signal_ref, subject_digest) =
        process_signal_command(&store, &session_id, 3, 0, "lint passed");
    store
        .record_process_signal(&signal_command)
        .expect("record signal");
    let mut unknown = evidence_command(
        &store,
        &session_id,
        3,
        haider_protocol::graph::verify_node(),
        EvidenceVerdict::Green,
        "lint passed",
    );
    unknown.slot = Some("security".into());
    unknown.subject_digest = Some(subject_digest.clone());
    unknown.signal = Some(signal_ref.clone());
    let unknown_slot = store
        .record_graph_evidence(&unknown)
        .expect_err("undeclared slot rejects");
    assert_eq!(
        unknown_slot
            .details
            .as_ref()
            .and_then(|value| value["kind"].as_str()),
        Some("unknown_evidence_slot")
    );

    let mut mismatched = unknown;
    mismatched.command_id = "evidence-mismatched-provenance".into();
    mismatched.request_digest = "evidence-mismatched-provenance-digest".into();
    mismatched.request_json = r#"{"case":"mismatched-provenance"}"#.into();
    mismatched.slot = Some("lint".into());
    mismatched.signal = Some(ProcessSignalRef {
        call_id: "different-process-call".into(),
        ..signal_ref.clone()
    });
    let provenance = store
        .record_graph_evidence(&mismatched)
        .expect_err("mismatched signal provenance rejects");
    assert_eq!(
        provenance
            .details
            .as_ref()
            .and_then(|value| value["kind"].as_str()),
        Some("mismatched_signal_provenance")
    );

    let (mut altered_signal, _, _) =
        process_signal_command_for_args(&store, &session_id, 4, 0, "tests passed", "cargo test");
    altered_signal.signal.command_arg_digest =
        format!("blake3:{}", blake3::hash(b"different command").to_hex());
    altered_signal.signal.subject_digest = process_signal_subject_digest(
        &altered_signal.signal.command_arg_digest,
        &altered_signal.signal.transcript_digest,
        None,
    );
    let altered = store
        .record_process_signal(&altered_signal)
        .expect_err("signal argument digest must match its durable effect intent");
    assert_eq!(
        altered
            .details
            .as_ref()
            .and_then(|value| value["kind"].as_str()),
        Some("mismatched_signal_provenance")
    );

    let mut valid = evidence_command(
        &store,
        &session_id,
        30,
        haider_protocol::graph::verify_node(),
        EvidenceVerdict::Green,
        "tests passed",
    );
    valid.run_id = signal_ref.run_id.clone();
    valid.slot = Some("tests".into());
    valid.subject_digest = Some(subject_digest.clone());
    valid.signal = Some(signal_ref.clone());
    store
        .record_graph_evidence(&valid)
        .expect("first slot may use process signal");
    let mut relabeled = valid;
    relabeled.command_id = "evidence-relabeled-signal".into();
    relabeled.request_digest = "evidence-relabeled-signal-digest".into();
    relabeled.request_json = r#"{"case":"relabeled-signal"}"#.into();
    relabeled.slot = Some("lint".into());
    let relabeled = store
        .record_graph_evidence(&relabeled)
        .expect_err("one process signal cannot prove two slots");
    assert_eq!(
        relabeled
            .details
            .as_ref()
            .and_then(|value| value["kind"].as_str()),
        Some("mismatched_signal_provenance")
    );

    let (same_command_rerun, rerun_ref, rerun_subject) = process_signal_command_for_args(
        &store,
        &session_id,
        5,
        0,
        "same command passed again",
        "command-3",
    );
    store
        .record_process_signal(&same_command_rerun)
        .expect("record same-command rerun");
    let head = store.latest_seq(&session_id).expect("head");
    let mut duplicate_subject = evidence_command(
        &store,
        &session_id,
        31,
        haider_protocol::graph::verify_node(),
        EvidenceVerdict::Green,
        "same command relabeled as lint",
    );
    duplicate_subject.run_id = rerun_ref.run_id.clone();
    duplicate_subject.slot = Some("lint".into());
    duplicate_subject.subject_digest = Some(rerun_subject);
    duplicate_subject.signal = Some(rerun_ref);
    let duplicate_subject = store
        .record_graph_evidence(&duplicate_subject)
        .expect_err("one command subject cannot prove two slots through distinct effects");
    assert_eq!(
        duplicate_subject
            .details
            .as_ref()
            .and_then(|value| value["kind"].as_str()),
        Some("mismatched_signal_provenance")
    );
    assert_eq!(store.latest_seq(&session_id).expect("head"), head);
}

#[test]
fn model_attested_slots_remain_explicit_testimony_in_status() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("open store");
    let session_id = create_session(&store, "model-attested-authority");
    let graph_id = GraphId::new("model-attested-graph");
    let mut nodes = ship_loop_nodes();
    let verify = nodes
        .iter_mut()
        .find(|node| node.name == haider_protocol::graph::verify_node())
        .expect("verify spec");
    for slot in &mut verify.verify_slots {
        slot.authority = EvidenceAuthority::ModelAttested;
        slot.subject_selector = SubjectSelector::Freeform;
    }
    let run_id = RunId::new("model-attested-run");
    let mut opening = vec![
        raw_envelope(
            &store,
            &session_id,
            &run_id,
            "model-attested-pin",
            EventPayload::GraphPinned(GraphPinned {
                graph_id: graph_id.clone(),
                template: "model-attested-test".into(),
                digest: "model-attested-digest".into(),
                template_version: 0,
                start_node: None,
                nodes,
            }),
        ),
        raw_envelope(
            &store,
            &session_id,
            &run_id,
            "model-attested-open",
            EventPayload::GraphAttemptOpened(GraphAttemptOpened {
                graph_id,
                node: haider_protocol::graph::verify_node(),
                attempt: 1,
            }),
        ),
    ];
    store.append(&mut opening).expect("append graph");
    let mut command = evidence_command(
        &store,
        &session_id,
        1,
        haider_protocol::graph::verify_node(),
        EvidenceVerdict::Green,
        "qualitative review passed",
    );
    command.slot = Some("tests".into());
    command.subject_digest = Some("review:current-intent".into());
    let mut wrong_authority = command.clone();
    wrong_authority.command_id = "evidence-model-slot-process-proof".into();
    wrong_authority.request_digest = "evidence-model-slot-process-proof-digest".into();
    wrong_authority.request_json = r#"{"case":"model-slot-process-proof"}"#.into();
    wrong_authority.signal = Some(ProcessSignalRef {
        run_id: RunId::new("unused-process-run"),
        call_id: "unused-process-call".into(),
        effect_id: EffectId::new("unused-process-effect"),
    });
    let error = store
        .record_graph_evidence(&wrong_authority)
        .expect_err("model-attested slot rejects a process proof");
    assert_eq!(
        error
            .details
            .as_ref()
            .and_then(|value| value["kind"].as_str()),
        Some("wrong_evidence_authority")
    );
    store
        .record_graph_evidence(&command)
        .expect("model-attested evidence");
    let status = store
        .graph_status(&session_id)
        .expect("status")
        .expect("graph");
    let slot = status
        .nodes
        .iter()
        .find(|node| node.node == haider_protocol::graph::verify_node())
        .expect("verify")
        .slot_statuses()
        .iter()
        .find(|slot| slot.id == "tests")
        .expect("tests slot");
    assert_eq!(slot.authority, EvidenceAuthority::ModelAttested);
    assert!(matches!(
        &slot.source,
        Some(haider_protocol::graph::GraphEvidenceSource::Model { .. })
    ));
}

/// M2a LAW 7 — MUTATION CHECK: apply slot semantics from current binary
/// defaults to an old pinned AllOfN node. Expected failure: unkeyed legacy
/// Greens are ignored instead of using the exact M1 flat frontier.
#[test]
fn legacy_empty_slot_all_of_n_retains_flat_counter_reduction() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("open store");
    let session_id = create_session(&store, "legacy-flat-law");
    let graph_id = GraphId::new("legacy-flat-graph");
    let mut nodes = ship_loop_nodes();
    for node in &mut nodes {
        node.verify_slots.clear();
    }
    let legacy_run = RunId::new("legacy-journal-run");
    let mut opening = vec![
        raw_envelope(
            &store,
            &session_id,
            &legacy_run,
            "legacy-graph-pinned",
            EventPayload::GraphPinned(GraphPinned {
                graph_id: graph_id.clone(),
                template: SHIP_LOOP_TEMPLATE.into(),
                digest: "legacy-empty-slot-digest".into(),
                template_version: 0,
                start_node: None,
                nodes,
            }),
        ),
        raw_envelope(
            &store,
            &session_id,
            &legacy_run,
            "legacy-verify-opened",
            EventPayload::GraphAttemptOpened(GraphAttemptOpened {
                graph_id,
                node: haider_protocol::graph::verify_node(),
                attempt: 1,
            }),
        ),
    ];
    store.append(&mut opening).expect("append legacy graph");
    for (serial, verdict) in [
        (1, EvidenceVerdict::Green),
        (2, EvidenceVerdict::Green),
        (3, EvidenceVerdict::Red),
        (4, EvidenceVerdict::Green),
        (5, EvidenceVerdict::Green),
    ] {
        store
            .record_graph_evidence(&evidence_command(
                &store,
                &session_id,
                serial,
                haider_protocol::graph::verify_node(),
                verdict,
                "legacy testimony",
            ))
            .expect("legacy evidence");
    }
    let open = store
        .graph_status(&session_id)
        .expect("status")
        .expect("graph");
    let verify = open
        .nodes
        .iter()
        .find(|node| node.node == haider_protocol::graph::verify_node())
        .expect("verify");
    assert_eq!(verify.evidence.effective_green, 2);
    assert_eq!(
        open.current_node,
        Some(haider_protocol::graph::verify_node())
    );
    assert_eq!(
        serde_json::to_string(&open).expect("serialize legacy reduction"),
        r#"{"graph_id":"legacy-flat-graph","template":"ship-loop","digest":"legacy-empty-slot-digest","phase":"active","current_node":"VERIFY","attempt":1,"nodes":[{"node":"BUILD","attempts_opened":0,"current_attempt":null,"evidence":{"green":0,"red":0,"effective_green":0,"standing_red":0},"satisfied":false},{"node":"VERIFY","attempts_opened":1,"current_attempt":1,"evidence":{"green":4,"red":1,"effective_green":2,"standing_red":0},"satisfied":false},{"node":"SHIP","attempts_opened":0,"current_attempt":null,"evidence":{"green":0,"red":0,"effective_green":0,"standing_red":0},"satisfied":false}]}"#,
        "legacy no-slot reduction must retain its pre-M2a serialized bytes"
    );
    store
        .record_graph_evidence(&evidence_command(
            &store,
            &session_id,
            6,
            haider_protocol::graph::verify_node(),
            EvidenceVerdict::Green,
            "legacy testimony",
        ))
        .expect("third post-red legacy Green");
    assert_eq!(
        store
            .graph_status(&session_id)
            .expect("status")
            .expect("graph")
            .current_node,
        Some(haider_protocol::graph::ship_node())
    );
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
                if advanced.from_node == haider_protocol::graph::verify_node()
                    && advanced.to_node == haider_protocol::graph::build_node()
        )
    });
    assert!(
        !backwards_advanced,
        "a retry is never a traversed back-edge"
    );
}

#[test]
/// M2a LAW 6 — MUTATION CHECK: scope no-progress only to one epoch or clear
/// historical Red fingerprints on retry. Expected failure: graph stays active.
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
            haider_protocol::graph::verify_node(),
            EvidenceVerdict::Red,
            &format!("other failure {serial}"),
        );
    }
    record(
        &store,
        &session_id,
        9,
        haider_protocol::graph::verify_node(),
        EvidenceVerdict::Red,
        "same persistent failure",
    );
    advance_to_verify(&store, &session_id, 10);
    record(
        &store,
        &session_id,
        11,
        haider_protocol::graph::verify_node(),
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
            haider_protocol::graph::verify_node(),
            EvidenceVerdict::Red,
            &format!("verify epoch one failure {serial}"),
        );
    }
    record(
        &store,
        &session_id,
        9,
        haider_protocol::graph::verify_node(),
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
            haider_protocol::graph::build_node(),
            EvidenceVerdict::Red,
            &format!("build epoch two failure {serial}"),
        );
    }
    advance_to_verify(&store, &session_id, 18);
    record(
        &store,
        &session_id,
        19,
        haider_protocol::graph::verify_node(),
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
/// M2a LAW 5 — MUTATION CHECK: retain slot frontiers when BUILD opens a new
/// epoch. Expected failure: epoch-one Greens leak into attempt two.
fn stale_verify_greens_from_an_older_build_epoch_never_satisfy() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("open store");
    let session_id = create_session(&store, "stale-green");
    pin(&store, &session_id, "stale-green");
    advance_to_verify(&store, &session_id, 1);
    record_verify_slot(
        &store,
        &session_id,
        2,
        "tests",
        EvidenceVerdict::Green,
        "old green a",
    );
    record_verify_slot(
        &store,
        &session_id,
        3,
        "lint",
        EvidenceVerdict::Green,
        "old green b",
    );
    for serial in 4..10 {
        record_verify_slot(
            &store,
            &session_id,
            serial,
            "typecheck",
            EvidenceVerdict::Red,
            &format!("epoch one failure {serial}"),
        );
    }
    let reopened = store
        .graph_status(&session_id)
        .expect("status")
        .expect("graph");
    assert_eq!(
        reopened.current_node,
        Some(haider_protocol::graph::build_node())
    );
    let stale_verify = reopened
        .nodes
        .iter()
        .find(|node| node.node == haider_protocol::graph::verify_node())
        .expect("verify");
    assert_eq!(stale_verify.evidence.green, 0);
    assert_eq!(stale_verify.evidence.red, 0);
    assert!(
        stale_verify
            .slot_statuses()
            .iter()
            .all(|slot| slot.verdict.is_none()),
        "opening BUILD epoch two must clear every old slot frontier"
    );
    assert!(!stale_verify.satisfied);
    advance_to_verify(&store, &session_id, 10);
    record(
        &store,
        &session_id,
        11,
        haider_protocol::graph::verify_node(),
        EvidenceVerdict::Green,
        "new green only",
    );
    let status = store
        .graph_status(&session_id)
        .expect("status")
        .expect("graph");
    assert_eq!(status.attempt, 2);
    assert_eq!(
        status.current_node,
        Some(haider_protocol::graph::verify_node())
    );
    let verify = status
        .nodes
        .iter()
        .find(|node| node.node == haider_protocol::graph::verify_node())
        .expect("verify");
    assert_eq!(verify.evidence.green, 1);
    assert_eq!(verify.evidence.effective_green, 1);
    assert!(
        !verify.satisfied,
        "epoch-one greens are stale by construction"
    );
}

/// M2a LAW 8 — MUTATION CHECK: append a fresh signal/evidence fact when the
/// caller replays a lost response. Expected failure: either count is two.
#[test]
fn replaying_the_same_signal_and_evidence_is_exactly_once() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("open store");
    let session_id = create_session(&store, "signal-replay-law");
    pin(&store, &session_id, "signal-replay-law");
    advance_to_verify(&store, &session_id, 1);
    let (signal_command, signal_ref, subject_digest) =
        process_signal_command(&store, &session_id, 2, 0, "tests passed");
    let ProcessSignalOutcome::Committed { recorded, .. } = store
        .record_process_signal(&signal_command)
        .expect("record signal")
    else {
        panic!("first signal must commit");
    };
    drop(store);
    let store = Store::open(root.path()).expect("reopen after lost signal response");
    assert_eq!(
        store
            .record_process_signal(&signal_command)
            .expect("signal lost-response replay"),
        ProcessSignalOutcome::IdempotentReplay {
            recorded: recorded.clone()
        }
    );

    let mut command = evidence_command(
        &store,
        &session_id,
        2,
        haider_protocol::graph::verify_node(),
        EvidenceVerdict::Green,
        "tests passed",
    );
    command.slot = Some("tests".into());
    command.subject_digest = Some(subject_digest);
    command.signal = Some(signal_ref);
    let GraphEvidenceOutcome::Committed { recorded, .. } = store
        .record_graph_evidence(&command)
        .expect("record evidence")
    else {
        panic!("first evidence must commit");
    };
    drop(store);
    let store = Store::open(root.path()).expect("reopen after lost evidence response");
    assert_eq!(
        store
            .record_graph_evidence(&command)
            .expect("evidence lost-response replay"),
        GraphEvidenceOutcome::IdempotentReplay {
            recorded: recorded.clone()
        }
    );

    let mut signals = 0;
    let mut evidence = 0;
    for envelope in store.journal_replay(&session_id).expect("journal") {
        match serde_json::from_value::<EventPayload>(envelope.payload) {
            Ok(EventPayload::ProcessSignalRecorded(signal))
                if signal.effect_id == signal_command.signal.effect_id =>
            {
                signals += 1;
            }
            Ok(EventPayload::EvidenceRecorded(fact))
                if fact.source
                    == haider_protocol::graph::GraphEvidenceSource::ProcessSignal {
                        run_id: signal_command.signal.run_id.clone(),
                        call_id: signal_command.signal.call_id.clone(),
                        effect_id: signal_command.signal.effect_id.clone(),
                    } =>
            {
                evidence += 1;
            }
            _ => {}
        }
    }
    assert_eq!(signals, 1);
    assert_eq!(evidence, 1);
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
            haider_protocol::graph::verify_node(),
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

fn pin_named_template(
    store: &Store,
    session_id: &SessionId,
    suffix: &str,
    template: &str,
) -> GraphId {
    let mut command = pin_command(store, session_id, suffix);
    command.template = template.into();
    command.request_json = format!(r#"{{"template":"{template}"}}"#);
    let GraphPinOutcome::Committed { pinned, .. } =
        store.pin_graph(&command).expect("catalog template pins")
    else {
        panic!("fresh catalog pin must commit");
    };
    pinned.graph_id
}

fn switch_command(
    store: &Store,
    session_id: &SessionId,
    old_graph_id: GraphId,
    new_graph_id: GraphId,
    template: &str,
    suffix: &str,
) -> GraphSwitchCommand {
    GraphSwitchCommand {
        command_id: format!("switch-{suffix}"),
        request_digest: format!("switch-digest-{suffix}"),
        request_json: format!(r#"{{"template":"{template}","suffix":"{suffix}"}}"#),
        session_id: session_id.clone(),
        worker_generation: store.worker_generation(),
        old_graph_id,
        new_graph_id,
        template: template.into(),
        device_id: DeviceId::new("graph-test"),
    }
}

#[test]
fn m2b_non_linear_ready_set_is_declaration_ordered() {
    // Mutation guard: iterating a HashMap or opening only one successor would
    // change TESTS,REVIEW into a nondeterministic or incomplete ready set.
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("open store");
    let session_id = create_session(&store, "m2b-ready-order");
    pin_named_template(
        &store,
        &session_id,
        "m2b-ready-order",
        SUPER_SHIP_LOOP_TEMPLATE,
    );
    let GraphEvidenceOutcome::Committed { envelopes, .. } = record(
        &store,
        &session_id,
        10_000,
        GraphNodeName::new("START").expect("node"),
        EvidenceVerdict::Green,
        "start is green",
    ) else {
        panic!("fresh evidence commits");
    };
    let kinds = envelopes
        .iter()
        .filter_map(|envelope| {
            serde_json::from_value::<EventPayload>(envelope.payload.clone()).ok()
        })
        .filter_map(|payload| match payload {
            EventPayload::GraphNodeReadied(readied) => Some(readied.node),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        kinds,
        vec![
            GraphNodeName::new("TESTS").expect("node"),
            GraphNodeName::new("REVIEW").expect("node"),
        ]
    );
    let status = store
        .graph_status(&session_id)
        .expect("status")
        .expect("graph");
    assert_eq!(status.ready_nodes, kinds);
}

#[test]
fn m2b_ship_loop_keeps_legacy_gate_and_advance_facts() {
    // Mutation guard: suppressing GraphAdvanced, swapping either edge, or
    // stamping a wrong graph/attempt breaks these exact legacy payloads.
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("open store");
    let session_id = create_session(&store, "m2b-linear-equivalence");
    let graph_id = pin(&store, &session_id, "m2b-linear-equivalence");
    let GraphEvidenceOutcome::Committed { envelopes, .. } = record(
        &store,
        &session_id,
        10_010,
        haider_protocol::graph::build_node(),
        EvidenceVerdict::Green,
        "build green",
    ) else {
        panic!("fresh evidence commits");
    };
    let payloads = envelopes
        .iter()
        .map(|envelope| {
            serde_json::from_value::<EventPayload>(envelope.payload.clone()).expect("payload")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        payloads[1..],
        [
            EventPayload::GraphGateSatisfied(haider_protocol::graph::GraphGateSatisfied {
                graph_id: graph_id.clone(),
                node: haider_protocol::graph::build_node(),
                attempt: 1,
            }),
            EventPayload::GraphAdvanced(haider_protocol::graph::GraphAdvanced {
                graph_id: graph_id.clone(),
                from_node: haider_protocol::graph::build_node(),
                to_node: haider_protocol::graph::verify_node(),
            }),
            EventPayload::GraphAttemptOpened(GraphAttemptOpened {
                graph_id: graph_id.clone(),
                node: haider_protocol::graph::verify_node(),
                attempt: 1,
            }),
        ]
    );
    assert!(
        !payloads
            .iter()
            .any(|payload| matches!(payload, EventPayload::GraphNodeReadied(_)))
    );

    record(
        &store,
        &session_id,
        10_011,
        haider_protocol::graph::verify_node(),
        EvidenceVerdict::Green,
        "tests green",
    );
    record(
        &store,
        &session_id,
        10_012,
        haider_protocol::graph::verify_node(),
        EvidenceVerdict::Green,
        "lint green",
    );
    let GraphEvidenceOutcome::Committed { envelopes, .. } = record(
        &store,
        &session_id,
        10_013,
        haider_protocol::graph::verify_node(),
        EvidenceVerdict::Green,
        "typecheck green",
    ) else {
        panic!("third VERIFY slot commits");
    };
    let payloads = envelopes
        .iter()
        .map(|envelope| {
            serde_json::from_value::<EventPayload>(envelope.payload.clone()).expect("payload")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        payloads[1..4],
        [
            EventPayload::GraphGateSatisfied(haider_protocol::graph::GraphGateSatisfied {
                graph_id: graph_id.clone(),
                node: haider_protocol::graph::verify_node(),
                attempt: 1,
            }),
            EventPayload::GraphAdvanced(haider_protocol::graph::GraphAdvanced {
                graph_id: graph_id.clone(),
                from_node: haider_protocol::graph::verify_node(),
                to_node: haider_protocol::graph::ship_node(),
            }),
            EventPayload::GraphAttemptOpened(GraphAttemptOpened {
                graph_id: graph_id.clone(),
                node: haider_protocol::graph::ship_node(),
                attempt: 1,
            }),
        ]
    );
    assert!(
        !payloads
            .iter()
            .any(|payload| matches!(payload, EventPayload::GraphNodeReadied(_)))
    );

    let status = store
        .graph_status(&session_id)
        .expect("status")
        .expect("graph");
    let menu_id = status.pending_menu.expect("SHIP menu");
    let request_seq = store
        .journal_replay(&session_id)
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
    let MenuResolutionOutcome::Committed { follow_up, .. } = store
        .resolve_menu(&answer_graph_menu(
            &store,
            &session_id,
            &menu_id,
            request_seq,
            "m2b-linear-confirm",
            0,
            "confirm",
        ))
        .expect("confirm")
    else {
        panic!("fresh confirmation commits");
    };
    let payloads = follow_up
        .iter()
        .map(|envelope| {
            serde_json::from_value::<EventPayload>(envelope.payload.clone()).expect("payload")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        payloads,
        vec![
            EventPayload::GraphGateSatisfied(haider_protocol::graph::GraphGateSatisfied {
                graph_id: graph_id.clone(),
                node: haider_protocol::graph::ship_node(),
                attempt: 1,
            }),
            EventPayload::GraphCompleted(haider_protocol::graph::GraphCompleted { graph_id }),
        ]
    );
}

#[test]
fn m2b_retry_reopens_declared_start_and_clears_parallel_greens() {
    // Mutation guard: resetting only the failing node would leave BACKEND
    // falsely green in the next START epoch.
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("open store");
    let session_id = create_session(&store, "m2b-general-retry");
    pin_named_template(&store, &session_id, "m2b-general-retry", STAGGERED_TEMPLATE);
    record(
        &store,
        &session_id,
        10_020,
        GraphNodeName::new("START").expect("node"),
        EvidenceVerdict::Green,
        "start green",
    );
    record(
        &store,
        &session_id,
        10_021,
        GraphNodeName::new("BACKEND").expect("node"),
        EvidenceVerdict::Green,
        "backend green",
    );
    for offset in 0..8 {
        record(
            &store,
            &session_id,
            10_030 + offset,
            GraphNodeName::new("FRONTEND").expect("node"),
            EvidenceVerdict::Red,
            &format!("frontend red {offset}"),
        );
    }
    let status = store
        .graph_status(&session_id)
        .expect("status")
        .expect("graph");
    assert_eq!(status.attempt, 2);
    assert_eq!(
        status.current_node,
        Some(GraphNodeName::new("START").expect("node"))
    );
    assert!(status.nodes.iter().all(|node| !node.satisfied));
}

#[test]
fn m2b_switch_is_one_batch_closes_menu_and_retains_both_roots() {
    // Mutation guard: committing supersession, menu closure, pin, or START in
    // separate transactions would produce non-contiguous facts or a half root.
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("open store");
    let session_id = create_session(&store, "m2b-switch-atomic");
    let old_graph_id = pin(&store, &session_id, "m2b-switch-old");
    let (menu_id, _) = reach_ship(&store, &session_id, 10_100);
    let new_graph_id = GraphId::new("graph-m2b-switch-new");
    let command = switch_command(
        &store,
        &session_id,
        old_graph_id.clone(),
        new_graph_id.clone(),
        STAGGERED_TEMPLATE,
        "atomic",
    );
    let GraphSwitchOutcome::Committed {
        switched,
        envelopes,
    } = store.switch_graph(&command).expect("switch commits")
    else {
        panic!("fresh switch commits");
    };
    assert_eq!(envelopes.len(), 4);
    assert!(
        envelopes
            .windows(2)
            .all(|pair| pair[1].seq == pair[0].seq + 1)
    );
    assert!(
        envelopes
            .iter()
            .all(|envelope| envelope.committed_at_ms == envelopes[0].committed_at_ms)
    );
    let payloads = envelopes
        .iter()
        .map(|envelope| {
            serde_json::from_value::<EventPayload>(envelope.payload.clone()).expect("payload")
        })
        .collect::<Vec<_>>();
    assert!(matches!(payloads[0], EventPayload::GraphSuperseded(_)));
    assert!(matches!(payloads[1], EventPayload::MenuClosed { ref menu, .. } if menu == &menu_id));
    assert!(matches!(payloads[2], EventPayload::GraphPinned(_)));
    assert!(matches!(payloads[3], EventPayload::GraphAttemptOpened(_)));
    assert_eq!(switched.superseded_seq, envelopes[0].seq);
    let active = store
        .graph_status(&session_id)
        .expect("active status")
        .expect("active graph");
    assert_eq!(active.graph_id, new_graph_id);
    assert_eq!(
        active.current_node,
        Some(GraphNodeName::new("START").expect("node"))
    );
    let old = store
        .graph_status_by_id(&session_id, &old_graph_id)
        .expect("old status")
        .expect("old graph retained");
    assert_eq!(old.phase, GraphPhase::Superseded);
    assert!(
        !store
            .graph_reduction_by_id(&session_id, &old_graph_id)
            .expect("old reduction")
            .expect("old graph retained")
            .evidence
            .is_empty()
    );
    assert!(
        store
            .graph_status_by_id(&session_id, &new_graph_id)
            .expect("new status")
            .is_some()
    );
    let head = store.latest_seq(&session_id).expect("head after switch");
    let GraphSwitchOutcome::IdempotentReplay { switched: replayed } = store
        .switch_graph(&command)
        .expect("lost switch response replay")
    else {
        panic!("a committed switch must replay from its receipt");
    };
    assert_eq!(replayed, switched);
    assert_eq!(
        store.latest_seq(&session_id).expect("head after replay"),
        head,
        "mutation guard: replaying graph.switch must append no second batch"
    );
}

#[test]
fn m2b_late_superseded_evidence_is_typed() {
    // Mutation guard: resolving evidence against latest-only state would
    // silently stamp this old BUILD call onto the replacement graph.
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("open store");
    let session_id = create_session(&store, "m2b-late-evidence");
    let old_graph_id = pin(&store, &session_id, "m2b-late-old");
    let late = evidence_command(
        &store,
        &session_id,
        10_200,
        haider_protocol::graph::build_node(),
        EvidenceVerdict::Green,
        "old build green",
    );
    store
        .switch_graph(&switch_command(
            &store,
            &session_id,
            old_graph_id,
            GraphId::new("graph-m2b-late-new"),
            SUPER_SHIP_LOOP_TEMPLATE,
            "late",
        ))
        .expect("switch");
    let error = store
        .record_graph_evidence(&late)
        .expect_err("old graph evidence rejects");
    assert_eq!(error.code, ErrorCode::GraphNotActive);
    assert_eq!(
        error
            .details
            .as_ref()
            .and_then(|details| details.get("kind"))
            .and_then(serde_json::Value::as_str),
        Some("superseded")
    );
}

#[test]
fn m2b_switch_racing_evidence_has_only_total_ordered_outcomes() {
    // Mutation guard: removing actor/store serialization permits evidence to
    // land on the replacement or leaves active_root pointing at no pin.
    use std::sync::{Arc, Barrier};
    use std::thread;

    let root = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(Store::open(root.path()).expect("open store"));
    let session_id = create_session(&store, "m2b-switch-race");
    let old_graph_id = pin(&store, &session_id, "m2b-race-old");
    let evidence = evidence_command(
        &store,
        &session_id,
        10_300,
        haider_protocol::graph::build_node(),
        EvidenceVerdict::Green,
        "racing old evidence",
    );
    let new_graph_id = GraphId::new("graph-m2b-race-new");
    let switch = switch_command(
        &store,
        &session_id,
        old_graph_id.clone(),
        new_graph_id.clone(),
        STAGGERED_TEMPLATE,
        "race",
    );
    let barrier = Arc::new(Barrier::new(3));
    let evidence_store = Arc::clone(&store);
    let evidence_barrier = Arc::clone(&barrier);
    let evidence_thread = thread::spawn(move || {
        evidence_barrier.wait();
        evidence_store.record_graph_evidence(&evidence)
    });
    let switch_store = Arc::clone(&store);
    let switch_barrier = Arc::clone(&barrier);
    let switch_thread = thread::spawn(move || {
        switch_barrier.wait();
        switch_store.switch_graph(&switch)
    });
    barrier.wait();
    let evidence_result = evidence_thread.join().expect("evidence thread");
    let switch_result = switch_thread
        .join()
        .expect("switch thread")
        .expect("switch always commits");
    assert!(matches!(
        switch_result,
        GraphSwitchOutcome::Committed { .. }
    ));
    match evidence_result {
        Ok(GraphEvidenceOutcome::Committed { recorded, .. }) => {
            assert_eq!(recorded.graph_id, old_graph_id);
        }
        Err(error) => {
            assert_eq!(error.code, ErrorCode::GraphNotActive);
            assert_eq!(
                error
                    .details
                    .as_ref()
                    .and_then(|details| details.get("kind"))
                    .and_then(serde_json::Value::as_str),
                Some("superseded")
            );
        }
        Ok(GraphEvidenceOutcome::IdempotentReplay { .. }) => {
            panic!("fresh racing evidence cannot be a replay");
        }
    }
    assert_eq!(
        store
            .graph_status(&session_id)
            .expect("status")
            .expect("active")
            .graph_id,
        new_graph_id
    );
    assert_eq!(
        store
            .graph_status_by_id(&session_id, &old_graph_id)
            .expect("old query")
            .expect("old retained")
            .phase,
        GraphPhase::Superseded
    );
}

fn record_catalog_green(
    store: &Store,
    session_id: &SessionId,
    node: &GraphNodeName,
    serial: &mut usize,
) {
    let status = store
        .graph_status(session_id)
        .expect("status")
        .expect("graph");
    let template = graph_template(&status.template).expect("catalog template");
    let spec = template
        .nodes
        .iter()
        .find(|spec| &spec.name == node)
        .expect("node spec");
    match &spec.gate {
        GraphGateKind::CommandGreen => {
            record(
                store,
                session_id,
                *serial,
                node.clone(),
                EvidenceVerdict::Green,
                "catalog command green",
            );
            *serial += 1;
        }
        GraphGateKind::AllOfN { .. } => {
            for slot in &spec.verify_slots {
                let mut command = evidence_command(
                    store,
                    session_id,
                    *serial,
                    node.clone(),
                    EvidenceVerdict::Green,
                    "catalog slot green",
                );
                command.slot = Some(slot.id.clone());
                match slot.authority {
                    EvidenceAuthority::DaemonVerified => {
                        let (signal, signal_ref, subject_digest) = process_signal_command(
                            store,
                            session_id,
                            *serial,
                            0,
                            &format!("catalog slot {}", slot.id),
                        );
                        store.record_process_signal(&signal).expect("signal");
                        command.subject_digest = Some(subject_digest);
                        command.signal = Some(signal_ref);
                    }
                    EvidenceAuthority::ModelAttested => {
                        command.subject_digest = Some(format!("attested:{}:{}", node, slot.id));
                    }
                }
                store
                    .record_graph_evidence(&command)
                    .expect("catalog slot evidence");
                *serial += 1;
            }
        }
        GraphGateKind::HumanConfirm => {
            let menu_id = graph_pending_menu_for_node(store, session_id, node);
            let request_seq = store
                .journal_replay(session_id)
                .expect("history")
                .into_iter()
                .find_map(|envelope| {
                    serde_json::from_value::<EventPayload>(envelope.payload)
                        .ok()
                        .and_then(|payload| match payload {
                            EventPayload::MenuOpened(menu) if menu.id == menu_id => {
                                Some(envelope.seq)
                            }
                            _ => None,
                        })
                })
                .expect("menu opening");
            store
                .resolve_menu(&answer_graph_menu(
                    store,
                    session_id,
                    &menu_id,
                    request_seq,
                    &format!("catalog-confirm-{serial}"),
                    0,
                    "confirm",
                ))
                .expect("catalog human confirm");
            *serial += 1;
        }
    }
}

fn graph_pending_menu_for_node(
    store: &Store,
    session_id: &SessionId,
    node: &GraphNodeName,
) -> haider_protocol::ids::MenuId {
    let status = store
        .graph_status(session_id)
        .expect("status")
        .expect("graph");
    store
        .journal_replay(session_id)
        .expect("history")
        .into_iter()
        .rev()
        .find_map(|envelope| {
            serde_json::from_value::<EventPayload>(envelope.payload)
                .ok()
                .and_then(|payload| match payload {
                    EventPayload::MenuOpened(menu)
                        if status.pending_menus.iter().any(|id| id == &menu.id)
                            && matches!(
                                menu.kind,
                                haider_protocol::menu::MenuKind::GraphHumanConfirm {
                                    node: ref menu_node,
                                    ..
                                } if menu_node == node
                            ) =>
                    {
                        Some(menu.id)
                    }
                    _ => None,
                })
        })
        .expect("pending human menu")
}

#[test]
fn m2b_all_catalog_templates_reach_completion_on_green_paths() {
    // Mutation guard: hardcoded name traversal or incomplete catalog wiring
    // leaves at least one validated template unable to finish.
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("open store");
    for (index, template) in graph_template_catalog().into_iter().enumerate() {
        let session_id = create_session(&store, &format!("m2b-catalog-{index}"));
        pin_named_template(
            &store,
            &session_id,
            &format!("m2b-catalog-{index}"),
            &template.name,
        );
        let mut serial = 20_000 + index * 1_000;
        for _ in 0..64 {
            let status = store
                .graph_status(&session_id)
                .expect("status")
                .expect("graph");
            if status.phase == GraphPhase::Completed {
                break;
            }
            let ready = if status.ready_nodes.is_empty() {
                status.current_node.into_iter().collect::<Vec<_>>()
            } else {
                status.ready_nodes
            };
            assert!(
                !ready.is_empty(),
                "{} has no ready obligation",
                template.name
            );
            for node in ready {
                record_catalog_green(&store, &session_id, &node, &mut serial);
            }
        }
        assert_eq!(
            store
                .graph_status(&session_id)
                .expect("status")
                .expect("graph")
                .phase,
            GraphPhase::Completed,
            "{} did not complete",
            template.name
        );
    }
}
