//! Convergence Graph M1 TUI surfaces: the always-visible strip above the
//! composer and the `/graph` status view. Pure over the daemon's
//! [`GraphStatus`] reduction — nothing here is ever fabricated. The glyph
//! vocabulary and header grammar match the `/tui` mockup's workflow strip.
//!
//! Visual authority: mockup DagStrip — node glyphs, `↺N` re-attempt markers,
//! and the `⚑ ship-loop · ✓a ◉b …` rollup line.

use haider_protocol::graph::{
    GraphBlockReason, GraphNodeName, GraphNodeStatus, GraphPhase, GraphStatus,
};

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
    if status.current_node == Some(node.node) {
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

/// The gate label for a node — pinned to the template, not re-derived.
#[must_use]
pub fn gate_label(node: GraphNodeName) -> &'static str {
    match node {
        GraphNodeName::Build => "command-green",
        GraphNodeName::Verify => "all-of-3",
        GraphNodeName::Ship => "human-confirm",
    }
}

/// The strip's terminal/phase badge, or empty while simply active.
#[must_use]
pub fn phase_badge(status: &GraphStatus) -> String {
    match status.phase {
        GraphPhase::Completed => "✓ complete".to_owned(),
        GraphPhase::Abandoned => "✗ abandoned".to_owned(),
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
        let evidence = if matches!(node.node, GraphNodeName::Ship) {
            String::new()
        } else {
            format!(
                " · {}g/{}r ({} eff)",
                node.evidence.green, node.evidence.red, node.evidence.effective_green
            )
        };
        lines.push(format!(
            "  {glyph}{marker} {} · {} · attempt {}/8{evidence}",
            node.node.label(),
            gate_label(node.node),
            node.current_attempt.unwrap_or(0),
        ));
    }
    match status.phase {
        GraphPhase::Completed => lines.push("✓ complete — every gate satisfied".to_owned()),
        GraphPhase::Abandoned => lines.push("✗ abandoned".to_owned()),
        GraphPhase::Blocked => lines.push(format!(
            "✗ blocked — {} · /graph abandon then re-pin to retry",
            status.blocked_reason.map_or("held", block_reason_label)
        )),
        GraphPhase::Active => {
            if let Some(node) = status.current_node {
                lines.push(format!(
                    "→ current: {} · {}",
                    node.label(),
                    expectation(node)
                ));
            }
        }
    }
    lines.join("\n")
}

fn expectation(node: GraphNodeName) -> &'static str {
    match node {
        GraphNodeName::Build => "record BUILD evidence (command green)",
        GraphNodeName::Verify => "record 3 green VERIFY results",
        GraphNodeName::Ship => "confirm the SHIP gate",
    }
}

/// The first 8 hex of a template digest.
#[must_use]
pub fn digest_short(digest: &str) -> &str {
    &digest[..digest.len().min(8)]
}
