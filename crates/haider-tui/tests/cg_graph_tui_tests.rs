//! Convergence Graph M1 TUI surface pins: the strip glyph vocabulary, the
//! `↺N` re-attempt marker, phase badges, the `/graph` status body, and the
//! transcript note rows. Deterministic fixtures; the plain renderers are the
//! parity authority the styled renderer mirrors.
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::graph::{
    EvidenceRecorded, EvidenceVerdict, GraphAttemptOpened, GraphBlockReason, GraphBlocked,
    GraphCompleted, GraphEvidenceTally, GraphExecutorShape, GraphGateKind, GraphGateSatisfied,
    GraphNodeName, GraphNodeStatus, GraphPhase, GraphPinned, GraphStatus, build_node, ship_node,
    verify_node,
};
use haider_protocol::ids::GraphId;
use haider_tui::graph;
use haider_tui::projection::{SessionProjection, TranscriptEntry};

fn tally(green: u32, red: u32, effective_green: u32) -> GraphEvidenceTally {
    GraphEvidenceTally {
        green,
        red,
        effective_green,
        standing_red: u32::from(red > effective_green),
    }
}

fn node(
    name: GraphNodeName,
    attempts_opened: u32,
    current_attempt: Option<u32>,
    evidence: GraphEvidenceTally,
    satisfied: bool,
) -> GraphNodeStatus {
    let (gate, executor) = match name.as_str() {
        "BUILD" => (GraphGateKind::CommandGreen, GraphExecutorShape::Inline),
        "VERIFY" => (GraphGateKind::AllOfN { n: 3 }, GraphExecutorShape::FanOut),
        _ => (GraphGateKind::HumanConfirm, GraphExecutorShape::Human),
    };
    GraphNodeStatus {
        node: name,
        gate: Some(gate),
        executor: Some(executor),
        attempts_opened,
        current_attempt,
        evidence,
        evidence_slots: Vec::new(),
        satisfied,
    }
}

/// A mid-run reduction: BUILD satisfied on its second attempt, VERIFY current
/// with 2 green / 1 red, SHIP not yet reached.
fn mid_run() -> GraphStatus {
    GraphStatus {
        graph_id: GraphId::new("g1"),
        template: "ship-loop".into(),
        digest: "abcdef0123456789".into(),
        template_version: 1,
        start_node: Some(build_node()),
        phase: GraphPhase::Active,
        current_node: Some(verify_node()),
        ready_nodes: vec![verify_node()],
        attempt: 2,
        nodes: vec![
            node(build_node(), 2, Some(2), tally(1, 0, 1), true),
            node(verify_node(), 1, Some(2), tally(2, 1, 2), false),
            node(ship_node(), 0, None, tally(0, 0, 0), false),
        ],
        blocked_reason: None,
        pending_menu: None,
        pending_menus: Vec::new(),
        run_set: None,
    }
}

#[test]
fn strip_glyph_vocabulary_maps_node_state() {
    let status = mid_run();
    // satisfied → ✓, current active → ◉, ahead → ◌.
    assert_eq!(graph::node_glyph(&status, &status.nodes[0]), "✓");
    assert_eq!(graph::node_glyph(&status, &status.nodes[1]), "◉");
    assert_eq!(graph::node_glyph(&status, &status.nodes[2]), "◌");

    let strip = graph::plain_strip(&status);
    assert!(strip.starts_with("⚑ ship-loop"), "strip: {strip}");
    assert!(strip.contains("BUILD ✓"), "strip: {strip}");
    assert!(strip.contains("VERIFY ◉"), "strip: {strip}");
    assert!(strip.contains("SHIP ◌"), "strip: {strip}");
}

#[test]
fn reattempt_marker_shows_only_past_the_first_attempt() {
    let status = mid_run();
    // BUILD was opened twice → ↺2; VERIFY once → no marker.
    assert_eq!(graph::attempt_marker(&status.nodes[0]), "↺2");
    assert_eq!(graph::attempt_marker(&status.nodes[1]), "");
    assert!(
        graph::plain_strip(&status).contains("BUILD ✓↺2"),
        "{}",
        graph::plain_strip(&status)
    );
}

