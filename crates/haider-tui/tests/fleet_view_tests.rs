//! Fleet view slice 1 — the session-born full-screen fleet (`crate::fleet`).
//!
//! Pinned laws:
//!
//! * ENTRY — <5 tree nodes keep the inline per-chip rows; ≥5 collapse to
//!   ONE `⣿ … · ⌥F fleet` summary row, and ⌥F opens the view for the
//!   current session (never a launcher/menu entry);
//! * DENSITY — ≤20 nodes at the current root render the tree list, >20 the
//!   max-density matrix grid; the switch is automatic (no manual toggle);
//! * DRILL — ⏎ on a node WITH children re-roots the view on that subtree
//!   (the header shows the path), esc walks up one level, esc at the root
//!   closes back to the session;
//! * ROLLUP — the client arithmetic (`fleet of N · ✓a ◉b ✗c ◌d · depth e`)
//!   agrees with the daemon's wire rollup over the same nodes;
//! * TRUNCATION — a bounded snapshot renders its honest footer witness;
//! * PLAIN PARITY — `fleet_plain` carries the same information as the
//!   styled screen;
//! * METRICS — the row metric wears the S4 cost vocabulary (OAuth lanes
//!   keep the labeled `≈$` API-equivalent form) and unknown is never
//!   rendered as zero;
//! * LIVE — the `session.fleet` read is single-flight with an event chase,
//!   and a stale reply for another session installs nothing.
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
use haider_tui::app::{AppEvent, AppModel, AppRequest, ChipModel, RuntimeMode, Screen};
use haider_tui::fleet;
use haider_tui::live::{LiveCommand, LiveDriver, LiveReply};
use haider_tui::render::render;
use haider_tui::script::ChipDisplayState;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

mod common;
use common::{key, launcher_model};

const SPAWN_MS: u64 = 1_000_000_000;

fn sid() -> SessionId {
    SessionId::new("s-fleet")
}

// ------------------------------------------------------------ fixtures ----

fn manifest(agent: &str, callsign: &str, task: &str) -> AgentManifest {
    AgentManifest {
        agent: AgentId::new(agent),
        role: AgentRole::Subagent,
        task: task.to_owned(),
        callsign: Some(callsign.to_owned()),
        model_profile: "fable-5".to_owned(),
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
    }
}

fn chip(agent: &str, callsign: &str, state: ChipDisplayState) -> ChipModel {
    let mut chip = ChipModel::from_manifest(&manifest(agent, callsign, "task"));
    chip.state = state;
    chip.device = "test-box".into();
    chip
}

fn oauth_usage(cost_microusd: u64) -> AgentUsageMetrics {
    AgentUsageMetrics {
        logical_input_tokens: 1_200,
        billed_output_tokens: 300,
        cache_read_tokens: 800,
        cache_write_tokens: 25,
        cache_hit_basis_points: Some(8_000),
        metered_cost_microusd: None,
        api_equivalent_cost_microusd: Some(cost_microusd),
        all_lanes_priced: true,
        has_metered_lanes: false,
        has_oauth_lanes: true,
        breakdowns: vec![AgentUsageBreakdown {
            provider: "anthropic".into(),
            model: "fable-5".into(),
            cache_epoch: "epoch-fleet".into(),
            request_kind: UsageRequestKind::DelegatedAgent,
            auth_method: Some(AuthMethod::OAuth),
            logical_input_tokens: 1_200,
            billed_output_tokens: 300,
            cache_read_tokens: 800,
            cache_write_tokens: 25,
            metered_cost_microusd: None,
            api_equivalent_cost_microusd: Some(cost_microusd),
            priced: true,
            ..AgentUsageBreakdown::default()
        }],
        ..AgentUsageMetrics::default()
    }
}

fn metrics(agent: &str, usage: Option<AgentUsageMetrics>) -> AgentMetricsSnapshot {
    AgentMetricsSnapshot {
        agent: Some(AgentId::new(agent)),
        session_id: SessionId::new(format!("child-{agent}")),
        head_seq: 9,
        started_at_ms: SPAWN_MS,
        terminal_at_ms: Some(SPAWN_MS + 42_000),
        live: false,
        tool_attempts: 3,
        usage,
    }
}

