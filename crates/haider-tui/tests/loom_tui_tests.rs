//! D1-D2 — the Loom surfaces: a typed child's `@type ·` label paints in its
//! registry accent, and the graph screen annotates a pinned Loom workflow's
//! nodes with their specialists and tasks.
#![allow(clippy::expect_used)]

use haider_protocol::agent::{AgentManifest, AgentRole, Grant, Placement};
use haider_protocol::graph::{
    GraphEvidenceTally, GraphGateKind, GraphNodeName, GraphNodeStatus, GraphPhase, GraphStatus,
};
use haider_protocol::ids::{AgentId, GraphId, LeaseId};
use haider_protocol::loom::{LoomAgentType, LoomTypeSig, compile_pipe, parse_pipe};
use haider_tui::app::{AppModel, Screen};
use haider_tui::render::render;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

mod common;
use common::{launcher_model, submit};

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

/// MUTATION CHECK: drop the `@type ·` split, the registry lookup, or the
/// accent parse. Expected RUNTIME failure: the typed chip loses its colored
/// `@researcher` segment (text or the exact registry RGB).
#[test]
fn typed_chip_paints_its_loom_accent() {
    let mut model = launcher_model();
    submit(&mut model, "spawn a subagent for the transcript");
    model.loom_types = vec![researcher()];
    // The C2 spawn convention: the child's task label leads with `@type ·`.
    let manifest = AgentManifest {
        agent: AgentId::new("loom-child"),
        role: AgentRole::Subagent,
        task: "@researcher · pull the transcript".into(),
        callsign: Some("Ammar".into()),
        model_profile: "fable-5".into(),
        grant: Grant {
            tools: vec![],
            effect_ceiling: vec![],
        },
        budget_tokens: None,
        placement: Placement::Local,
        lease: LeaseId::new("lease-loom"),
        fencing_epoch: 1,
        attempt: 0,
        parent: None,
        coordinates: None,
        cli_scope: None,
    };
    model
        .chips
        .push(haider_tui::app::ChipModel::from_manifest(&manifest));

    let (rows, colors) = draw(&model);
    let row = rows
        .iter()
        .position(|row| row.contains("@researcher"))
        .unwrap_or_else(|| panic!("typed chip row missing:\n{}", rows.join("\n")));
    let column = rows[row].find("@researcher").expect("column");
    // The registry color #c2701c must reach the terminal cell.
    assert_eq!(
        colors[row][column],
        Some(ratatui::style::Color::Rgb(0xc2, 0x70, 0x1c)),
        "typed segment must carry the Loom accent"
    );
    assert!(rows[row].contains("▲"), "glyph missing: {}", rows[row]);
}

