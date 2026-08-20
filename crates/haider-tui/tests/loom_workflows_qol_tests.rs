//! W-flow — the Loom/Workflows QOL wave: the /workflows pane's fixed-head
//! row space (`∅ none` first and undeletable, then the built-in catalog
//! pair, then the REGISTERED section), `p` pin-by-name to the bound
//! session, the `n` describe-it authoring input (⏎ submits the instruction
//! turn, ⌥m picks the authoring model), the fleet's typed-agent accent, and
//! the pane-entry snapshot re-request.
#![allow(clippy::expect_used)]

use haider_protocol::graph::{GraphPhase, GraphStatus};
use haider_protocol::ids::{GraphId, SessionId};
use haider_protocol::loom::{LoomAgentType, LoomTypeSig, compile_pipe, parse_pipe};
use haider_rpc::{
    FLEET_MAX_DEPTH, FLEET_MAX_NODES, FleetAgentStateWire, FleetMetricsTotalsWire, FleetNodeWire,
    FleetRollupWire, FleetStateCountsWire, RequestBody, SessionFleetSnapshot,
};
use haider_tui::app::{AppModel, AppRequest, LoomPane, RuntimeMode, Screen, WorkflowRow};
use haider_tui::fleet;
use haider_tui::link::request_body;
use haider_tui::live::{LiveCommand, LiveDriver, LiveReply};
use haider_tui::render::render;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

mod common;
use common::{key, launcher_model, run_slash, submit};

fn sid() -> SessionId {
    SessionId::new("s-loomflow")
}

/// A live model with the Loom + Convergence Graph features served and one
/// bound session — the ground every pin/authoring law stands on.
fn live_bound_model() -> AppModel {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.daemon_features = [
        haider_rpc::FEATURE_LOOM_V1.to_owned(),
        haider_rpc::FEATURE_CONVERGENCE_GRAPH_V1.to_owned(),
    ]
    .into_iter()
    .collect();
    model.daemon_version = Some("0.0.933".to_owned());
    model.sessions.clear();
    model.upsert_live_session(&sid());
    model.open_session(&sid());
    model.loom_loaded = true;
    model
}

fn researcher() -> LoomAgentType {
    LoomAgentType {
        id: "researcher".into(),
        name: "Researcher".into(),
        job: "Pull a source and transcribe it.".into(),
        in_type: "SourceURL".into(),
        out_type: "Transcript".into(),
        clis: vec!["yt-dlp".into()],
        apis: Vec::new(),
        skills: Vec::new(),
        scripts: Vec::new(),
        color: "#c2701c".into(),
        glyph: "▲".into(),
        rev: 1,
    }
}

fn clip_workflow() -> haider_protocol::loom::LoomWorkflow {
    compile_pipe(
        &parse_pipe(
            "clip: SourceURL -> Transcript\nresearch @researcher \"pull and transcribe\" :cmd",
        ),
        |id| {
            (id == "researcher").then(|| LoomTypeSig {
                in_type: "SourceURL".into(),
                out_type: "Transcript".into(),
            })
        },
    )
    .expect("compiles")
}

/// A minimal ACTIVE graph reduction — enough truth for the abandon path.
fn active_graph() -> GraphStatus {
    GraphStatus {
        graph_id: GraphId::new("graph-active"),
        template: "ship-loop".into(),
        digest: "digest-active".into(),
        template_version: 1,
        start_node: None,
        phase: GraphPhase::Active,
        current_node: None,
        ready_nodes: Vec::new(),
        attempt: 1,
        nodes: Vec::new(),
        blocked_reason: None,
        pending_menu: None,
        pending_menus: Vec::new(),
        run_set: None,
    }
}

fn draw(model: &AppModel) -> (Vec<String>, Vec<Vec<Option<ratatui::style::Color>>>) {
    let backend = TestBackend::new(110, 32);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| {
            render(model, frame);
        })
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    let mut rows = Vec::new();
    let mut colors = Vec::new();
    for y in 0..buffer.area.height {
        let mut text = String::new();
        let mut row_colors = Vec::new();
        for x in 0..buffer.area.width {
            text.push_str(buffer[(x, y)].symbol());
            row_colors.push(buffer[(x, y)].style().fg);
        }
        rows.push(text);
        colors.push(row_colors);
    }
    (rows, colors)
}