fn node(
    agent: &str,
    callsign: &str,
    task: &str,
    depth: u32,
    state: FleetAgentStateWire,
    children: Vec<FleetNodeWire>,
) -> FleetNodeWire {
    FleetNodeWire {
        agent_id: AgentId::new(agent),
        session_id: SessionId::new(format!("child-{agent}")),
        callsign: Some(callsign.to_owned()),
        task: task.to_owned(),
        depth,
        parent_session_id: sid(),
        parent_agent_id: None,
        state,
        metrics: Some(metrics(agent, Some(oauth_usage(420_000)))),
        folded_children: 0,
        children,
    }
}

/// A daemon-shaped snapshot: the wire rollup describes exactly the nodes
/// present (the daemon's `fleet_snapshot` contract).
fn snapshot(roots: Vec<FleetNodeWire>, truncated: bool) -> SessionFleetSnapshot {
    let roll = fleet::rollup(&roots);
    SessionFleetSnapshot {
        session_id: sid(),
        generated_at_ms: SPAWN_MS + 60_000,
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
            complete: !truncated,
        },
        roots,
        truncated,
    }
}

/// alpha (live, 3 children incl. one grandchild) + beta (done leaf):
/// 6 nodes, depths 1-3, one of every headline state.
fn drill_tree() -> Vec<FleetNodeWire> {
    vec![
        node(
            "ag-alpha",
            "alpha",
            "coordinate the sweep",
            1,
            FleetAgentStateWire::Live,
            vec![
                node(
                    "ag-a1",
                    "recon",
                    "map the seams",
                    2,
                    FleetAgentStateWire::Done,
                    vec![],
                ),
                node(
                    "ag-a2",
                    "probe",
                    "exercise the edges",
                    2,
                    FleetAgentStateWire::Failed,
                    vec![node(
                        "ag-a2x",
                        "shim",
                        "retry the edge",
                        3,
                        FleetAgentStateWire::Queued,
                        vec![],
                    )],
                ),
                node(
                    "ag-a3",
                    "weld",
                    "land the fix",
                    2,
                    FleetAgentStateWire::Live,
                    vec![],
                ),
            ],
        ),
        node(
            "ag-beta",
            "beta",
            "verify the fix",
            1,
            FleetAgentStateWire::Done,
            vec![],
        ),
    ]
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

fn session_model_with_chips(count: usize) -> AppModel {
    let mut model = launcher_model();
    model.screen = Screen::Session;
    for index in 0..count {
        model.chips.push(chip(
            &format!("ag-{index}"),
            &format!("unit{index}"),
            ChipDisplayState::Idle,
        ));
    }
    model
}

fn fleet_model(snapshot: SessionFleetSnapshot) -> AppModel {
    let mut model = launcher_model();
    model.screen = Screen::Fleet;
    model.fleet.snapshot = Some(snapshot);
    model
}

// ------------------------------------------------------------ thresholds ----

#[test]
fn thresholds_are_the_frozen_spec_values() {
    assert_eq!(fleet::ENTRY_COLLAPSE, 5, "entry-row collapse threshold");
    assert_eq!(fleet::GRID_THRESHOLD, 20, "list→grid density threshold");
    assert_eq!(fleet::density(20), fleet::Density::List);
    assert_eq!(fleet::density(21), fleet::Density::Grid);
}

// ----------------------------------------------------------------- entry ----

#[test]
fn under_five_children_keep_inline_rows() {
    let model = session_model_with_chips(4);
    let rows = draw_rows(&model, 100, 32);
    let all = rows.join("\n");
    assert!(all.contains("unit0"), "inline chip rows stay: {all}");
    assert!(all.contains("unit3"), "inline chip rows stay: {all}");
    assert!(
        !all.contains("⣿"),
        "no summary row under the threshold: {all}"
    );
}

#[test]
fn five_children_collapse_to_one_summary_row() {
    let model = session_model_with_chips(5);
    let rows = draw_rows(&model, 100, 32);
    let all = rows.join("\n");
    let summary = rows
        .iter()
        .find(|row| row.contains("⣿"))
        .cloned()
        .expect("the summary row renders");
    assert!(
        summary.contains("5 subagents"),
        "the count is the tree total: {summary}"
    );
    assert!(summary.contains("⌥F fleet"), "the door is named: {summary}");
    assert!(
        !all.contains("unit0") && !all.contains("unit4"),
        "per-chip rows are replaced whole: {all}"
    );
}

#[test]
fn entry_summary_counts_every_bucket_honestly() {
    let chips = vec![
        chip("ag-0", "done0", ChipDisplayState::Done),
        chip("ag-1", "err1", ChipDisplayState::Error),
        chip("ag-2", "run2", ChipDisplayState::Running),
        chip("ag-3", "wait3", ChipDisplayState::Waiting),
        chip("ag-4", "idle4", ChipDisplayState::Idle),
    ];
    let roll = fleet::entry_rollup(&chips);
    assert_eq!(
        fleet::entry_summary(&roll),
        "5 subagents · 1 done · 1 live · 1 waiting · 1 failed · 1 queued · ⌥F fleet"
    );
}

#[test]
fn alt_f_opens_the_fleet_for_the_current_session() {
    let mut model = session_model_with_chips(5);
    model.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Char('f'),
        KeyModifiers::ALT,
    )));
    assert_eq!(model.screen, Screen::Fleet, "⌥F opens the fleet view");
    let snapshot = model
        .fleet
        .snapshot
        .as_ref()
        .expect("demo synthesizes at open");
    assert_eq!(snapshot.rollup.node_count, 5, "one node per chip");
    // Esc at the root closes back to the session — the entry is a door,
    // not a destination.
    model.handle(key(KeyCode::Esc));
    assert_eq!(model.screen, Screen::Session);
}