/// MUTATION CHECK: drop the graph-screen Loom join (template → workflow →
/// per-node meta). Expected RUNTIME failure: the node row loses its
/// `@researcher "task"` annotation or the header loses the typed signature.
#[test]
fn graph_screen_annotates_loom_nodes() {
    let mut model = launcher_model();
    submit(&mut model, "run the clip workflow");
    model.loom_types = vec![researcher()];
    let workflow = compile_pipe(
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
    .expect("compiles");
    let node = GraphNodeName::new("RESEARCH").expect("node name");
    model.graph = Some(GraphStatus {
        graph_id: GraphId::new("graph-clip"),
        template: "clip".into(),
        // The pin persists graph_template_digest(template) — the TUI join
        // key (review round 2), never the workflow content digest.
        digest: haider_protocol::graph::graph_template_digest(&workflow.template),
        template_version: 1,
        start_node: Some(node.clone()),
        phase: GraphPhase::Active,
        current_node: Some(node.clone()),
        ready_nodes: vec![node.clone()],
        attempt: 1,
        nodes: vec![GraphNodeStatus {
            node,
            gate: Some(GraphGateKind::CommandGreen),
            executor: None,
            attempts_opened: 1,
            current_attempt: Some(1),
            evidence: GraphEvidenceTally::default(),
            evidence_slots: Vec::new(),
            satisfied: false,
        }],
        blocked_reason: None,
        pending_menu: None,
        pending_menus: Vec::new(),
        run_set: None,
    });
    model.loom_workflows = vec![workflow];
    model.screen = Screen::Graph;

    let (rows, _) = draw(&model);
    let all = rows.join("\n");
    assert!(
        all.contains("SourceURL -> Transcript"),
        "typed signature missing from header:\n{all}"
    );
    assert!(
        all.contains("@researcher"),
        "node specialist missing:\n{all}"
    );
    assert!(
        all.contains("\"pull and transcribe\""),
        "node task missing:\n{all}"
    );
}

/// MUTATION CHECK: drop the Loom screen dispatch arm or the registry list
/// rendering in `render_loom`. Expected RUNTIME failure: the `/loom` screen
/// loses the accent-painted type row or the workflow row.
#[test]
fn loom_screen_lists_registry() {
    let mut model = launcher_model();
    submit(&mut model, "open the loom");
    model.loom_types = vec![researcher()];
    model.loom_workflows = vec![
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
        .expect("compiles"),
    ];
    model.screen = Screen::Loom;

    let (rows, colors) = draw(&model);
    let row = rows
        .iter()
        .position(|row| row.contains("@researcher"))
        .unwrap_or_else(|| panic!("type row missing:\n{}", rows.join("\n")));
    let column = rows[row].find('▲').expect("glyph column");
    assert_eq!(
        colors[row][column],
        Some(ratatui::style::Color::Rgb(0xc2, 0x70, 0x1c)),
        "type row must paint the registry accent"
    );
    assert!(
        rows.iter().any(|row| row.contains("@clip")),
        "workflow row missing:\n{}",
        rows.join("\n")
    );

    // ⏎ detail: the workflow pane shows the typed signature + pipe source.
    model.loom_selection = 1;
    model.loom_detail = true;
    let (rows, _) = draw(&model);
    let all = rows.join("\n");
    assert!(
        all.contains("PIPE SOURCE"),
        "workflow detail missing:\n{all}"
    );
    assert!(
        all.contains("SourceURL -> Transcript"),
        "typed signature missing:\n{all}"
    );
}

/// Review round 2 MUTATION CHECK: join the graph screen's Loom annotations
/// by NAME again. Expected RUNTIME failure: a drifted registry (same id, new
/// digest) still paints its tasks/specialists over the pinned graph.
#[test]
fn graph_annotations_require_the_pinned_digest() {
    let mut model = launcher_model();
    submit(&mut model, "run the clip workflow");
    model.loom_types = vec![researcher()];
    let workflow = compile_pipe(
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
    .expect("compiles");
    let node = GraphNodeName::new("RESEARCH").expect("node name");
    model.graph = Some(GraphStatus {
        graph_id: GraphId::new("graph-clip"),
        template: "clip".into(),
        digest: "drifted-instance-digest".into(),
        template_version: 1,
        start_node: Some(node.clone()),
        phase: GraphPhase::Active,
        current_node: Some(node.clone()),
        ready_nodes: vec![node.clone()],
        attempt: 1,
        nodes: vec![GraphNodeStatus {
            node,
            gate: Some(GraphGateKind::CommandGreen),
            executor: None,
            attempts_opened: 1,
            current_attempt: Some(1),
            evidence: GraphEvidenceTally::default(),
            evidence_slots: Vec::new(),
            satisfied: false,
        }],
        blocked_reason: None,
        pending_menu: None,
        pending_menus: Vec::new(),
        run_set: None,
    });
    model.loom_workflows = vec![workflow];
    model.screen = Screen::Graph;

    let (rows, _) = draw(&model);
    let all = rows.join("\n");
    assert!(
        !all.contains("@researcher") && !all.contains("pull and transcribe"),
        "drifted registry must render NO Loom annotations:\n{all}"
    );
}

/// Review round 2 MUTATION CHECK: drop the Loom-feature gate from the typed
/// chip paint. Expected RUNTIME failure: task text from a daemon that never
/// strips `@type ·` cosplay (no FEATURE_LOOM_V1) earns the trusted accent.
#[test]
fn typed_chip_accent_requires_a_loom_aware_daemon() {
    let mut model = launcher_model();
    submit(&mut model, "spawn one");
    model.mode = haider_tui::app::RuntimeMode::Live;
    model.loom_types = vec![researcher()];
    let manifest = AgentManifest {
        agent: AgentId::new("spoof-child"),
        role: AgentRole::Subagent,
        task: "@researcher · attacker controlled".into(),
        callsign: Some("Ammar".into()),
        model_profile: "fable-5".into(),
        grant: Grant {
            tools: vec![],
            effect_ceiling: vec![],
        },
        budget_tokens: None,
        placement: Placement::Local,
        lease: LeaseId::new("lease-spoof"),
        fencing_epoch: 1,
        attempt: 0,
        parent: None,
        coordinates: None,
        cli_scope: None,
    };
    model
        .chips
        .push(haider_tui::app::ChipModel::from_manifest(&manifest));

    // An old daemon (no Loom feature): the label renders as PLAIN text —
    // no registry accent anywhere on its row.
    let (rows, colors) = draw(&model);
    let accent = Some(ratatui::style::Color::Rgb(0xc2, 0x70, 0x1c));
    assert!(
        !colors.iter().flatten().any(|cell| *cell == accent),
        "old-daemon task text must never earn the Loom accent:\n{}",
        rows.join("\n")
    );

    // The same connection advertising the feature restores the paint.
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_LOOM_V1.to_owned());
    let (_, colors) = draw(&model);
    assert!(
        colors.iter().flatten().any(|cell| *cell == accent),
        "a Loom-aware daemon's stamped prefix paints the accent"
    );
}

/// Review round 2 MUTATION CHECK: let the Loom snapshot survive the socket.
/// Expected RUNTIME failure: after a disconnect the stale registry (types,
/// workflows, the hydration latch) still stands, so a reconnect to a
/// different (or Loom-less) daemon keeps painting connection A's accents.
#[test]
fn disconnect_clears_the_loom_snapshot() {
    let mut model = launcher_model();
    model.mode = haider_tui::app::RuntimeMode::Live;
    model.loom_types = vec![researcher()];
    model.loom_workflows = Vec::new();
    model.loom_loaded = true;

    let mut driver = haider_tui::live::LiveDriver::new("test");
    driver.apply(
        &mut model,
        haider_tui::live::LiveReply::Disconnected {
            reason: "peer closed".to_owned(),
        },
    );
    assert!(
        !model.loom_loaded,
        "the hydration latch dies with the socket"
    );
    assert!(model.loom_types.is_empty(), "stale types must not survive");
    assert!(model.loom_workflows.is_empty());
}

/// Round 3 MUTATION CHECK: render "registry empty" for an unhydrated live
/// connection, or forget the origin screen / detail fold on esc. Expected
/// RUNTIME failures below.
#[test]
fn loom_screen_loading_state_and_esc_origin() {
    let mut model = launcher_model();
    model.mode = haider_tui::app::RuntimeMode::Live;
    model.loom_loaded = false;
    model.screen = Screen::Loom;
    let (rows, _) = draw(&model);
    let all = rows.join("\n");
    assert!(
        all.contains("loading registry"),
        "unhydrated live /loom must say LOADING, never empty:\n{all}"
    );
    assert!(
        !all.contains("registry empty"),
        "loading must not read as empty:\n{all}"
    );

    // Esc returns to the screen /loom was opened FROM — and an emptied
    // registry folds an open detail pane so one press suffices.
    model.loom_loaded = true;
    model.loom_detail = true;
    model.loom_return = Some(Screen::Graph);
    model.handle(haider_tui::app::AppEvent::Key(
        ratatui::crossterm::event::KeyEvent::new(
            ratatui::crossterm::event::KeyCode::Esc,
            ratatui::crossterm::event::KeyModifiers::NONE,
        ),
    ));
    assert_eq!(model.screen, Screen::Graph, "esc honors the origin screen");
}