fn drain(driver: &mut LiveDriver, model: &mut AppModel) -> Vec<LiveCommand> {
    let requests: Vec<AppRequest> = model.requests.drain(..).collect();
    let mut commands = Vec::new();
    for request in requests {
        commands.extend(driver.handle_request(model, request));
    }
    commands
}

// ---- item 1: the /workflows fixed-head row space ------------------------

/// MUTATION CHECK: reorder the /workflows rows (registered before the
/// head), drop the synthetic `∅ none` row, or source it from the registry
/// (making it deletable). Expected RUNTIME failure: the order assertion
/// below, or the empty-registry assertion that `none` + built-ins survive a
/// registry with zero records.
#[test]
fn workflows_pane_leads_with_none_then_builtins_then_registered() {
    let mut model = launcher_model();
    submit(&mut model, "open the workflows");
    model.loom_types = vec![researcher()];
    model.loom_workflows = vec![clip_workflow()];
    model.screen = Screen::Loom;
    model.loom_pane = LoomPane::Workflows;

    let (rows, _) = draw(&model);
    let position = |needle: &str| {
        rows.iter()
            .position(|row| row.contains(needle))
            .unwrap_or_else(|| panic!("`{needle}` missing:\n{}", rows.join("\n")))
    };
    let none = position("∅ none — no flow · default");
    let ship = position("⛩ ship-loop");
    let super_ship = position("⛩ super-ship-loop");
    let registered = position("REGISTERED");
    let clip = position("@clip");
    assert!(
        none < ship && ship < super_ship && super_ship < registered && registered < clip,
        "fixed-head order broken:\n{}",
        rows.join("\n")
    );
    assert!(
        rows[ship].contains("built-in") && rows[super_ship].contains("built-in"),
        "built-ins must be marked:\n{}",
        rows.join("\n")
    );
    // The footer carries the new verbs.
    assert!(
        rows.iter()
            .any(|row| row.contains("p pin to session") && row.contains("n new")),
        "workflows footer missing p/n:\n{}",
        rows.join("\n")
    );

    // The row-space authority agrees with the paint: none, 2 built-ins,
    // then the registered record.
    assert_eq!(model.workflow_row_count(), 4);
    assert_eq!(model.workflow_row(0), Some(WorkflowRow::None));
    assert!(matches!(
        model.workflow_row(1),
        Some(WorkflowRow::BuiltIn(template)) if template.name == "ship-loop"
    ));
    assert!(matches!(
        model.workflow_row(2),
        Some(WorkflowRow::BuiltIn(template)) if template.name == "super-ship-loop"
    ));
    assert_eq!(model.workflow_row(3), Some(WorkflowRow::Registered(0)));

    // An EMPTY registry never empties the pane — the head stays, and the
    // registry emptiness moves inside the REGISTERED section (reworded).
    model.loom_workflows.clear();
    let (rows, _) = draw(&model);
    assert!(
        rows.iter().any(|row| row.contains("∅ none")),
        "the synthetic default must survive an empty registry:\n{}",
        rows.join("\n")
    );
    assert!(
        rows.iter()
            .any(|row| row.contains("none registered — press n")),
        "the REGISTERED section carries the reworded empty line:\n{}",
        rows.join("\n")
    );
    assert!(
        !rows
            .iter()
            .any(|row| row.contains("no workflows registered")),
        "the old pane-level empty state must not return:\n{}",
        rows.join("\n")
    );
}

/// MUTATION CHECK: drop the built-in detail derivation (nodes/gates from
/// the GraphTemplateSpec) or the `none` detail one-liner. Expected RUNTIME
/// failure: the node/gate assertions or the default-line assertion below.
#[test]
fn builtin_and_none_details_render_from_the_catalog() {
    let mut model = launcher_model();
    submit(&mut model, "inspect the built-ins");
    model.screen = Screen::Loom;
    model.loom_pane = LoomPane::Workflows;

    // ∅ none — one honest line, no fabricated graph.
    model.loom_selection = 0;
    model.loom_detail = true;
    let (rows, _) = draw(&model);
    let all = rows.join("\n");
    assert!(
        all.contains("every session starts here — no graph, no gates"),
        "none detail missing:\n{all}"
    );

    // super-ship-loop — the five owner stages with their gates.
    model.loom_selection = 2;
    model.loom_scroll = 0;
    let (rows, _) = draw(&model);
    let all = rows.join("\n");
    for needle in [
        "super-ship-loop",
        "IMPLEMENT",
        "TESTS",
        "CLEAN",
        "OPTIMIZE",
        "SHIP",
        "command-green",
        "all-of-2",
        "human-confirm",
        "← after TESTS+CLEAN",
    ] {
        assert!(all.contains(needle), "`{needle}` missing:\n{all}");
    }
}