#[test]
fn alt_f_without_a_fleet_keeps_the_composer_word_motion() {
    let mut model = launcher_model();
    model.screen = Screen::Session;
    assert!(!model.fleet_available());
    model.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Char('f'),
        KeyModifiers::ALT,
    )));
    assert_eq!(
        model.screen,
        Screen::Session,
        "no subagents — ⌥f stays readline word-right"
    );
}

#[test]
fn live_open_gates_on_the_daemon_feature() {
    let mut model = session_model_with_chips(5);
    model.mode = RuntimeMode::Live;
    model.active_session = Some(sid());
    // An old daemon: the open refuses honestly, nothing switches.
    model.open_fleet();
    assert_eq!(model.screen, Screen::Session);
    assert!(
        model
            .flash
            .as_deref()
            .is_some_and(|flash| flash.contains("session_fleet_v1")),
        "the refusal names the missing feature: {:?}",
        model.flash
    );
    // A serving daemon: the read is requested, the screen switches, and
    // the honest fetching state shows until the snapshot lands.
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_SESSION_FLEET_V1.to_owned());
    model.open_fleet();
    assert_eq!(model.screen, Screen::Fleet);
    assert!(model.fleet.fetching);
    assert!(
        model.fleet.snapshot.is_none(),
        "live never fabricates a tree"
    );
    assert!(
        model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::FleetRefresh)),
        "the session.fleet read is requested at open"
    );
    let rows = draw_rows(&model, 100, 32);
    assert!(
        rows.iter().any(|row| row.contains("fetching fleet…")),
        "the in-flight read renders honestly: {rows:?}"
    );
}

// --------------------------------------------------------------- density ----

