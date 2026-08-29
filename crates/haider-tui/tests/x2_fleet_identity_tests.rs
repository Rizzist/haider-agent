//! 966 lane X2 — the fleet row's IDENTITY, and the composer that speaks
//! for the surface it steers.
//!
//! The owner's report: a fleet row spent its whole line on one 64-char hex
//! agent id, and the composer footer under a CHILD transcript read the
//! PARENT's model and auth.
//!
//! Pinned laws:
//!
//! * IDENTITY — a row carries `callsign · model · provider`, in BOTH
//!   densities; it ADDS identity beside the task, never restates it;
//! * ABSENCE — a node with no model and no provider renders NEITHER, not a
//!   placeholder and not a guess (`fleet::callsign`'s "never a fabrication"
//!   and `fleet::node_metric`'s dropped-segment law, applied to identity);
//! * DEGRADATION — the list tail drops segments WHOLE (provider, then
//!   model, then the tail) and always yields to the task's floor; the GRID
//!   cell instead truncates with a `…`, because the callsign directly above
//!   it is already hard-cut to the same ten columns;
//! * COMPOSER — while a child is viewed the band rule speaks the CHILD's
//!   model and auth; the session's reasoning/fast knobs are not the
//!   child's, so they drop rather than mislabel it;
//! * ACTIVATION — a click is an activation, not a dead selection, and the
//!   member-detail frame's transcript door answers the mouse as well as ⏎;
//! * DESTROY — always two presses, and the arm names its target.
//!
//! Fixtures are deterministic by construction — no randomness anywhere.
#![allow(clippy::expect_used)]

use haider_protocol::agent::{
    AgentManifest, AgentMetricsSnapshot, AgentRole, AgentUsageBreakdown, AgentUsageMetrics, Grant,
    Placement,
};
use haider_protocol::credential::AuthMethod;
use haider_protocol::ids::{AgentId, LeaseId, SessionId};
use haider_protocol::provider::UsageRequestKind;
use haider_rpc::{
    FLEET_MAX_DEPTH, FLEET_MAX_NODES, FleetAgentStateWire, FleetMetricsTotalsWire, FleetNodeWire,
    FleetRollupWire, FleetStateCountsWire, SessionFleetSnapshot,
};
use haider_tui::app::{AppModel, AppRequest, ChipModel, Hit, Screen};
use haider_tui::fleet;
use haider_tui::render::render;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::KeyCode;

mod common;
use common::{key, launcher_model};

const SPAWN_MS: u64 = 1_000_000_000;

fn sid() -> SessionId {
    SessionId::new("s-fleet")
}

// ------------------------------------------------------------ fixtures ----

fn usage(method: AuthMethod) -> AgentUsageMetrics {
    AgentUsageMetrics {
        logical_input_tokens: 1_200,
        billed_output_tokens: 300,
        all_lanes_priced: true,
        breakdowns: vec![AgentUsageBreakdown {
            provider: "anthropic".into(),
            model: "fable-5".into(),
            cache_epoch: "epoch-x2".into(),
            request_kind: UsageRequestKind::DelegatedAgent,
            auth_method: Some(method),
            logical_input_tokens: 1_200,
            billed_output_tokens: 300,
            priced: true,
            ..AgentUsageBreakdown::default()
        }],
        ..AgentUsageMetrics::default()
    }
}

/// A node carrying whatever identity the case is about. `None`/`None` is
/// the honest "this node was never told" case, and it is the DEFAULT the
/// pre-existing fixtures elsewhere already use.
fn node(
    agent: &str,
    callsign: &str,
    task: &str,
    model: Option<&str>,
    provider: Option<&str>,
    children: Vec<FleetNodeWire>,
) -> FleetNodeWire {
    FleetNodeWire {
        agent_id: AgentId::new(agent),
        session_id: SessionId::new(format!("child-{agent}")),
        callsign: Some(callsign.to_owned()),
        model: model.map(str::to_owned),
        provider: provider.map(str::to_owned),
        task: task.to_owned(),
        depth: 1,
        parent_session_id: sid(),
        parent_agent_id: None,
        state: FleetAgentStateWire::Done,
        metrics: None,
        folded_children: 0,
        children,
    }
}

