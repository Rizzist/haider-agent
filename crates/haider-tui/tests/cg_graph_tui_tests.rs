//! Convergence Graph M1 TUI surface pins: the strip glyph vocabulary, the
//! `↺N` re-attempt marker, phase badges, the `/graph` status body, and the
//! transcript note rows. Deterministic fixtures; the plain renderers are the
//! parity authority the styled renderer mirrors.
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::graph::{
    EvidenceRecorded, EvidenceVerdict, GraphAttemptOpened, GraphBlockReason, GraphBlocked,
    GraphCompleted, GraphEvidenceTally, GraphGateSatisfied, GraphNodeName, GraphNodeStatus,
    GraphPhase, GraphPinned, GraphStatus,
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
    GraphNodeStatus {
        node: name,
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
        phase: GraphPhase::Active,
        current_node: Some(GraphNodeName::Verify),
        attempt: 2,
        nodes: vec![
            node(GraphNodeName::Build, 2, Some(2), tally(1, 0, 1), true),
            node(GraphNodeName::Verify, 1, Some(2), tally(2, 1, 2), false),
            node(GraphNodeName::Ship, 0, None, tally(0, 0, 0), false),
        ],
        blocked_reason: None,
        pending_menu: None,
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
    hold.current_node = Some(GraphNodeName::Ship);
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
        .find(|node| node.node == GraphNodeName::Verify)
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
    assert!(!body.contains("aabbccddeeff"), "digest stays bounded: {body}");
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

#[test]
fn graph_facts_render_quiet_transcript_notes() {
    let mut projection = SessionProjection::new();
    let gid = GraphId::new("g1");
    projection.apply(&EventPayload::GraphPinned(GraphPinned {
        graph_id: gid.clone(),
        template: "ship-loop".into(),
        digest: "abcdef0123456789".into(),
        nodes: Vec::new(),
    }));
    projection.apply(&EventPayload::GraphAttemptOpened(GraphAttemptOpened {
        graph_id: gid.clone(),
        node: GraphNodeName::Build,
        attempt: 2,
    }));
    projection.apply(&EventPayload::EvidenceRecorded(EvidenceRecorded {
        graph_id: gid.clone(),
        node: GraphNodeName::Verify,
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
        node: GraphNodeName::Verify,
        attempt: 2,
    }));
    projection.apply(&EventPayload::GraphBlocked(GraphBlocked {
        graph_id: gid.clone(),
        node: GraphNodeName::Build,
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
        node: GraphNodeName::Build,
        attempt: 1,
    }));
    projection.apply(&EventPayload::GraphAdvanced(
        haider_protocol::graph::GraphAdvanced {
            graph_id: gid,
            from_node: GraphNodeName::Build,
            to_node: GraphNodeName::Verify,
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
