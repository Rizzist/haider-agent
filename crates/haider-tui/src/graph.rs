//! Convergence Graph M1 TUI surfaces: the always-visible strip above the
//! composer and the `/graph` status view. Pure over the daemon's
//! [`GraphStatus`] reduction — nothing here is ever fabricated. The glyph
//! vocabulary and header grammar match the `/tui` mockup's workflow strip.
//!
//! Visual authority: mockup DagStrip — node glyphs, `↺N` re-attempt markers,
//! and the `⚑ ship-loop · ✓a ◉b …` rollup line.

use haider_protocol::graph::{
    EvidenceAuthority, EvidenceVerdict, GraphBlockReason, GraphEvidenceSlotStatus, GraphGateKind,
    GraphNodeStatus, GraphPhase, GraphRunSetStatus, GraphStatus, TodoGraphStatus,
};

/// A child (per-todo) graph's glyph + stage word for the run-set section.
/// M2d: purely property/phase-based, like the node glyphs.
#[must_use]
pub fn child_glyph_stage(child: &TodoGraphStatus) -> (&'static str, String) {
    match child.phase {
        GraphPhase::Completed => ("✓", "complete".to_owned()),
        GraphPhase::Abandoned => ("⊘", "abandoned".to_owned()),
        GraphPhase::Superseded => ("⊘", "superseded".to_owned()),
        GraphPhase::Blocked => ("✗", "blocked".to_owned()),
        GraphPhase::Active => (
            "◉",
            child.current_node.as_ref().map_or_else(
                || "pending".to_owned(),
                |node| format!("{} attempt {}", node.label(), child.attempt),
            ),
        ),
    }
}

/// The M2d per-todo run-set section as plain rows: an aggregate
/// `run-set N/K todos` header + one child row (glyph · todo N · stage [· dep]).
#[must_use]
pub fn plain_run_set(run_set: &GraphRunSetStatus) -> Vec<String> {
    let mut lines = vec![format!(
        "run-set {}/{} todos",
        run_set.terminal_children, run_set.required_children
    )];
    for child in &run_set.children {
        let (glyph, stage) = child_glyph_stage(child);
        let dep = child
            .depends_on_todo_id
            .map_or_else(String::new, |id| format!(" → after todo {id}"));
        lines.push(format!("  {glyph} todo {} · {stage}{dep}", child.todo_id));
    }
    lines
}

/// A node whose gate is a human confirmation — the M2b PROPERTY-based
/// replacement for the old `node == SHIP` name check, with a canonical-name
/// fallback for legacy reductions that predate populated `gate` fields.
#[must_use]
pub fn is_human_gate(node: &GraphNodeStatus) -> bool {
    match node.gate {
        Some(GraphGateKind::HumanConfirm) => true,
        Some(_) => false,
        None => node.node.as_str() == "SHIP",
    }
}

/// Per-node glyph. A satisfied obligation is `✓`; the current node is `◉`
/// while active and `✗`/`⏸` while blocked; an obligation not yet reached is
/// `◌`. Terminal graphs paint every node satisfied.
#[must_use]
pub fn node_glyph(status: &GraphStatus, node: &GraphNodeStatus) -> &'static str {
    if status.phase == GraphPhase::Completed {
        return "✓";
    }
    if node.satisfied {
        return "✓";
    }
    if status.current_node.as_ref() == Some(&node.node) {
        return match (status.phase, status.blocked_reason) {
            (GraphPhase::Blocked, Some(GraphBlockReason::HumanHold)) => "⏸",
            (GraphPhase::Blocked, _) => "✗",
            _ => "◉",
        };
    }
    "◌"
}

/// The `↺N` re-attempt marker for a node opened more than once, else empty.
#[must_use]
pub fn attempt_marker(node: &GraphNodeStatus) -> String {
    if node.attempts_opened > 1 {
        format!("↺{}", node.attempts_opened)
    } else {
        String::new()
    }
}