fn snapshot(roots: Vec<FleetNodeWire>) -> SessionFleetSnapshot {
    let roll = fleet::rollup(&roots);
    SessionFleetSnapshot {
        session_id: sid(),
        generated_at_ms: SPAWN_MS,
        node_limit: FLEET_MAX_NODES,
        depth_limit: FLEET_MAX_DEPTH,
        rollup: FleetRollupWire {
            node_count: u32::try_from(roll.total).expect("fixture bounds"),
            states: FleetStateCountsWire {
                queued: u32::try_from(roll.queued).expect("fixture bounds"),
                live: u32::try_from(roll.live).expect("fixture bounds"),
                waiting: u32::try_from(roll.waiting).expect("fixture bounds"),
                done: u32::try_from(roll.done).expect("fixture bounds"),
                failed: u32::try_from(roll.failed).expect("fixture bounds"),
                cancelled: u32::try_from(roll.cancelled).expect("fixture bounds"),
            },
            max_depth: roll.max_depth,
            metrics: FleetMetricsTotalsWire::default(),
            metrics_complete: false,
            complete: true,
        },
        roots,
        truncated: false,
    }
}

fn fleet_model(snapshot: SessionFleetSnapshot) -> AppModel {
    let mut model = launcher_model();
    model.screen = Screen::Fleet;
    model.fleet.snapshot = Some(snapshot);
    model
}

fn draw_rows(model: &AppModel, width: u16, height: u16) -> Vec<String> {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| {
            render(model, frame);
        })
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect()
}

/// The mixed tree every identity case reads: one node that knows both
/// facts, one that knows only its provider, one that knows NOTHING.
fn mixed_tree() -> Vec<FleetNodeWire> {
    vec![
        node(
            "ag-recon",
            "recon",
            "map the seams",
            Some("glm-5.2"),
            Some("zai"),
            vec![],
        ),
        node(
            "ag-probe",
            "probe",
            "exercise the edges",
            None,
            Some("anthropic"),
            vec![],
        ),
        node("ag-shim", "shim", "retry the edge", None, None, vec![]),
    ]
}

// ------------------------------------------------------------- the row ----

/// A row renders callsign + model + provider, in the file's own ` · `
/// grammar, and it ADDS to the task rather than restating it.
///
/// MUTATION CHECK: render `node.agent_id` instead of the identity tail.
/// Expected runtime failure: the `recon · glm-5.2 · zai` containment below
/// stops matching.
#[test]
fn a_list_row_carries_callsign_model_and_provider() {
    let model = fleet_model(snapshot(mixed_tree()));
    let rows = draw_rows(&model, 100, 24).join("\n");
    assert!(
        rows.contains("recon · glm-5.2 · zai — map the seams"),
        "the row grammar: name, then identity, then the task: {rows}"
    );
    // Plain parity carries the same information, unbudgeted.
    let plain = fleet::fleet_plain(&model);
    assert!(
        plain.contains("recon · glm-5.2 · zai — map the seams"),
        "plain parity: {plain}"
    );
}

