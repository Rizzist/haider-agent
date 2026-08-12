//! The FLEET VIEW (slice 1) — a session-born, full-screen view of the
//! session's recursive subagent fleet, backed by the daemon's bounded
//! `session.fleet` snapshot (`session_fleet_v1`).
//!
//! Entry is SESSION-BORN, never a menu destination: under five tree nodes
//! the subagents panel keeps today's inline rows; at five or more the rows
//! collapse into ONE summary row (`⣿ N subagents · … · ⌥F fleet`), and ⌥F
//! or the row opens this view for the CURRENT session.
//!
//! Densities (mockup grammar, tui.js `FleetStage`):
//!
//! * ≤ [`GRID_THRESHOLD`] nodes at the current root — the depth-annotated
//!   tree LIST: state glyph · callsign · task fragment · right-aligned
//!   `Nt · <tok> · ≈$<cost>` metrics (the S4 cost vocabulary: OAuth lanes
//!   stay the labeled `≈$` API-equivalent form, never a bare `$`);
//! * above it — the max-density GRID: one compact cell per DIRECT child,
//!   the agent's deterministic 8-bit matrix-dot pattern state-tinted, the
//!   callsign under the cell. The switch is automatic; slice 1 ships no
//!   manual density toggle.
//!
//! Drill-down re-roots: ⏎ on a row/cell WITH children re-roots the view on
//! that subtree (the header shows the path), esc walks up one level, esc at
//! the root closes back to the session.
//!
//! Data: LIVE sessions re-read on the existing event cadence — the driver
//! chases one `session.fleet` read per applied-envelope burst while the
//! screen is open (single-flight, no new polling loop). Terminal sessions
//! render the durable snapshot once. A truncated snapshot renders an honest
//! footer row. Demo mode synthesizes the snapshot from the local chip tree
//! at open (the sim's session-born fleet), the same once-rendered shape as
//! a terminal session.

use haider_protocol::ids::{AgentId, SessionId};
use haider_rpc::{
    FLEET_MAX_DEPTH, FLEET_MAX_NODES, FleetAgentStateWire, FleetMetricsTotalsWire, FleetNodeWire,
    FleetRollupWire, FleetStateCountsWire, SessionFleetSnapshot,
};

/// Above this many nodes in the CURRENT root's subtree the view auto-switches
/// to the max-density grid (mockup: `total > 20 ? "max" : "list"`).
pub const GRID_THRESHOLD: usize = 20;

/// At this many tree nodes the session panel collapses its per-chip rows
/// into the single fleet summary row.
pub const ENTRY_COLLAPSE: usize = 5;

/// The fleet view's state. The snapshot is daemon truth (live) or the
/// open-instant chip synthesis (demo); the drill stack re-roots by agent
/// id so a refreshed snapshot re-resolves the same subtree.
#[derive(Debug, Default)]
pub struct FleetView {
    pub snapshot: Option<SessionFleetSnapshot>,
    /// Drill path, outermost first. Empty = the whole-session root.
    pub stack: Vec<AgentId>,
    /// Selected row (list: flattened index) / cell (grid: child index).
    pub sel: usize,
    /// A live `session.fleet` read is outstanding for THIS screen.
    pub fetching: bool,
    /// The last read failed — rendered honestly, never a silent stale view.
    pub error: Option<String>,
    /// Grid geometry measured by RENDER (the layout authority — the
    /// `scroll_max` discipline): columns for ↑/↓ arithmetic.
    pub grid_cols: std::cell::Cell<usize>,
    /// Visible body rows measured by RENDER, for PgUp/PgDn.
    pub page_rows: std::cell::Cell<usize>,
}

/// Which density the current root renders at. Derived, never stored —
/// slice 1 has no manual toggle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Density {
    List,
    Grid,
}

#[must_use]
pub fn density(subtree_nodes: usize) -> Density {
    if subtree_nodes > GRID_THRESHOLD {
        Density::Grid
    } else {
        Density::List
    }
}

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

fn find_in<'t>(nodes: &'t [FleetNodeWire], agent: &AgentId) -> Option<&'t FleetNodeWire> {
    for node in nodes {
        if &node.agent_id == agent {
            return Some(node);
        }
        if let Some(found) = find_in(&node.children, agent) {
            return Some(found);
        }
    }
    None
}

/// Resolve the drill stack against the snapshot: the sibling level at the
/// current root plus the resolved path nodes. A hop that no longer resolves
/// (the node left a refreshed snapshot) TRUNCATES the path there — the view
/// honestly falls back to the nearest surviving ancestor.
#[must_use]
pub fn resolve<'t>(
    snapshot: &'t SessionFleetSnapshot,
    stack: &[AgentId],
) -> (&'t [FleetNodeWire], Vec<&'t FleetNodeWire>) {
    let mut level: &'t [FleetNodeWire] = &snapshot.roots;
    let mut path = Vec::new();
    for agent in stack {
        match find_in(level, agent) {
            Some(node) => {
                path.push(node);
                level = &node.children;
            }
            None => break,
        }
    }
    (level, path)
}

