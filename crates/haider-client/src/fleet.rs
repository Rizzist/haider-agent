//! Shared text-only fleet projections over daemon wire snapshots.

use haider_rpc::{FleetAgentStateWire, FleetNodeWire, SessionFleetSnapshot};

/// The mockup's `FLEET_GLYPH` vocabulary plus the calm waiting glyph the
/// subtree panel already speaks (`◔ waiting`) and `⊘` for cancelled.
#[must_use]
pub const fn state_glyph(state: FleetAgentStateWire) -> &'static str {
    match state {
        FleetAgentStateWire::Queued => "◌",
        FleetAgentStateWire::Live => "◉",
        FleetAgentStateWire::Waiting => "◔",
        FleetAgentStateWire::Done => "✓",
        FleetAgentStateWire::Failed => "✗",
        FleetAgentStateWire::Cancelled => "⊘",
        _ => "?",
    }
}

/// One flattened row of the current subtree, with its RELATIVE depth
/// (0 at the current root level) for indentation.
#[derive(Debug, Clone, Copy)]
pub struct FlatRow<'t> {
    pub node: &'t FleetNodeWire,
    pub rel_depth: usize,
}

/// Preorder flatten — the list density is a depth-annotated tree.
#[must_use]
pub fn flatten(nodes: &[FleetNodeWire]) -> Vec<FlatRow<'_>> {
    fn walk<'t>(nodes: &'t [FleetNodeWire], depth: usize, out: &mut Vec<FlatRow<'t>>) {
        for node in nodes {
            out.push(FlatRow {
                node,
                rel_depth: depth,
            });
            walk(&node.children, depth + 1, out);
        }
    }
    let mut out = Vec::new();
    walk(nodes, 0, &mut out);
    out
}

/// Client-side rollup over one subtree — the drill header's arithmetic.
/// At the whole-session root these figures agree with the daemon's
/// [`haider_rpc::FleetRollupWire`] by construction (both describe exactly the returned
/// nodes); the pin in `fleet_view_tests` holds the two to it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ViewRollup {
    pub total: usize,
    pub queued: usize,
    pub live: usize,
    pub waiting: usize,
    pub done: usize,
    pub failed: usize,
    pub cancelled: usize,
    pub unknown: usize,
    /// Deepest ABSOLUTE delegation depth in the subtree (wire `depth`,
    /// 1-based for direct children) — the daemon rollup's `max_depth`
    /// vocabulary.
    pub max_depth: u32,
}

#[must_use]
pub fn rollup(nodes: &[FleetNodeWire]) -> ViewRollup {
    let mut roll = ViewRollup::default();
    fn walk(nodes: &[FleetNodeWire], roll: &mut ViewRollup) {
        for node in nodes {
            roll.total += 1;
            roll.max_depth = roll.max_depth.max(node.depth);
            match node.state {
                FleetAgentStateWire::Queued => roll.queued += 1,
                FleetAgentStateWire::Live => roll.live += 1,
                FleetAgentStateWire::Waiting => roll.waiting += 1,
                FleetAgentStateWire::Done => roll.done += 1,
                FleetAgentStateWire::Failed => roll.failed += 1,
                FleetAgentStateWire::Cancelled => roll.cancelled += 1,
                _ => roll.unknown += 1,
            }
        }
        for node in nodes {
            walk(&node.children, roll);
        }
    }
    walk(nodes, &mut roll);
    roll
}

/// The frozen rollup-header grammar:
/// `fleet of N · ✓a ◉b ✗c ◌d · depth e`, with the additive `◔`/`⊘`/`?`
/// buckets appearing only when non-zero (never a fabricated 0).
#[must_use]
pub fn header_line(roll: &ViewRollup) -> String {
    let mut out = format!(
        "fleet of {} · ✓{} ◉{} ✗{} ◌{}",
        roll.total, roll.done, roll.live, roll.failed, roll.queued
    );
    if roll.waiting > 0 {
        out.push_str(&format!(" · ◔{}", roll.waiting));
    }
    if roll.cancelled > 0 {
        out.push_str(&format!(" · ⊘{}", roll.cancelled));
    }
    if roll.unknown > 0 {
        out.push_str(&format!(" · ?{}", roll.unknown));
    }
    out.push_str(&format!(" · depth {}", roll.max_depth));
    out
}

/// A node's display callsign: the persisted callsign, else the opaque
/// agent id (clients may choose their own fallback — never a fabrication).
#[must_use]
pub fn callsign(node: &FleetNodeWire) -> &str {
    match node.callsign.as_deref() {
        Some(callsign) if !callsign.is_empty() => callsign,
        _ => node.agent_id.as_str(),
    }
}

/// The right-aligned row metric, mockup grammar `Nt · <tok> · ≈$<cost>`.
/// Queued renders the literal `queued`; a node without durable metrics
/// DROPS its segments (unknown is never rendered as zero); the cost form
/// is the S4 [`crate::agent_metrics::compact_cost`] vocabulary — OAuth
/// lanes keep the labeled `≈$` API-equivalent form.
#[must_use]
pub fn node_metric(node: &FleetNodeWire) -> String {
    if node.state == FleetAgentStateWire::Queued {
        return "queued".to_owned();
    }
    let Some(metrics) = &node.metrics else {
        return String::new();
    };
    let mut segments = vec![format!("{}t", metrics.tool_attempts)];
    if let Some(usage) = &metrics.usage {
        segments.push(crate::display::fmt_tok(
            crate::agent_metrics::normalized_tokens(usage),
        ));
        segments.push(crate::agent_metrics::compact_cost(usage));
    }
    segments.join(" · ")
}

/// A node's direct-child marker. A fold witness outranks the count of
/// returned children because it is the information that would otherwise be
/// lost when a bounded node looks like a leaf.
#[must_use]
pub fn child_marker(node: &FleetNodeWire) -> Option<String> {
    if node.folded_children > 0 {
        Some(format!("⊞{}", node.folded_children))
    } else if node.children.is_empty() {
        None
    } else {
        Some(format!("▸{}", node.children.len()))
    }
}

/// The honest truncation footer, present only when the daemon bounded the
/// tree: the deepest branches were folded to fit the node cap.
#[must_use]
pub fn truncation_footer(snapshot: &SessionFleetSnapshot) -> Option<String> {
    snapshot.truncated.then(|| {
        format!(
            "{}-node view cap reached — deepest branches folded",
            snapshot.node_limit
        )
    })
}