/// ABSENCE. A node that knows neither fact renders NEITHER — no `—`, no
/// `unknown`, no empty ` · ` husk. A node that knows only one renders only
/// that one, because a true half beats an empty line.
///
/// MUTATION CHECK: give the absent halves a placeholder (`"unknown"`).
/// Expected runtime failure: the shim row's assertion finds the
/// placeholder next to its callsign.
#[test]
fn a_node_without_model_or_provider_renders_neither() {
    let model = fleet_model(snapshot(mixed_tree()));
    let rows = draw_rows(&model, 100, 24);
    let shim = rows
        .iter()
        .find(|row| row.contains("shim"))
        .expect("the shim row renders");
    assert!(
        shim.contains("shim — retry the edge"),
        "no model, no provider: the task follows the name directly: {shim:?}"
    );
    for ghost in ["unknown", "none", "—t", " ·  ", "n/a"] {
        assert!(
            !shim.contains(ghost),
            "no placeholder stands in for an absent fact ({ghost}): {shim:?}"
        );
    }
    // Provider-only is still TRUE, so it still renders.
    let probe = rows
        .iter()
        .find(|row| row.contains("probe"))
        .expect("the probe row renders");
    assert!(
        probe.contains("probe · anthropic — exercise the edges"),
        "a node that knows only its provider shows it: {probe:?}"
    );
    // And the pure helper agrees with the surface.
    let tree = mixed_tree();
    assert_eq!(fleet::node_identity(&tree[2], usize::MAX), None);
    assert_eq!(
        fleet::node_identity(&tree[1], usize::MAX).as_deref(),
        Some("anthropic")
    );
    assert_eq!(
        fleet::node_identity(&tree[0], usize::MAX).as_deref(),
        Some("glm-5.2 · zai")
    );
}

/// WIDTH DEGRADATION in the list: whole segments, provider first, then the
/// model, then the tail — never a mid-word cut.
#[test]
fn the_list_tail_drops_whole_segments_in_order() {
    let tree = mixed_tree();
    let both = &tree[0];
    let full = "glm-5.2 · zai";
    assert_eq!(fleet::node_identity(both, 80).as_deref(), Some(full));
    assert_eq!(
        fleet::node_identity(both, full.chars().count()).as_deref(),
        Some(full)
    );
    assert_eq!(
        fleet::node_identity(both, full.chars().count() - 1).as_deref(),
        Some("glm-5.2"),
        "the provider drops WHOLE before the model gives up a character"
    );
    assert_eq!(
        fleet::node_identity(both, 6),
        None,
        "below the model name the tail vanishes whole — never `glm-5…`"
    );
    for budget in 0..=40usize {
        if let Some(candidate) = fleet::node_identity(both, budget) {
            assert!(
                [full, "glm-5.2"].contains(&candidate.as_str()),
                "unexpected degraded form {candidate:?} at {budget}"
            );
        }
    }
}

/// The identity yields to the TASK: a narrow row keeps the task fragment
/// rather than spending the whole line on a model name.
#[test]
fn the_identity_yields_to_the_task_on_a_narrow_row() {
    let model = fleet_model(snapshot(mixed_tree()));
    let rows = draw_rows(&model, 34, 24);
    let recon = rows
        .iter()
        .find(|row| row.contains("recon"))
        .expect("the recon row renders at 34 columns");
    assert!(
        recon.contains('—'),
        "the task fragment survives the identity: {recon:?}"
    );
    assert!(
        !recon.contains("glm-5.2 · zai"),
        "the provider yielded before the task did: {recon:?}"
    );
}

// ------------------------------------------------------------- the grid ----

/// A tree wide enough to force the max-density grid (> `GRID_THRESHOLD`
/// nodes at the root), carrying one long model, one short one, one
/// provider-only node and one that knows nothing.
fn grid_tree() -> Vec<FleetNodeWire> {
    let mut roots = vec![
        node(
            "ag-long",
            "longcall",
            "t",
            Some("deepseek-v4-flash"),
            Some("deepseek"),
            vec![],
        ),
        node("ag-short", "shortie", "t", Some("glm-5.2"), None, vec![]),
        node("ag-provo", "provo", "t", None, Some("zai"), vec![]),
        node("ag-blank", "blank", "t", None, None, vec![]),
    ];
    for index in 0..20 {
        roots.push(node(
            &format!("ag-f{index}"),
            &format!("f{index}"),
            "t",
            None,
            None,
            vec![],
        ));
    }
    roots
}