// ---- item 2: `p` pins the selected row to the bound session -------------

/// MUTATION CHECK: drop the template name from the widened GraphPin (or
/// resolve the wrong row). Expected RUNTIME failure: the AppRequest
/// assertion, the LiveCommand assertion, or the wire-body template below.
#[test]
fn p_pin_carries_the_selected_template_name_on_the_wire() {
    let mut model = live_bound_model();
    model.screen = Screen::Loom;
    model.loom_pane = LoomPane::Workflows;
    model.loom_selection = 2; // super-ship-loop
    model.requests.clear();

    model.handle(key(KeyCode::Char('p')));
    assert!(
        model.requests.iter().any(|request| matches!(
            request,
            AppRequest::GraphPin { template: Some(name) } if name == "super-ship-loop"
        )),
        "p must request a pin CARRYING the selected template name: {:?}",
        model.requests
    );
    assert_eq!(
        model.flash.as_deref(),
        Some("· pinning super-ship-loop…"),
        "issuance flash names the template"
    );

    let mut driver = LiveDriver::new("test");
    let commands = drain(&mut driver, &mut model);
    let pin = commands
        .into_iter()
        .find(|command| matches!(command, LiveCommand::GraphPin { .. }))
        .expect("a GraphPin command");
    match request_body(pin) {
        RequestBody::GraphPin { template, .. } => {
            assert_eq!(template, "super-ship-loop", "the wire pin is BY NAME");
        }
        other => panic!("wrong wire body: {other:?}"),
    }

    // A registered workflow row pins by ITS name too.
    model.loom_workflows = vec![clip_workflow()];
    model.loom_selection = 3;
    model.requests.clear();
    model.handle(key(KeyCode::Char('p')));
    assert!(
        model.requests.iter().any(|request| matches!(
            request,
            AppRequest::GraphPin { template: Some(name) } if name == "clip"
        )),
        "registered rows pin by registry id: {:?}",
        model.requests
    );

    // The legacy caller (`/graph pin`, template: None) keeps the ship-loop
    // fallback at the link seam.
    model.requests.clear();
    model.requests.push(AppRequest::GraphPin { template: None });
    let commands = drain(&mut driver, &mut model);
    let pin = commands
        .into_iter()
        .find(|command| matches!(command, LiveCommand::GraphPin { .. }))
        .expect("a GraphPin command");
    match request_body(pin) {
        RequestBody::GraphPin { template, .. } => {
            assert_eq!(template, "ship-loop", "None keeps the legacy fallback");
        }
        other => panic!("wrong wire body: {other:?}"),
    }
}

/// MUTATION CHECK: make `p` on the `∅ none` row pin something (or no-op
/// silently over an active graph). Expected RUNTIME failure: the abandon
/// request assertion, or the already-none honest flash below.
#[test]
fn p_on_none_abandons_the_active_graph() {
    let mut model = live_bound_model();
    model.screen = Screen::Loom;
    model.loom_pane = LoomPane::Workflows;
    model.loom_selection = 0;
    model.graph = Some(active_graph());
    model.requests.clear();

    model.handle(key(KeyCode::Char('p')));
    assert!(
        model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::GraphAbandon { .. })),
        "none must abandon, never pin: {:?}",
        model.requests
    );
    assert!(
        !model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::GraphPin { .. })),
        "none must never issue a pin"
    );

    // Without a graph the row is already truth — honest flash, no wire.
    model.graph = None;
    model.requests.clear();
    model.handle(key(KeyCode::Char('p')));
    assert!(model.requests.is_empty(), "already-none issues nothing");
    assert_eq!(
        model.flash.as_deref(),
        Some("· already none — no graph pinned")
    );
}