#[test]
fn blocked_and_hold_glyphs_differ_from_active() {
    let mut rounds = mid_run();
    rounds.phase = GraphPhase::Blocked;
    rounds.blocked_reason = Some(GraphBlockReason::RoundsExhausted);
    assert_eq!(graph::node_glyph(&rounds, &rounds.nodes[1]), "✗");
    assert!(graph::phase_badge(&rounds).contains("attempts exhausted"));

    let mut hold = mid_run();
    hold.phase = GraphPhase::Blocked;
    hold.blocked_reason = Some(GraphBlockReason::HumanHold);
    hold.current_node = Some(ship_node());
    // The SHIP node is current under a human hold → ⏸, not ✗.
    assert_eq!(graph::node_glyph(&hold, &hold.nodes[2]), "⏸");
}

#[test]
fn completed_graph_paints_every_node_satisfied() {
    let mut status = mid_run();
    status.phase = GraphPhase::Completed;
    status.current_node = None;
    for node in &status.nodes {
        assert_eq!(graph::node_glyph(&status, node), "✓");
    }
    assert_eq!(graph::phase_badge(&status), "✓ complete");
}

#[test]
fn status_body_carries_gate_epoch_and_evidence() {
    let body = graph::plain_status(&mid_run());
    assert!(
        body.contains("graph ship-loop · abcdef01 · epoch 2"),
        "{body}"
    );
    assert!(body.contains("BUILD"), "{body}");
    assert!(body.contains("all-of-3"), "{body}"); // VERIFY gate label
    assert!(body.contains("2g/1r (2 eff)"), "{body}"); // VERIFY tally
    assert!(body.contains("→ current: VERIFY"), "{body}");
}

#[test]
fn slotted_verify_never_labels_an_attested_slot_verified() {
    use haider_protocol::graph::{
        EvidenceAuthority, EvidenceVerdict, GraphEvidenceSlotStatus, SubjectSelector,
    };
    let slot = |id: &str,
                authority: EvidenceAuthority,
                verdict: Option<EvidenceVerdict>,
                digest: Option<&str>| GraphEvidenceSlotStatus {
        id: id.into(),
        authority,
        subject_selector: SubjectSelector::Command,
        verdict,
        fingerprint: None,
        subject_digest: digest.map(Into::into),
        source: None,
    };
    let mut status = mid_run();
    let verify = status
        .nodes
        .iter_mut()
        .find(|node| node.node == verify_node())
        .expect("verify node");
    verify.evidence.effective_green = 2;
    verify.evidence_slots = vec![
        slot(
            "tests",
            EvidenceAuthority::DaemonVerified,
            Some(EvidenceVerdict::Green),
            Some("blake3:aabbccddeeff"),
        ),
        slot("lint", EvidenceAuthority::DaemonVerified, None, None),
        slot(
            "review",
            EvidenceAuthority::ModelAttested,
            Some(EvidenceVerdict::Green),
            None,
        ),
    ];
    let body = graph::plain_status(&status);
    // Slotted nodes report the distinct green frontier, not the flat tally.
    assert!(body.contains("2/3 slots"), "{body}");
    // A daemon-verified slot reads `verified`, with a BOUNDED digest.
    assert!(body.contains("tests  verified · blake3:aabbcc"), "{body}");
    assert!(
        !body.contains("aabbccddeeff"),
        "digest stays bounded: {body}"
    );
    // A model-attested slot reads `attested` and is NEVER labelled verified.
    assert!(body.contains("review  attested"), "{body}");
    assert!(!body.contains("review  verified"), "{body}");
    // An unfilled slot is pending.
    assert!(body.contains("lint  pending"), "{body}");
}

#[test]
fn abandoned_graph_status_is_terminal() {
    let mut status = mid_run();
    status.phase = GraphPhase::Abandoned;
    assert_eq!(graph::phase_badge(&status), "✗ abandoned");
    assert!(graph::plain_status(&status).contains("abandoned"));
}

/// M2c: a deferred finalization surfaces an honest, BOUNDED transcript note —
/// the guardrail never silently drops an unfinished graph.
#[test]
fn m2c_finalization_deferred_surfaces_a_bounded_note() {
    let mut projection = SessionProjection::new();
    projection.apply(&EventPayload::GraphFinalizationDeferred(
        haider_protocol::graph::GraphFinalizationDeferred {
            graph_id: GraphId::new("g1"),
            run_id: haider_protocol::ids::RunId::new("r1"),
            state_digest: "blake3:deadbeef".into(),
            provider_requests_consumed: 0,
            unmet_nodes: vec![
                build_node(),
                verify_node(),
                ship_node(),
                GraphNodeName::new("AUDIT").expect("name"),
            ],
        },
    ));
    let note = projection
        .entries()
        .iter()
        .find_map(|entry| match entry {
            TranscriptEntry::Note { text } => Some(text.as_str()),
            _ => None,
        })
        .expect("a deferral note");
    assert!(note.contains("⚠ finalize deferred"), "{note}");
    assert!(note.contains("4 unmet"), "{note}");
    assert!(note.contains("BUILD, VERIFY, SHIP"), "{note}");
    assert!(
        note.contains("+1"),
        "bounds the list to 3 names then +N: {note}"
    );
    assert!(note.contains("keep working or abandon"), "{note}");
}