#[test]
fn twenty_nodes_render_the_list_twenty_one_the_grid() {
    let nodes = |count: usize| -> Vec<FleetNodeWire> {
        (0..count)
            .map(|index| {
                node(
                    &format!("ag-{index}"),
                    &format!("cell{index}"),
                    "work",
                    1,
                    FleetAgentStateWire::Done,
                    vec![],
                )
            })
            .collect()
    };
    let list_model = fleet_model(snapshot(nodes(20), false));
    let list = draw_rows(&list_model, 100, 40).join("\n");
    assert!(list.contains("fleet of 20"), "rollup header: {list}");
    assert!(
        list.contains("✓ cell0"),
        "list rows carry the glyph: {list}"
    );
    assert!(!list.contains('●'), "no matrix dots in the list: {list}");

    let mut grid_nodes = nodes(21);
    grid_nodes[0].folded_children = 4;
    let grid_model = fleet_model(snapshot(grid_nodes, true));
    let grid = draw_rows(&grid_model, 100, 40).join("\n");
    assert!(grid.contains("fleet of 21"), "rollup header: {grid}");
    assert!(
        grid.contains('●'),
        "grid cells wear the matrix dots: {grid}"
    );
    assert!(
        grid.contains("cell0"),
        "the callsign sits under the cell: {grid}"
    );
    assert!(
        grid.contains("⊞4"),
        "a folded grid cell carries its per-node witness: {grid}"
    );
    assert!(
        grid_model.fleet.grid_cols.get() > 1,
        "render measured the grid geometry"
    );
}

// ------------------------------------------------------------ drill-down ----

#[test]
fn enter_reroots_esc_walks_up_and_out() {
    let mut model = fleet_model(snapshot(drill_tree(), false));
    let root = draw_rows(&model, 100, 32).join("\n");
    assert!(root.contains("fleet of 6"), "root rollup: {root}");
    assert!(
        root.contains("beta"),
        "the sibling renders at the root: {root}"
    );

    // ⏎ on alpha (sel 0, has children) re-roots on its subtree.
    model.handle(key(KeyCode::Enter));
    assert_eq!(model.fleet.stack.len(), 1, "re-rooted one level down");
    let drilled = draw_rows(&model, 100, 32).join("\n");
    assert!(
        drilled.contains("› alpha"),
        "the header shows the path: {drilled}"
    );
    assert!(
        drilled.contains("fleet of 4"),
        "the rollup is the SUBTREE's: {drilled}"
    );
    assert!(
        !drilled.contains("beta"),
        "the sibling left the view: {drilled}"
    );
    assert!(
        drilled.contains("recon"),
        "the children are the rows: {drilled}"
    );

    // ⏎ on a leaf drills nowhere — the refusal is honest.
    model.handle(key(KeyCode::Enter));
    assert_eq!(model.fleet.stack.len(), 1, "a leaf never re-roots");

    // esc walks UP one level, then OUT from the root.
    model.handle(key(KeyCode::Esc));
    assert!(model.fleet.stack.is_empty(), "esc pops one drill level");
    assert_eq!(model.screen, Screen::Fleet);
    model.handle(key(KeyCode::Esc));
    assert_eq!(
        model.screen,
        Screen::Session,
        "root esc closes to the session"
    );
}

// ---------------------------------------------------------------- rollup ----

#[test]
fn rollup_arithmetic_agrees_with_the_wire() {
    let roots = drill_tree();
    let roll = fleet::rollup(&roots);
    assert_eq!(
        (
            roll.total,
            roll.done,
            roll.live,
            roll.failed,
            roll.queued,
            roll.max_depth
        ),
        (6, 2, 2, 1, 1, 3),
        "client arithmetic over the fixture"
    );
    let wire = snapshot(roots, false).rollup;
    assert_eq!(wire.node_count as usize, roll.total);
    assert_eq!(wire.states.done as usize, roll.done);
    assert_eq!(wire.states.live as usize, roll.live);
    assert_eq!(wire.states.failed as usize, roll.failed);
    assert_eq!(wire.states.queued as usize, roll.queued);
    assert_eq!(wire.states.waiting as usize, roll.waiting);
    assert_eq!(wire.states.cancelled as usize, roll.cancelled);
    assert_eq!(wire.max_depth, roll.max_depth);
    assert_eq!(
        fleet::header_line(&roll),
        "fleet of 6 · ✓2 ◉2 ✗1 ◌1 · depth 3",
        "the frozen header grammar"
    );
}