/// Client-side rollup over one subtree — the drill header's arithmetic.
/// At the whole-session root these figures agree with the daemon's
/// [`FleetRollupWire`] by construction (both describe exactly the returned
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

/// The deterministic 8-bit matrix-dot pattern (mockup `agent.matrix`):
/// an FNV-1a fold of the agent id, OR `0x42` so a pattern is never empty.
/// Same id → same pattern, forever — no randomness anywhere.
#[must_use]
pub fn matrix_bits(agent: &str) -> u8 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in agent.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let folded = (hash ^ (hash >> 8) ^ (hash >> 16) ^ (hash >> 32)) as u8;
    folded | 0x42
}

/// The 4×2 dot matrix as two 4-char terminal rows (bit 0 top-left,
/// row-major — the mockup's grid order): lit `●`, unlit `·`.
#[must_use]
pub fn matrix_rows(bits: u8) -> [String; 2] {
    let row = |offset: u8| {
        (0..4u8)
            .map(|bit| {
                if (bits >> (offset + bit)) & 1 == 1 {
                    '●'
                } else {
                    '·'
                }
            })
            .collect::<String>()
    };
    [row(0), row(4)]
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
        segments.push(crate::format::fmt_tok(
            crate::agent_metrics::normalized_tokens(usage),
        ));
        segments.push(crate::agent_metrics::compact_cost(usage));
    }
    segments.join(" · ")
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

/// Any live node anywhere in the snapshot — the `animated()` gate for the
/// ◉ glyph / matrix pulse.
#[must_use]
pub fn has_live(snapshot: &SessionFleetSnapshot) -> bool {
    fn walk(nodes: &[FleetNodeWire]) -> bool {
        nodes
            .iter()
            .any(|node| node.state == FleetAgentStateWire::Live || walk(&node.children))
    }
    walk(&snapshot.roots)
}

// ---------------------------------------------------------------- entry ----

/// The chip panel's display state → the fleet state vocabulary, for the
/// entry summary row and the demo synthesis. `closed` is a cancellation in
/// flight; the working states are all LIVE; input-required is the user's
/// wait, so it joins `waiting` (the calm bucket).
#[must_use]
pub fn chip_fleet_state(chip: &crate::app::ChipModel) -> FleetAgentStateWire {
    use crate::script::ChipDisplayState as S;
    if chip.closed {
        return FleetAgentStateWire::Cancelled;
    }
    match chip.display_state() {
        S::Idle => FleetAgentStateWire::Queued,
        S::Thinking | S::Streaming | S::Running | S::Tool => FleetAgentStateWire::Live,
        S::InputRequired | S::Waiting => FleetAgentStateWire::Waiting,
        S::Done => FleetAgentStateWire::Done,
        S::Error => FleetAgentStateWire::Failed,
    }
}

/// Rollup over the LOCAL chip tree — what the summary row replaces, so the
/// counts describe exactly the rows the panel would have drawn.
#[must_use]
pub fn entry_rollup(chips: &[crate::app::ChipModel]) -> ViewRollup {
    let mut roll = ViewRollup::default();
    for (depth, chip) in crate::app::flatten_chips(chips) {
        roll.total += 1;
        roll.max_depth = roll
            .max_depth
            .max(u32::try_from(depth + 1).unwrap_or(u32::MAX));
        match chip_fleet_state(chip) {
            FleetAgentStateWire::Queued => roll.queued += 1,
            FleetAgentStateWire::Live => roll.live += 1,
            FleetAgentStateWire::Waiting => roll.waiting += 1,
            FleetAgentStateWire::Done => roll.done += 1,
            FleetAgentStateWire::Failed => roll.failed += 1,
            FleetAgentStateWire::Cancelled => roll.cancelled += 1,
            _ => roll.unknown += 1,
        }
    }
    roll
}

/// The collapsed entry row (mockup tui.js:4562-4565):
/// `⣿ N subagents · a done · b live[ · …] · ⌥F fleet` — done and live
/// always named, the quieter buckets only when non-zero.
#[must_use]
pub fn entry_summary(roll: &ViewRollup) -> String {
    let mut out = format!(
        "{} subagents · {} done · {} live",
        roll.total, roll.done, roll.live
    );
    if roll.waiting > 0 {
        out.push_str(&format!(" · {} waiting", roll.waiting));
    }
    if roll.failed > 0 {
        out.push_str(&format!(" · {} failed", roll.failed));
    }
    if roll.queued > 0 {
        out.push_str(&format!(" · {} queued", roll.queued));
    }
    if roll.cancelled > 0 {
        out.push_str(&format!(" · {} closing", roll.cancelled));
    }
    out.push_str(" · ⌥F fleet");
    out
}

// ----------------------------------------------------------- synthesis ----