/// MUTATION CHECK: let an unbound (or demo) `p` reach the wire. Expected
/// RUNTIME failure: the no-request assertion or the honest-flash text.
#[test]
fn p_without_a_bound_session_flashes_honestly() {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.daemon_features = [
        haider_rpc::FEATURE_LOOM_V1.to_owned(),
        haider_rpc::FEATURE_CONVERGENCE_GRAPH_V1.to_owned(),
    ]
    .into_iter()
    .collect();
    model.loom_loaded = true;
    model.screen = Screen::Loom;
    model.loom_pane = LoomPane::Workflows;
    model.loom_selection = 1;
    model.requests.clear();

    model.handle(key(KeyCode::Char('p')));
    assert!(
        !model.requests.iter().any(|request| matches!(
            request,
            AppRequest::GraphPin { .. } | AppRequest::GraphAbandon { .. }
        )),
        "an unbound p must never reach the wire: {:?}",
        model.requests
    );
    assert_eq!(
        model.flash.as_deref(),
        Some("· pin — no bound session; open a session first"),
        "the flash names the fix"
    );
}

/// MUTATION CHECK: install pin/abandon truth locally instead of flashing
/// the RECEIPT, or swallow the daemon's one-active-graph refusal. Expected
/// RUNTIME failure: the receipt-flash assertions or the refusal-flash
/// carrying the daemon's message.
#[test]
fn graph_receipts_flash_and_refusals_carry_the_daemon_message() {
    let mut model = live_bound_model();
    let mut driver = LiveDriver::new("test");

    // Pin receipt → "· pinned X".
    model.requests.push(AppRequest::GraphPin {
        template: Some("super-ship-loop".to_owned()),
    });
    let commands = drain(&mut driver, &mut model);
    let LiveCommand::GraphPin { command_id, .. } = commands
        .into_iter()
        .find(|command| matches!(command, LiveCommand::GraphPin { .. }))
        .expect("a GraphPin command")
    else {
        unreachable!()
    };
    driver.apply(&mut model, LiveReply::GraphMutated { command_id });
    assert_eq!(model.flash.as_deref(), Some("· pinned super-ship-loop"));

    // Refused pin → the DAEMON's reason reaches the flash.
    model.requests.push(AppRequest::GraphPin {
        template: Some("clip".to_owned()),
    });
    let commands = drain(&mut driver, &mut model);
    let LiveCommand::GraphPin { command_id, .. } = commands
        .into_iter()
        .find(|command| matches!(command, LiveCommand::GraphPin { .. }))
        .expect("a GraphPin command")
    else {
        unreachable!()
    };
    driver.apply(
        &mut model,
        LiveReply::Failed {
            command_id: Some(command_id),
            code: "graph_active".to_owned(),
            message: "a graph is already active".to_owned(),
            retryable: false,
            presentation: None,
        },
    );
    assert_eq!(
        model.flash.as_deref(),
        Some("· pin clip refused — a graph is already active"),
        "the refusal flash carries the daemon's message"
    );

    // Abandon receipt → the cleared-workflow flash.
    model.requests.push(AppRequest::GraphAbandon {
        why: "test".to_owned(),
    });
    let commands = drain(&mut driver, &mut model);
    let LiveCommand::GraphAbandon { command_id, .. } = commands
        .into_iter()
        .find(|command| matches!(command, LiveCommand::GraphAbandon { .. }))
        .expect("a GraphAbandon command")
    else {
        unreachable!()
    };
    driver.apply(&mut model, LiveReply::GraphMutated { command_id });
    assert_eq!(model.flash.as_deref(), Some("· workflow cleared — none"));
}

// ---- item 3: `n` authoring input ----------------------------------------