// ------------------------------------------------------------ truncation ----

#[test]
fn truncation_witness_renders_an_honest_footer() {
    let mut roots = drill_tree();
    roots[0].folded_children = 2;
    let bounded = fleet_model(snapshot(roots, true));
    let rows = draw_rows(&bounded, 100, 32).join("\n");
    assert!(
        rows.contains("alpha ⊞2"),
        "the row names the exact omitted direct-child count: {rows}"
    );
    assert!(
        !rows.contains("alpha ▸3"),
        "the fold witness replaces the returned-child marker: {rows}"
    );
    assert!(
        rows.contains("512-node view cap reached — deepest branches folded"),
        "the styled footer: {rows}"
    );
    let plain = fleet::fleet_plain(&bounded);
    assert!(plain.contains("alpha ⊞2"), "the plain fold marker: {plain}");
    assert!(
        plain.contains("512-node view cap reached"),
        "the plain footer"
    );

    let complete = fleet_model(snapshot(drill_tree(), false));
    let rows = draw_rows(&complete, 100, 32).join("\n");
    assert!(
        !rows.contains("view cap"),
        "no witness when nothing was folded: {rows}"
    );
}

// ---------------------------------------------------------------- plain ----

#[test]
fn plain_parity_carries_the_styled_information() {
    let model = fleet_model(snapshot(drill_tree(), false));
    let plain = fleet::fleet_plain(&model);
    assert!(
        plain.contains("FLEET — session › fleet"),
        "the crumb: {plain}"
    );
    assert!(
        plain.contains("fleet of 6 · ✓2 ◉2 ✗1 ◌1 · depth 3"),
        "the rollup header: {plain}"
    );
    for name in ["alpha", "recon", "probe", "shim", "weld", "beta"] {
        assert!(
            plain.contains(name),
            "every node renders: {name} in {plain}"
        );
    }
    assert!(
        plain.contains("✓ recon — map the seams · 3t · 1.5k · ≈$0.42"),
        "the row grammar with the labeled ≈$ cost: {plain}"
    );
    assert!(
        plain.contains("◌ shim"),
        "queued keeps its glyph in plain: {plain}"
    );
    assert!(plain.contains("alpha ▸3"), "real children marker: {plain}");
    assert!(
        plain.contains("probe ▸1"),
        "nested children marker: {plain}"
    );

    // The drilled view's plain twin follows the re-root.
    let mut model = model;
    model.fleet.stack.push(AgentId::new("ag-alpha"));
    let plain = fleet::fleet_plain(&model);
    assert!(plain.contains("› alpha"), "the drilled path: {plain}");
    assert!(plain.contains("fleet of 4"), "the subtree rollup: {plain}");
    assert!(
        !plain.contains("beta"),
        "the sibling left the plain view too"
    );
}

#[test]
fn plain_renders_the_fetching_and_failed_states_honestly() {
    let mut model = launcher_model();
    model.screen = Screen::Fleet;
    model.fleet.fetching = true;
    assert!(fleet::fleet_plain(&model).contains("fetching fleet…"));
    model.fleet.fetching = false;
    model.fleet.error = Some("overloaded".to_owned());
    assert!(fleet::fleet_plain(&model).contains("✗ fleet read failed — overloaded"));
}

// --------------------------------------------------------------- metrics ----

#[test]
fn row_metric_wears_the_s4_cost_vocabulary() {
    let priced = node("ag-m", "m", "t", 1, FleetAgentStateWire::Done, vec![]);
    assert_eq!(
        fleet::node_metric(&priced),
        "3t · 1.5k · ≈$0.42",
        "OAuth lanes keep the labeled API-equivalent form"
    );
    let mut queued = priced.clone();
    queued.state = FleetAgentStateWire::Queued;
    assert_eq!(fleet::node_metric(&queued), "queued");
    let mut unpriced = priced.clone();
    unpriced.metrics = Some(metrics("ag-m", None));
    assert_eq!(
        fleet::node_metric(&unpriced),
        "3t",
        "no usage truth — the token/cost segments DROP, never a zero"
    );
    let mut bare = priced;
    bare.metrics = None;
    assert_eq!(
        fleet::node_metric(&bare),
        "",
        "no metrics — no fabricated figures"
    );
}