/// Human-readable block reason for the strip badge / status footer.
#[must_use]
pub fn block_reason_label(reason: GraphBlockReason) -> &'static str {
    match reason {
        GraphBlockReason::RoundsExhausted => "attempts exhausted",
        GraphBlockReason::NoProgress => "no progress",
        GraphBlockReason::HumanHold => "held for review",
    }
}

/// The gate label for a node — derived from its PINNED gate kind (M2b),
/// with a canonical-name fallback for legacy reductions without a gate field.
#[must_use]
pub fn gate_label(node: &GraphNodeStatus) -> String {
    if let Some(gate) = &node.gate {
        return match gate {
            GraphGateKind::CommandGreen => "command-green".to_owned(),
            GraphGateKind::AllOfN { n } => format!("all-of-{n}"),
            GraphGateKind::HumanConfirm => "human-confirm".to_owned(),
        };
    }
    match node.node.as_str() {
        "BUILD" => "command-green".to_owned(),
        "VERIFY" => "all-of-3".to_owned(),
        "SHIP" => "human-confirm".to_owned(),
        other => other.to_owned(),
    }
}

/// The strip's terminal/phase badge, or empty while simply active.
#[must_use]
pub fn phase_badge(status: &GraphStatus) -> String {
    match status.phase {
        GraphPhase::Completed => "✓ complete".to_owned(),
        GraphPhase::Abandoned => "✗ abandoned".to_owned(),
        GraphPhase::Superseded => "⊘ superseded".to_owned(),
        GraphPhase::Blocked => format!(
            "✗ blocked · {}",
            status.blocked_reason.map_or("held", block_reason_label)
        ),
        GraphPhase::Active => String::new(),
    }
}

/// The plain (greppable) strip text — the parity authority for the styled
/// strip. Shape: `⚑ ship-loop  BUILD ✓  VERIFY ◉↺2  SHIP ◌   [badge]`.
#[must_use]
pub fn plain_strip(status: &GraphStatus) -> String {
    let mut out = format!("⚑ {}", status.template);
    for node in &status.nodes {
        out.push_str("  ");
        out.push_str(node.node.label());
        out.push(' ');
        out.push_str(node_glyph(status, node));
        out.push_str(&attempt_marker(node));
    }
    let badge = phase_badge(status);
    if !badge.is_empty() {
        out.push_str("  ");
        out.push_str(&badge);
    }
    out
}

/// The `/graph` status view as plain text — the parity authority for the
/// styled body. One header line, one row per node, then the current-node
/// expectation (or the block/terminal footer).
#[must_use]
pub fn plain_status(status: &GraphStatus) -> String {
    let mut lines = Vec::new();
    lines.push(format!(
        "graph {} · {} · epoch {}",
        status.template,
        digest_short(&status.digest),
        status.attempt
    ));
    for node in &status.nodes {
        let glyph = node_glyph(status, node);
        let marker = attempt_marker(node);
        let evidence = node_evidence_fragment(node);
        lines.push(format!(
            "  {glyph}{marker} {} · {} · attempt {}/8{evidence}",
            node.node.label(),
            gate_label(node),
            node.current_attempt.unwrap_or(0),
        ));
        // M2a: one indented provenance row per declared evidence slot.
        for slot in &node.evidence_slots {
            lines.push(format!("      {}", slot_row(slot)));
        }
    }
    // M2d: the per-todo run-set (K child graphs), when this turn drives one.
    if let Some(run_set) = &status.run_set {
        for line in plain_run_set(run_set) {
            lines.push(format!("  {line}"));
        }
    }
    match status.phase {
        GraphPhase::Completed => lines.push("✓ complete — every gate satisfied".to_owned()),
        GraphPhase::Abandoned => lines.push("✗ abandoned".to_owned()),
        GraphPhase::Superseded => {
            lines.push("⊘ superseded — replaced by a newer workflow".to_owned());
        }
        GraphPhase::Blocked => lines.push(format!(
            "✗ blocked — {} · /graph abandon then re-pin to retry",
            status.blocked_reason.map_or("held", block_reason_label)
        )),
        GraphPhase::Active => {
            if let Some(current) = status.current_node.as_ref() {
                let expect = status
                    .nodes
                    .iter()
                    .find(|status_node| &status_node.node == current)
                    .map_or_else(|| format!("advance {current}"), expectation);
                lines.push(format!("→ current: {} · {expect}", current.label()));
            }
        }
    }
    lines.join("\n")
}

