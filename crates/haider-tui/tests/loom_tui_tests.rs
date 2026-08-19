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
        digest: workflow.digest.clone(),
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