/// GRID DENSITY UNDER TRUNCATION. Ten columns cannot hold two facts, so
/// the cell carries ONE — the model, or the provider when there is no
/// model — and the cut is MARKED, so a clipped name never reads whole.
///
/// MUTATION CHECK: drop the `…` and hard-cut instead. Expected runtime
/// failure: `deepseek-…` stops appearing and the bare `deepseek-` cut is
/// indistinguishable from a real model of that name.
#[test]
fn the_grid_cell_truncates_one_marked_fact() {
    let model = fleet_model(snapshot(grid_tree()));
    assert_eq!(
        fleet::density(fleet::rollup(&grid_tree()).total),
        fleet::Density::Grid,
        "the fixture is wide enough for the max-density grid"
    );
    let rows = draw_rows(&model, 120, 30).join("\n");
    assert!(
        rows.contains("deepseek-…"),
        "the long model is cut to the cell and MARKED: {rows}"
    );
    assert!(
        rows.contains("glm-5.2"),
        "a model that fits renders whole: {rows}"
    );
    assert!(
        rows.contains("zai"),
        "no model, so the cell falls back to the provider: {rows}"
    );
    // The pure helper carries the same law.
    let tree = grid_tree();
    assert_eq!(
        fleet::node_identity_cell(&tree[0], 10).as_deref(),
        Some("deepseek-…")
    );
    assert_eq!(
        fleet::node_identity_cell(&tree[1], 10).as_deref(),
        Some("glm-5.2")
    );
    assert_eq!(
        fleet::node_identity_cell(&tree[2], 10).as_deref(),
        Some("zai")
    );
    assert_eq!(
        fleet::node_identity_cell(&tree[3], 10),
        None,
        "a node that knows nothing keeps the blank it always had"
    );
}

/// The identity row costs the grid NO cells: a band is still four rows,
/// so the max-density view shows exactly what it showed before.
#[test]
fn the_grid_identity_row_costs_no_cells() {
    let bare: Vec<FleetNodeWire> = (0..24)
        .map(|index| {
            node(
                &format!("ag-b{index}"),
                &format!("b{index}"),
                "t",
                None,
                None,
                vec![],
            )
        })
        .collect();
    let known = fleet_model(snapshot(grid_tree()));
    let unknown = fleet_model(snapshot(bare));
    let count = |model: &AppModel| {
        draw_rows(model, 120, 30)
            .iter()
            .filter(|row| row.contains('●') || row.contains('·'))
            .count()
    };
    assert_eq!(
        count(&known),
        count(&unknown),
        "identity rides the band's existing 4th row — it never costs a band"
    );
}

// ---------------------------------------------------------- the composer ----

fn child_manifest(agent: &str, model_profile: &str) -> AgentManifest {
    AgentManifest {
        agent: AgentId::new(agent),
        role: AgentRole::Subagent,
        task: "guess a random number".to_owned(),
        callsign: Some("recon".to_owned()),
        model_profile: model_profile.to_owned(),
        grant: Grant {
            tools: vec![],
            effect_ceiling: vec![],
        },
        budget_tokens: None,
        placement: Placement::Local,
        lease: LeaseId::new(format!("lease-{agent}")),
        fencing_epoch: 1,
        attempt: 0,
        parent: None,
        coordinates: None,
        cli_scope: None,
    }
}

/// A parent whose OWN identity is deliberately different from the child's,
/// so a leaked parent fact is unmistakable.
fn child_view(model_profile: &str) -> AppModel {
    let mut model = launcher_model();
    // Seeded so the PARENT's own identity resolves to its full four-segment
    // form — the contrast with the child's is then unmistakable.
    model
        .providers
        .apply_snapshot(haider_tui::mock::seed_provider_summaries(), 1);
    model
        .accounts
        .apply_snapshot(haider_tui::mock::seed_account_rows(), Some(1));
    model.identity.provider = "anthropic".to_owned();
    model.identity.model_short = "glm-5.2".to_owned();
    model.identity.reasoning = Some("high".to_owned());
    model.identity.fast = true;
    let chip = ChipModel::from_manifest(&child_manifest("ag-recon", model_profile));
    model.chips = vec![chip];
    model.view_path = vec!["ag-recon".to_owned()];
    model.screen = Screen::Subagent;
    model
}