// ---------------------------------------------------------------- matrix ----

#[test]
fn matrix_pattern_is_deterministic_and_never_empty() {
    let bits = fleet::matrix_bits("ag-alpha");
    assert_eq!(
        bits,
        fleet::matrix_bits("ag-alpha"),
        "same id, same pattern"
    );
    assert_eq!(bits & 0x42, 0x42, "the 0x42 floor keeps a pattern lit");
    assert_ne!(
        fleet::matrix_bits("ag-alpha"),
        fleet::matrix_bits("ag-beta"),
        "distinct ids draw distinct patterns (for these fixtures)"
    );
    let [top, bottom] = fleet::matrix_rows(bits);
    assert_eq!(top.chars().count(), 4);
    assert_eq!(bottom.chars().count(), 4);
    assert!(
        top.chars()
            .chain(bottom.chars())
            .all(|dot| dot == '●' || dot == '·')
    );
}

// ------------------------------------------------------------------ live ----

#[test]
fn fleet_read_is_single_flight_with_one_chase() {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.active_session = Some(sid());
    model.screen = Screen::Fleet;
    let mut driver = LiveDriver::new("t-fleet");

    let first = driver.handle_request(&mut model, AppRequest::FleetRefresh);
    assert_eq!(
        first,
        vec![LiveCommand::SessionFleet { session: sid() }],
        "the open issues the read"
    );
    // Event-cadence asks while the read flies FOLD into one chase.
    assert!(
        driver
            .handle_request(&mut model, AppRequest::FleetRefresh)
            .is_empty(),
        "single-flight: no second read while one is outstanding"
    );
    assert!(
        driver
            .handle_request(&mut model, AppRequest::FleetRefresh)
            .is_empty(),
        "bursts keep folding"
    );
    let chased = driver.apply(
        &mut model,
        LiveReply::Fleet {
            snapshot: Box::new(snapshot(drill_tree(), false)),
        },
    );
    assert_eq!(
        chased,
        vec![LiveCommand::SessionFleet { session: sid() }],
        "the folded chase re-reads ONCE when the reply lands"
    );
    assert!(model.fleet.snapshot.is_some(), "the snapshot installed");
    let quiet = driver.apply(
        &mut model,
        LiveReply::Fleet {
            snapshot: Box::new(snapshot(drill_tree(), false)),
        },
    );
    assert!(quiet.is_empty(), "no chase without a new ask");
}

#[test]
fn fleet_read_failure_lands_on_the_screen() {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.active_session = Some(sid());
    model.screen = Screen::Fleet;
    model.fleet.fetching = true;
    let mut driver = LiveDriver::new("t-fleet");
    let _ = driver.handle_request(&mut model, AppRequest::FleetRefresh);
    let commands = driver.apply(
        &mut model,
        LiveReply::FleetFailed {
            message: "overloaded".to_owned(),
        },
    );
    assert!(commands.is_empty());
    assert!(!model.fleet.fetching);
    assert_eq!(model.fleet.error.as_deref(), Some("overloaded"));
    let rows = draw_rows(&model, 100, 32).join("\n");
    assert!(rows.contains("fleet read failed — overloaded"), "{rows}");
}

#[test]
fn stale_reply_for_another_session_installs_nothing() {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.active_session = Some(SessionId::new("s-other"));
    model.screen = Screen::Fleet;
    model.apply_fleet_snapshot(snapshot(drill_tree(), false));
    assert!(
        model.fleet.snapshot.is_none(),
        "a snapshot for a session no longer attached installs nothing"
    );
}