/// MUTATION CHECK: break the input lifecycle — `n` no longer opens it, ⏎
/// no longer leaves for the session with the instruction turn, or esc no
/// longer cancels. Expected RUNTIME failure: the assertions at each step.
#[test]
fn n_authoring_input_submits_the_instruction_turn() {
    let mut model = live_bound_model();
    model.screen = Screen::Loom;
    model.loom_pane = LoomPane::Workflows;
    model.requests.clear();

    model.handle(key(KeyCode::Char('n')));
    assert_eq!(model.loom_input.as_deref(), Some(""), "n opens the input");
    let (rows, _) = draw(&model);
    let all = rows.join("\n");
    assert!(
        all.contains("new workflow — describe the flow"),
        "input title missing:\n{all}"
    );
    assert!(
        all.contains("⏎ send to model · ⌥m model · esc cancel"),
        "input footer missing:\n{all}"
    );

    // The description is typed VERBATIM — `p` and `n` are letters here,
    // never hotkeys.
    for c in "spin and pin".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    assert_eq!(model.loom_input.as_deref(), Some("spin and pin"));

    model.handle(key(KeyCode::Enter));
    assert_eq!(model.screen, Screen::Session, "⏎ leaves for the session");
    assert!(model.loom_input.is_none(), "the input closed");
    let turn = model
        .requests
        .iter()
        .find_map(|request| match request {
            AppRequest::SubmitText { text, .. } => Some(text.clone()),
            _ => None,
        })
        .expect("⏎ submits one ordinary turn");
    assert!(
        turn.contains("Draft a Loom workflow for: spin and pin"),
        "instruction must carry the description: {turn}"
    );
    assert!(
        turn.contains("loom_register") && turn.contains("pipe DSL"),
        "instruction must route through plan + loom_register: {turn}"
    );

    // The TYPES pane asks for an agent type instead.
    model.requests.clear();
    model.screen = Screen::Loom;
    model.loom_pane = LoomPane::Types;
    model.handle(key(KeyCode::Char('n')));
    for c in "sec auditor".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter));
    let turn = model
        .requests
        .iter()
        .find_map(|request| match request {
            AppRequest::SubmitText { text, .. } => Some(text.clone()),
            _ => None,
        })
        .expect("⏎ submits the type instruction");
    assert!(
        turn.contains("Draft a Loom agent type for: sec auditor") && turn.contains("color #rrggbb"),
        "type instruction wrong: {turn}"
    );

    // Esc cancels without leaving the loom screen or submitting.
    model.requests.clear();
    model.screen = Screen::Loom;
    model.handle(key(KeyCode::Char('n')));
    model.handle(key(KeyCode::Char('x')));
    model.handle(key(KeyCode::Esc));
    assert!(model.loom_input.is_none(), "esc cancels the input");
    assert_eq!(model.screen, Screen::Loom, "esc stays on the loom screen");
    assert!(
        !model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::SubmitText { .. })),
        "esc submits nothing"
    );
}

/// MUTATION CHECK: let an unbound ⏎ submit anyway. Expected RUNTIME
/// failure: the no-submit assertion or the honest flash naming the fix.
#[test]
fn n_submit_without_a_bound_session_flashes_honestly() {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.daemon_features = [haider_rpc::FEATURE_LOOM_V1.to_owned()]
        .into_iter()
        .collect();
    model.loom_loaded = true;
    model.screen = Screen::Loom;
    model.loom_pane = LoomPane::Types;
    model.requests.clear();

    model.handle(key(KeyCode::Char('n')));
    for c in "anything".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter));
    assert!(
        !model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::SubmitText { .. })),
        "no bound session — nothing may submit"
    );
    assert_eq!(
        model.flash.as_deref(),
        Some("· no bound session — open a session, then n"),
        "the flash names the fix"
    );
    assert_eq!(model.screen, Screen::Loom, "the input surface stays put");
}

/// MUTATION CHECK: drop the ⌥m hop (or route it into the buffer). Expected
/// RUNTIME failure: the picker-open assertion, the buffer-untouched
/// assertion, or the return-to-input assertion after esc.
#[test]
fn alt_m_from_the_input_opens_the_model_picker_and_returns() {
    let mut model = live_bound_model();
    model.screen = Screen::Loom;
    model.loom_pane = LoomPane::Workflows;
    model.handle(key(KeyCode::Char('n')));
    for c in "flow".chars() {
        model.handle(key(KeyCode::Char(c)));
    }

    model.handle(haider_tui::app::AppEvent::Key(KeyEvent::new(
        KeyCode::Char('m'),
        KeyModifiers::ALT,
    )));
    assert!(
        model.model_picker.is_some(),
        "⌥m opens the model picker over the loom screen"
    );
    assert_eq!(
        model.loom_input.as_deref(),
        Some("flow"),
        "⌥m never lands in the description"
    );

    // Esc closes the picker and the authoring input is still there.
    model.handle(key(KeyCode::Esc));
    assert!(model.model_picker.is_none(), "esc closes the picker");
    assert_eq!(model.screen, Screen::Loom, "back on the loom screen");
    assert_eq!(
        model.loom_input.as_deref(),
        Some("flow"),
        "the input survives the picker round trip"
    );
}

// ---- item 4: typed agents are colored in the fleet ----------------------