/// THE BUG. While a child is viewed, the composer band speaks the CHILD's
/// model — not the parent's — and the parent's reasoning/fast knobs, which
/// are not the child's, do not ride along.
///
/// MUTATION CHECK: restore `composer_identity` in `render_composer`.
/// Expected runtime failure: the rule carries `glm-5.2` and the child's
/// `deepseek-v4-flash` is absent from it.
#[test]
fn the_composer_speaks_the_child_while_a_child_is_viewed() {
    let model = child_view("deepseek-v4-flash");
    assert_eq!(
        model.surface_composer_identity(60).as_deref(),
        Some("deepseek-v4-flash"),
        "the child's manifest model, and no parent auth/reasoning riding it"
    );
    let rows = draw_rows(&model, 100, 30);
    let rule = rows
        .iter()
        .find(|row| row.contains('─') && row.contains("deepseek-v4-flash"))
        .expect("the band rule carries the CHILD's model");
    assert!(
        !rule.contains("glm-5.2"),
        "the parent's model never rides the child's rule: {rule:?}"
    );
    assert!(
        !rule.contains("high") && !rule.contains("fast"),
        "the session's reasoning knobs are not the child's: {rule:?}"
    );
    // The parent's own surface is untouched by the fix.
    let mut parent = model;
    parent.screen = Screen::Session;
    assert_eq!(
        parent.surface_composer_identity(60).as_deref(),
        Some("glm-5.2 · oauth · high · fast"),
        "the session surface still speaks the session's own pair"
    );
}

/// The child's AUTH comes from what the child actually billed — its own
/// usage breakdowns carry the method outright.
#[test]
fn the_child_auth_label_comes_from_the_childs_own_usage() {
    let mut model = child_view("deepseek-v4-flash");
    model.chips[0].metrics = Some(AgentMetricsSnapshot {
        agent: Some(AgentId::new("ag-recon")),
        session_id: SessionId::new("child-ag-recon"),
        head_seq: 9,
        started_at_ms: SPAWN_MS,
        terminal_at_ms: None,
        live: true,
        tool_attempts: 3,
        usage: Some(usage(AuthMethod::ApiKey)),
    });
    assert_eq!(
        model.surface_composer_identity(60).as_deref(),
        Some("deepseek-v4-flash · api"),
        "the child's own billed auth method"
    );
}

/// A child whose model is not known renders NO identity — the parent's
/// never stands in for it.
#[test]
fn a_child_without_a_model_renders_no_identity() {
    let model = child_view("");
    assert_eq!(
        model.surface_composer_identity(60),
        None,
        "absent is absent — never the parent's model as a stand-in"
    );
    let rows = draw_rows(&model, 100, 30).join("\n");
    assert!(
        !rows.contains("glm-5.2 · oauth"),
        "the parent identity block is nowhere on the child's surface: {rows}"
    );
}

// --------------------------------------------------------- activation ----

/// A click is an ACTIVATION, not a dead selection: it opens a leaf's
/// detail frame exactly as ⏎ does, on the row that was clicked.
#[test]
fn a_row_click_activates_the_row_it_was_rendered_for() {
    let mut model = fleet_model(snapshot(mixed_tree()));
    model.handle_hit(Hit::FleetNode("ag-probe".to_owned()));
    assert_eq!(
        model
            .fleet
            .detail
            .as_ref()
            .map(haider_protocol::ids::AgentId::as_str),
        Some("ag-probe"),
        "the clicked leaf opened ITS detail frame"
    );
    assert_eq!(model.fleet.sel, 1, "and the selection followed the click");
}