/// M2d: the per-todo run-set renders an aggregate `N/K todos` line + one child
/// row per todo (glyph · stage · dependency), all off the fetched GraphStatus.
#[test]
fn m2d_run_set_renders_aggregate_and_child_stages() {
    use haider_protocol::graph::{GraphRunSetStatus, TodoGraphStatus};
    use haider_protocol::ids::{EventId, GraphRunSetId, ItemId};
    let child =
        |todo_id: u32, phase: GraphPhase, current: Option<&str>, attempt: u32, dep: Option<u32>| {
            TodoGraphStatus {
                todo_id,
                depends_on_todo_id: dep,
                graph_id: GraphId::new(format!("child-{todo_id}")),
                ordinal: todo_id,
                phase,
                current_node: current.map(|n| GraphNodeName::new(n).expect("name")),
                attempt,
            }
        };
    let mut status = mid_run();
    status.run_set = Some(GraphRunSetStatus {
        run_set_id: GraphRunSetId::new("rs1"),
        root_graph_id: GraphId::new("g1"),
        plan_item_id: ItemId::new("plan1"),
        plan_event_id: EventId::new("ev1"),
        required_children: 3,
        terminal_children: 1,
        children: vec![
            child(1, GraphPhase::Completed, None, 1, None),
            child(2, GraphPhase::Active, Some("VERIFY"), 2, None),
            child(3, GraphPhase::Active, None, 1, Some(2)),
        ],
    });
    let body = graph::plain_status(&status);
    assert!(body.contains("run-set 1/3 todos"), "{body}");
    assert!(body.contains("✓ todo 1 · complete"), "{body}");
    assert!(body.contains("◉ todo 2 · VERIFY attempt 2"), "{body}");
    assert!(body.contains("todo 3 · pending → after todo 2"), "{body}");
}

/// M2b: the `/graph` surface is PROPERTY-based — an arbitrary template with
/// non-ship-loop node names renders off gate kind, never a BUILD/VERIFY/SHIP
/// name match, and `Superseded` reads terminal.
#[test]
fn m2b_status_is_property_based_not_name_based() {
    let mk = |name: &str, gate: GraphGateKind, executor: GraphExecutorShape, satisfied: bool| {
        GraphNodeStatus {
            node: GraphNodeName::new(name).expect("valid node name"),
            gate: Some(gate),
            executor: Some(executor),
            attempts_opened: 1,
            current_attempt: Some(1),
            evidence: tally(0, 0, 0),
            evidence_slots: Vec::new(),
            satisfied,
        }
    };
    let status = GraphStatus {
        graph_id: GraphId::new("g2"),
        template: "sec-audit".into(),
        digest: "0011223344556677".into(),
        template_version: 1,
        start_node: Some(GraphNodeName::new("SCAN").expect("name")),
        phase: GraphPhase::Active,
        current_node: Some(GraphNodeName::new("REVIEW").expect("name")),
        ready_nodes: vec![GraphNodeName::new("REVIEW").expect("name")],
        attempt: 1,
        nodes: vec![
            mk(
                "SCAN",
                GraphGateKind::CommandGreen,
                GraphExecutorShape::Inline,
                true,
            ),
            mk(
                "REVIEW",
                GraphGateKind::AllOfN { n: 5 },
                GraphExecutorShape::FanOut,
                false,
            ),
            mk(
                "APPROVE",
                GraphGateKind::HumanConfirm,
                GraphExecutorShape::Human,
                false,
            ),
        ],
        blocked_reason: None,
        pending_menu: None,
        pending_menus: Vec::new(),
        run_set: None,
    };
    let body = graph::plain_status(&status);
    // Gate labels derive from the gate KIND, not a hardcoded node name.
    assert!(body.contains("SCAN · command-green"), "{body}");
    assert!(body.contains("REVIEW · all-of-5"), "{body}");
    assert!(body.contains("APPROVE · human-confirm"), "{body}");
    // The HumanConfirm node — named APPROVE, not SHIP — suppresses the tally
    // purely by its gate property.
    let approve = body.lines().find(|l| l.contains("APPROVE")).expect("row");
    assert!(
        !approve.contains("eff") && !approve.contains("slots"),
        "human gate carries no tally: {approve}"
    );
    // The current expectation is derived from the gate too.
    assert!(
        body.contains("→ current: REVIEW · record 5 green evidence slots"),
        "{body}"
    );
    // A superseded graph reads terminal.
    let mut superseded = status;
    superseded.phase = GraphPhase::Superseded;
    assert_eq!(graph::phase_badge(&superseded), "⊘ superseded");
    assert!(
        graph::plain_status(&superseded).contains("⊘ superseded — replaced by a newer workflow"),
        "superseded footer"
    );
}