fn fleet_node(task: &str) -> FleetNodeWire {
    FleetNodeWire {
        agent_id: haider_protocol::ids::AgentId::new("fleet-typed"),
        session_id: SessionId::new("child-fleet-typed"),
        callsign: Some("Ammar".to_owned()),
        task: task.to_owned(),
        depth: 1,
        parent_session_id: sid(),
        parent_agent_id: None,
        state: FleetAgentStateWire::Live,
        metrics: None,
        folded_children: 0,
        children: Vec::new(),
    }
}

fn fleet_snapshot(roots: Vec<FleetNodeWire>) -> SessionFleetSnapshot {
    let roll = fleet::rollup(&roots);
    SessionFleetSnapshot {
        session_id: sid(),
        generated_at_ms: 1_000,
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

/// MUTATION CHECK: drop the fleet row's typed parse, its feature gate, or
/// the fallback. Expected RUNTIME failure: the accent assertion for the
/// typed row, or the no-accent assertions for the ungated / unknown-type
/// rows (state styling must stand).
#[test]
fn fleet_rows_paint_the_typed_accent_and_fall_back() {
    let mut model = live_bound_model();
    model.loom_types = vec![researcher()];
    model.fleet.snapshot = Some(fleet_snapshot(vec![fleet_node(
        "@researcher · pull the transcript",
    )]));
    model.screen = Screen::Fleet;

    let accent = Some(ratatui::style::Color::Rgb(0xc2, 0x70, 0x1c));
    let (rows, colors) = draw(&model);
    let row = rows
        .iter()
        .position(|row| row.contains("@researcher"))
        .unwrap_or_else(|| panic!("typed fleet row missing:\n{}", rows.join("\n")));
    // Multi-byte glyphs precede the matches: byte offsets must fold to
    // CELL columns before indexing the color grid.
    let cell = |byte: usize| rows[row][..byte].chars().count();
    let column = cell(rows[row].find("@researcher").expect("column"));
    assert_eq!(
        colors[row][column], accent,
        "the fleet row's @type segment wears the Loom accent"
    );
    let glyph_column = cell(rows[row].find('▲').expect("type glyph column"));
    assert_eq!(colors[row][glyph_column], accent, "the type glyph too");
    // The STATE glyph keeps meaning state — it never takes the accent.
    let state_column = cell(rows[row].find('◉').expect("state glyph column"));
    assert_ne!(
        colors[row][state_column], accent,
        "state glyphs keep their state styling"
    );

    // An old daemon (no FEATURE_LOOM_V1): the prefix is untrusted text.
    model.daemon_features.clear();
    let (rows, colors) = draw(&model);
    assert!(
        !colors.iter().flatten().any(|cell| *cell == accent),
        "ungated task text must never earn the accent:\n{}",
        rows.join("\n")
    );

    // An unknown type id: unparseable → today's plain fallback.
    model.daemon_features = [haider_rpc::FEATURE_LOOM_V1.to_owned()]
        .into_iter()
        .collect();
    model.fleet.snapshot = Some(fleet_snapshot(vec![fleet_node("@ghost · haunt the API")]));
    let (rows, colors) = draw(&model);
    assert!(
        rows.iter().any(|row| row.contains("@ghost")),
        "the raw task text still renders:\n{}",
        rows.join("\n")
    );
    assert!(
        !colors.iter().flatten().any(|cell| *cell == accent),
        "an unknown type earns no accent"
    );
}

// ---- item 3 tail: the snapshot must not stay stale ----------------------

/// MUTATION CHECK: restore the once-per-connection loom.list gate on pane
/// entry. Expected RUNTIME failure: the LoomRefresh request (and its
/// LoomList command) below never appear for an ALREADY-hydrated snapshot.
#[test]
fn pane_entry_rerequests_the_loom_snapshot() {
    let mut model = live_bound_model();
    model.loom_requested = true; // the connection already hydrated once
    model.requests.clear();

    run_slash(&mut model, "/loom");
    assert_eq!(model.screen, Screen::Loom);
    assert!(
        model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::LoomRefresh)),
        "every pane entry re-reads the registry: {:?}",
        model.requests
    );

    let mut driver = LiveDriver::new("test");
    let commands = drain(&mut driver, &mut model);
    assert!(
        commands
            .iter()
            .any(|command| matches!(command, LiveCommand::LoomList { .. })),
        "LoomRefresh rides the loom.list wire: {commands:?}"
    );
}