/// The detail frame's transcript door answers the mouse as well as ⏎, and
/// opening it lands on the MEMBER's own session rather than inheriting the
/// previous surface's scroll.
#[test]
fn the_detail_transcript_door_opens_the_members_own_session() {
    let mut model = fleet_model(snapshot(mixed_tree()));
    model.chips = vec![ChipModel::from_manifest(&child_manifest(
        "ag-recon", "glm-5.2",
    ))];
    model.fleet.detail = Some(AgentId::new("ag-recon"));
    model.scroll_back.set(37);
    model.handle_hit(Hit::FleetTranscript("ag-recon".to_owned()));
    assert_eq!(model.screen, Screen::Subagent, "the chip view opened");
    assert_eq!(
        model.view_path,
        vec!["ag-recon".to_owned()],
        "rooted on the member that was clicked"
    );
    assert_eq!(
        model.scroll_back.get(),
        0,
        "the member's own session is what the view opens on — the parent's \
         scroll never rides along"
    );
}

// ------------------------------------------------------------ destroy ----

/// DESTROY is always two presses, and the arm NAMES its target so a
/// snapshot refreshed underneath it cannot retarget the kill.
///
/// MUTATION CHECK: act on the first press. Expected runtime failure: the
/// request queue is non-empty after one `d`.
#[test]
fn destroy_takes_two_presses_and_the_arm_names_its_target() {
    let mut model = fleet_model(snapshot(mixed_tree()));
    model.chips = vec![ChipModel::from_manifest(&child_manifest(
        "ag-recon", "glm-5.2",
    ))];
    model.fleet.detail = Some(AgentId::new("ag-recon"));

    model.handle(key(KeyCode::Char('d')));
    assert_eq!(
        model.fleet.kill_armed.as_ref().map(AgentId::as_str),
        Some("ag-recon"),
        "the first press only ARMS, and the arm names the member"
    );
    assert!(
        model.requests.is_empty(),
        "one press destroys nothing: {:?}",
        model.requests
    );
    assert!(
        model
            .flash
            .as_deref()
            .is_some_and(|flash| flash.contains("recon") && flash.contains("confirm")),
        "the arm asks by CALLSIGN, not by hex id: {:?}",
        model.flash
    );

    model.handle(key(KeyCode::Char('d')));
    assert!(
        model.fleet.kill_armed.is_none(),
        "the confirming press disarms"
    );
    assert!(
        matches!(
            model.requests.as_slice(),
            [AppRequest::ChipClose { agent }] if agent == "ag-recon"
        ),
        "the confirmed destroy closes exactly that member: {:?}",
        model.requests
    );
}

/// Navigation DISARMS. An arm never survives the cursor moving off the
/// member it named.
#[test]
fn navigation_disarms_a_pending_destroy() {
    let mut model = fleet_model(snapshot(mixed_tree()));
    model.chips = vec![ChipModel::from_manifest(&child_manifest(
        "ag-recon", "glm-5.2",
    ))];
    model.fleet.detail = Some(AgentId::new("ag-recon"));
    model.handle(key(KeyCode::Char('d')));
    assert!(model.fleet.kill_armed.is_some(), "armed");
    model.handle(key(KeyCode::Down));
    assert!(
        model.fleet.kill_armed.is_none(),
        "a movement key disarms the destroy"
    );
    model.handle(key(KeyCode::Char('d')));
    model.handle(key(KeyCode::Esc));
    assert!(model.fleet.kill_armed.is_none(), "esc disarms too");
    assert!(
        model.requests.is_empty(),
        "and nothing was destroyed along the way: {:?}",
        model.requests
    );
}

/// A member that is NOT one of this session's chips cannot be destroyed
/// from here, and the refusal says so rather than pretending.
#[test]
fn a_foreign_member_refuses_the_destroy_honestly() {
    let mut model = fleet_model(snapshot(mixed_tree()));
    model.fleet.detail = Some(AgentId::new("ag-recon"));
    model.handle(key(KeyCode::Char('d')));
    model.handle(key(KeyCode::Char('d')));
    assert!(
        model.requests.is_empty(),
        "nothing was sent for a member this view does not own: {:?}",
        model.requests
    );
    assert!(
        model
            .flash
            .as_deref()
            .is_some_and(|flash| flash.contains("cannot destroy")),
        "the refusal is explicit: {:?}",
        model.flash
    );
}