/// Demo-mode synthesis: the local chip tree as a fleet snapshot, built at
/// open (the sim's session-born fleet). Deterministic over the chips —
/// callsigns, tasks, metrics and states come straight from the panel's own
/// rows; nothing is invented. Live mode NEVER calls this: the daemon's
/// durable snapshot is the only truth there.
#[must_use]
pub fn snapshot_from_chips(
    chips: &[crate::app::ChipModel],
    session: &SessionId,
    now_ms: u64,
) -> SessionFleetSnapshot {
    fn node_of(
        chip: &crate::app::ChipModel,
        parent_session: &SessionId,
        parent_agent: Option<&AgentId>,
        depth: u32,
    ) -> FleetNodeWire {
        let session_id = chip.child_session.clone().map_or_else(
            || SessionId::new(format!("demo-{}", chip.agent)),
            SessionId::new,
        );
        let agent_id = AgentId::new(chip.agent.clone());
        let children = chip
            .children
            .iter()
            .map(|child| node_of(child, &session_id, Some(&agent_id), depth + 1))
            .collect();
        FleetNodeWire {
            agent_id,
            session_id,
            callsign: (!chip.callsign.is_empty()).then(|| chip.callsign.clone()),
            task: chip.name.clone(),
            depth,
            parent_session_id: parent_session.clone(),
            parent_agent_id: parent_agent.cloned(),
            state: chip_fleet_state(chip),
            metrics: chip.metrics.clone(),
            children,
        }
    }
    let roots: Vec<FleetNodeWire> = chips
        .iter()
        .map(|chip| node_of(chip, session, None, 1))
        .collect();
    let roll = rollup(&roots);
    let flat = flatten(&roots);
    let mut elapsed_ms = 0_u64;
    let mut tool_attempts = 0_u64;
    let metrics_complete = flat.iter().all(|row| {
        row.node
            .metrics
            .as_ref()
            .is_some_and(|metrics| metrics.usage.is_some())
    }) && !flat.is_empty();
    for row in &flat {
        if let Some(metrics) = &row.node.metrics {
            elapsed_ms =
                elapsed_ms.saturating_add(crate::agent_metrics::elapsed_ms(metrics, now_ms));
            tool_attempts = tool_attempts.saturating_add(metrics.tool_attempts);
        }
    }
    let usage =
        crate::agent_metrics::aggregate(flat.iter().filter_map(|row| row.node.metrics.as_ref()))
            .and_then(|aggregate| aggregate.usage);
    let states = FleetStateCountsWire {
        queued: u32::try_from(roll.queued).unwrap_or(u32::MAX),
        live: u32::try_from(roll.live).unwrap_or(u32::MAX),
        waiting: u32::try_from(roll.waiting).unwrap_or(u32::MAX),
        done: u32::try_from(roll.done).unwrap_or(u32::MAX),
        failed: u32::try_from(roll.failed).unwrap_or(u32::MAX),
        cancelled: u32::try_from(roll.cancelled).unwrap_or(u32::MAX),
    };
    SessionFleetSnapshot {
        session_id: session.clone(),
        generated_at_ms: now_ms,
        node_limit: FLEET_MAX_NODES,
        depth_limit: FLEET_MAX_DEPTH,
        rollup: FleetRollupWire {
            node_count: u32::try_from(roll.total).unwrap_or(u32::MAX),
            states,
            max_depth: roll.max_depth,
            metrics: FleetMetricsTotalsWire {
                elapsed_ms,
                tool_attempts,
                usage,
            },
            metrics_complete,
            complete: true,
        },
        roots,
        truncated: false,
    }
}

// ---------------------------------------------------------------- plain ----

/// Plain parity for the fleet screen — the same information as the styled
/// surface in honest UTF-8 lines: crumb path, the rollup header, one line
/// per node in tree order (BOTH densities carry the list grammar — plain
/// has no grid geometry, the information is what must survive), and the
/// truncation footer.
#[must_use]
pub fn fleet_plain(model: &crate::app::AppModel) -> String {
    let view = &model.fleet;
    let mut out = String::from("FLEET — ");
    out.push_str(model.display_name());
    out.push_str(" › fleet");
    if let Some(snapshot) = &view.snapshot {
        let (level, path) = resolve(snapshot, &view.stack);
        for node in &path {
            out.push_str(" › ");
            out.push_str(callsign(node));
        }
        out.push('\n');
        out.push_str(&header_line(&rollup(level)));
        out.push('\n');
        for row in flatten(level) {
            out.push_str("  ");
            out.push_str(&" │ ".repeat(row.rel_depth));
            out.push_str(state_glyph(row.node.state));
            out.push(' ');
            out.push_str(callsign(row.node));
            if !row.node.task.is_empty() {
                out.push_str(" — ");
                out.push_str(&row.node.task);
            }
            let metric = node_metric(row.node);
            if !metric.is_empty() {
                out.push_str(" · ");
                out.push_str(&metric);
            }
            out.push('\n');
        }
        if let Some(footer) = truncation_footer(snapshot) {
            out.push_str(&footer);
            out.push('\n');
        }
    } else {
        out.push('\n');
        if let Some(error) = &view.error {
            out.push_str(&format!("✗ fleet read failed — {error}\n"));
        } else if view.fetching {
            out.push_str("fetching fleet…\n");
        } else {
            out.push_str("no fleet — this session has no subagents\n");
        }
    }
    out
}