#[test]
fn graph_facts_render_quiet_transcript_notes() {
    let mut projection = SessionProjection::new();
    let gid = GraphId::new("g1");
    projection.apply(&EventPayload::GraphPinned(GraphPinned {
        graph_id: gid.clone(),
        template: "ship-loop".into(),
        digest: "abcdef0123456789".into(),
        template_version: 1,
        start_node: Some(build_node()),
        nodes: Vec::new(),
    }));
    projection.apply(&EventPayload::GraphAttemptOpened(GraphAttemptOpened {
        graph_id: gid.clone(),
        node: build_node(),
        attempt: 2,
    }));
    projection.apply(&EventPayload::EvidenceRecorded(EvidenceRecorded {
        graph_id: gid.clone(),
        node: verify_node(),
        attempt: 2,
        verdict: EvidenceVerdict::Green,
        detail: "cargo test green\n\n  446 passed".into(),
        fingerprint: "fp".into(),
        slot: None,
        subject_digest: None,
        source: haider_protocol::graph::GraphEvidenceSource::Model {
            run_id: haider_protocol::ids::RunId::new("r1"),
            call_id: "c1".into(),
        },
    }));
    projection.apply(&EventPayload::GraphGateSatisfied(GraphGateSatisfied {
        graph_id: gid.clone(),
        node: verify_node(),
        attempt: 2,
    }));
    projection.apply(&EventPayload::GraphBlocked(GraphBlocked {
        graph_id: gid.clone(),
        node: build_node(),
        reason: GraphBlockReason::NoProgress,
    }));
    projection.apply(&EventPayload::GraphCompleted(GraphCompleted {
        graph_id: gid,
    }));

    let notes: Vec<&str> = projection
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            TranscriptEntry::Note { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        notes
            .iter()
            .any(|n| n.contains("ship-loop pinned") && n.contains("abcdef01")),
        "{notes:?}"
    );
    assert!(
        notes
            .iter()
            .any(|n| n.contains("BUILD attempt 2/8") && n.contains("stale")),
        "{notes:?}"
    );
    // Evidence detail is flattened single-line (no embedded newline).
    let evidence = notes
        .iter()
        .find(|n| n.contains("evidence · VERIFY green"))
        .expect("evidence note");
    assert!(!evidence.contains('\n'));
    assert!(
        evidence.contains("cargo test green 446 passed"),
        "{evidence}"
    );
    assert!(
        notes.iter().any(|n| n.contains("VERIFY gate satisfied")),
        "{notes:?}"
    );
    assert!(
        notes
            .iter()
            .any(|n| n.contains("blocked") && n.contains("no progress")),
        "{notes:?}"
    );
    assert!(
        notes.iter().any(|n| n.contains("ship-loop complete")),
        "{notes:?}"
    );
}

#[test]
fn first_build_attempt_and_advances_are_not_noise_rows() {
    // A first-attempt BUILD open (attempt 1) and a plain Advanced fact both
    // stay OFF the transcript — the strip owns forward position.
    let mut projection = SessionProjection::new();
    let gid = GraphId::new("g1");
    projection.apply(&EventPayload::GraphAttemptOpened(GraphAttemptOpened {
        graph_id: gid.clone(),
        node: build_node(),
        attempt: 1,
    }));
    projection.apply(&EventPayload::GraphAdvanced(
        haider_protocol::graph::GraphAdvanced {
            graph_id: gid,
            from_node: build_node(),
            to_node: verify_node(),
        },
    ));
    assert!(
        projection
            .entries()
            .iter()
            .all(|entry| !matches!(entry, TranscriptEntry::Note { .. })),
        "no note rows for attempt-1 open or advance"
    );
}