/// The current-node expectation — derived from its PINNED gate kind (M2b),
/// with a canonical-name fallback for legacy reductions without a gate field.
#[must_use]
pub fn expectation(node: &GraphNodeStatus) -> String {
    match node.gate.as_ref() {
        Some(GraphGateKind::CommandGreen) => "record command-green evidence".to_owned(),
        Some(GraphGateKind::AllOfN { n }) => format!("record {n} green evidence slots"),
        Some(GraphGateKind::HumanConfirm) => "confirm the human gate".to_owned(),
        None => match node.node.as_str() {
            "BUILD" => "record BUILD evidence (command green)".to_owned(),
            "VERIFY" => "record 3 green VERIFY results".to_owned(),
            "SHIP" => "confirm the SHIP gate".to_owned(),
            other => format!("advance {other}"),
        },
    }
}

/// The per-node evidence summary fragment. Human gates carry none; slot-aware
/// nodes (M2a) report the distinct green frontier over their declared slots;
/// legacy slot-less nodes keep the exact M1 `Ng/Nr (N eff)` tally.
#[must_use]
pub fn node_evidence_fragment(node: &GraphNodeStatus) -> String {
    if is_human_gate(node) {
        String::new()
    } else if node.evidence_slots.is_empty() {
        format!(
            " · {}g/{}r ({} eff)",
            node.evidence.green, node.evidence.red, node.evidence.effective_green
        )
    } else {
        format!(
            " · {}/{} slots",
            node.evidence.effective_green,
            node.evidence_slots.len()
        )
    }
}

/// A slot's state glyph + word. A GREEN slot reports its AUTHORITY —
/// `verified` (daemon-observed process truth) vs `attested` (model testimony);
/// the UI must NEVER call an attested slot verified. Pending and failed slots
/// carry no authority word (there is nothing yet to trust).
#[must_use]
pub fn slot_state(slot: &GraphEvidenceSlotStatus) -> (&'static str, &'static str) {
    match slot.verdict {
        None => ("○", "pending"),
        Some(EvidenceVerdict::Green) => match slot.authority {
            EvidenceAuthority::DaemonVerified => ("✓", "verified"),
            EvidenceAuthority::ModelAttested => ("✓", "attested"),
        },
        Some(EvidenceVerdict::Red) => ("✗", "failed"),
    }
}

/// A bounded provenance fragment for a satisfied slot — a short subject or
/// fingerprint digest, never raw unbounded output. Empty until the slot greens.
#[must_use]
pub fn slot_provenance(slot: &GraphEvidenceSlotStatus) -> String {
    if slot.verdict != Some(EvidenceVerdict::Green) {
        return String::new();
    }
    slot.subject_digest
        .as_deref()
        .or(slot.fingerprint.as_deref())
        .map(|digest| format!(" · {}", provenance_short(digest)))
        .unwrap_or_default()
}

/// A bounded digest head for display (keeps any `algo:` prefix); never raw.
#[must_use]
pub fn provenance_short(digest: &str) -> String {
    digest.chars().take(14).collect()
}

/// One slot's plain provenance row: `glyph id  word[ · digest]`.
fn slot_row(slot: &GraphEvidenceSlotStatus) -> String {
    let (glyph, word) = slot_state(slot);
    format!("{glyph} {}  {word}{}", slot.id, slot_provenance(slot))
}

/// The first 8 hex of a template digest.
#[must_use]
pub fn digest_short(digest: &str) -> &str {
    &digest[..digest.len().min(8)]
}
