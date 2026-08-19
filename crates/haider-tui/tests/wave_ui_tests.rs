//! W-UI — the Loom split surfaces reach the menus (palette entries, launcher
//! rows, the `/workflows` pane) and the subagents tree speaks a child's
//! pinned-workflow DAG (`agent_graph_rollup_v1` → the chip row, sim
//! tui.js:5410-5428 / the Image #26 law).
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::agent::{
    AGENT_GRAPH_ROLLUP_EXTENSION_KIND, AgentGraphRollupV1, AgentManifest, AgentRole, Grant,
    Placement,
};
use haider_protocol::ids::{AgentId, ItemId, LeaseId};
use haider_protocol::item::{ItemEvent, TurnItem};
use haider_protocol::loom::{LoomTypeSig, compile_pipe, parse_pipe};
use haider_tui::app::{AppModel, ChipModel, Hit, LauncherRow, LoomPane, RuntimeMode, Screen};
use haider_tui::commands::COMMANDS;
use haider_tui::render::render;
use haider_tui::runtime::osc_session_announce;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

mod common;
use common::{launcher_model, run_slash, submit};

fn live_loom_model() -> AppModel {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_LOOM_V1.to_owned());
    model.loom_loaded = true;
    model
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

fn researcher_type() -> haider_protocol::loom::LoomAgentType {
    haider_protocol::loom::LoomAgentType {
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

fn draw_rows(
    model: &AppModel,
    width: u16,
    height: u16,
) -> (Vec<String>, Vec<(ratatui::layout::Rect, Hit)>) {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    let mut hits = Vec::new();
    terminal
        .draw(|frame| {
            hits = render(model, frame);
        })
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    let mut rows = Vec::new();
    for y in 0..buffer.area.height {
        let mut text = String::new();
        for x in 0..buffer.area.width {
            text.push_str(buffer[(x, y)].symbol());
        }
        rows.push(text);
    }
    (rows, hits)
}

fn rollup(agent: &str, state: &str) -> AgentGraphRollupV1 {
    AgentGraphRollupV1 {
        agent: AgentId::new(agent),
        workflow_id: Some("ship".into()),
        template_digest: "d1".into(),
        state: state.into(),
        node_index: 2,
        nodes_total: 3,
        nodes_green: 3,
        node_label: Some("verify".into()),
        agent_type: None,
        gate: None,
    }
}

fn rollup_envelope(roll: &AgentGraphRollupV1) -> EventPayload {
    EventPayload::Item(ItemEvent::Completed {
        item_id: ItemId::new("roll-1"),
        item: TurnItem::Extension {
            kind: AGENT_GRAPH_ROLLUP_EXTENSION_KIND.to_owned(),
            data: serde_json::to_value(roll).expect("serializes"),
        },
    })
}

fn workflow_chip(agent: &str) -> ChipModel {
    ChipModel::from_manifest(&AgentManifest {
        agent: AgentId::new(agent),
        role: AgentRole::Subagent,
        task: "ship the wave".into(),
        callsign: Some("Ship Loop".into()),
        model_profile: "fable-5".into(),
        grant: Grant {
            tools: vec![],
            effect_ceiling: vec![],
        },
        budget_tokens: None,
        parent: None,
        placement: Placement::Local,
        coordinates: None,
        cli_scope: None,
        lease: LeaseId::new("lease-wave-ui"),
        fencing_epoch: 1,
        attempt: 0,
    })
}

/// MUTATION CHECK: drop any of the three palette registrations (or flip
/// `/graph` off session-only). Expected RUNTIME failure: the registry rows
/// disappear or their gating changes.
#[test]
fn palette_registers_the_loom_split_surfaces() {
    let loom = COMMANDS
        .iter()
        .find(|spec| spec.name == "loom")
        .expect("/loom is a palette citizen");
    assert!(!loom.session_only, "the registry browser is global");
    let workflows = COMMANDS
        .iter()
        .find(|spec| spec.name == "workflows")
        .expect("/workflows is a palette citizen");
    assert!(!workflows.session_only, "the template browser is global");
    let graph = COMMANDS
        .iter()
        .find(|spec| spec.name == "graph")
        .expect("/graph is a palette citizen");
    assert!(graph.session_only, "the pinned run is session-scoped");
}

/// MUTATION CHECK: remove a launcher row or its value-carrying hit.
/// Expected RUNTIME failure: the row text or its ExtraRow hit vanishes.
#[test]
fn launcher_lists_workflows_and_loom_rows_with_hits() {
    let model = launcher_model();
    let (rows, hits) = draw_rows(&model, 140, 40);
    assert!(rows.iter().any(|row| row.contains("⌘ Workflows")));
    assert!(rows.iter().any(|row| row.contains("✦ Loom")));
    assert!(
        hits.iter()
            .any(|(_, hit)| *hit == Hit::ExtraRow(LauncherRow::Workflows))
    );
    assert!(
        hits.iter()
            .any(|(_, hit)| *hit == Hit::ExtraRow(LauncherRow::Loom))
    );
}

/// MUTATION CHECK: point a launcher row at the wrong pane (or drop the
/// pane reset). Expected RUNTIME failure: the opened pane flips.
#[test]
fn launcher_rows_open_their_registry_panes() {
    let mut model = live_loom_model();
    model.handle_hit(Hit::ExtraRow(LauncherRow::Workflows));
    assert_eq!(model.screen, Screen::Loom);
    assert_eq!(model.loom_pane, LoomPane::Workflows);

    let mut model = live_loom_model();
    model.handle_hit(Hit::ExtraRow(LauncherRow::Loom));
    assert_eq!(model.screen, Screen::Loom);
    assert_eq!(model.loom_pane, LoomPane::Types);
}

/// MUTATION CHECK: leak the sibling pane's list into the view, or break
/// the tab hop / its selection reset. Expected RUNTIME failure: the
/// workflows view shows AGENT TYPES (or tab keeps the old pane).
#[test]
fn workflows_pane_lists_only_workflows_and_tab_hops_to_types() {
    let mut model = live_loom_model();
    model.loom_types = vec![researcher_type()];
    model.loom_workflows = vec![clip_workflow()];
    run_slash(&mut model, "/workflows");
    assert_eq!(model.screen, Screen::Loom);
    assert_eq!(model.loom_pane, LoomPane::Workflows);

    let (rows, _) = draw_rows(&model, 120, 34);
    assert!(
        rows.iter()
            .any(|row| row.contains("workflows — 1 workflow"))
    );
    assert!(rows.iter().any(|row| row.contains("@clip")));
    assert!(
        !rows.iter().any(|row| row.contains("AGENT TYPES")),
        "the workflows pane never renders the types section"
    );

    model.loom_selection = 0;
    model.handle(common::key(ratatui::crossterm::event::KeyCode::Tab));
    assert_eq!(model.loom_pane, LoomPane::Types);
    let (rows, _) = draw_rows(&model, 120, 34);
    assert!(rows.iter().any(|row| row.contains("loom — 1 agent type")));
    assert!(rows.iter().any(|row| row.contains("@researcher")));
    assert!(!rows.iter().any(|row| row.contains("@clip")));
}

/// MUTATION CHECK: stop swallowing the rollup marker (it leaks into the
/// transcript) or stop applying it to the chip. Expected RUNTIME failure:
/// chip.graph stays None or the raw extension renders as a transcript row.
#[test]
fn rollup_routes_to_the_chip_and_stays_out_of_the_transcript() {
    // Demo fabricates the session surface; the envelope intercept is
    // mode-independent, so the render law is provable without a daemon.
    let mut model = launcher_model();
    submit(&mut model, "ship the wave");
    model.chips.push(workflow_chip("wf-child"));
    let before = draw_rows(&model, 130, 40).0.join("\n");

    model.handle(haider_tui::app::AppEvent::Envelope(Box::new(
        rollup_envelope(&rollup("wf-child", "complete")),
    )));

    let chip = &model.chips[0];
    assert_eq!(
        chip.graph.as_ref().map(|roll| roll.state.as_str()),
        Some("complete")
    );
    let after = draw_rows(&model, 130, 40).0.join("\n");
    assert!(
        !after.contains(AGENT_GRAPH_ROLLUP_EXTENSION_KIND),
        "the raw marker never renders"
    );
    assert!(after.contains("⛩ ship"), "the row names the workflow");
    assert!(after.contains("✓ 3/3 nodes green"), "the act slot tallies");
    // The only new prose is the chip row itself — the transcript did not
    // grow a generic ⋯ extension row.
    assert_eq!(
        before.matches('⋯').count(),
        after.matches('⋯').count(),
        "no generic extension row appeared"
    );
}

/// MUTATION CHECK: collapse the state vocabulary (gate/running lose their
/// sentences). Expected RUNTIME failure: a wrong act-slot sentence.
#[test]
fn rollup_states_speak_the_dag_position() {
    let mut model = launcher_model();
    submit(&mut model, "ship the wave");
    model.chips.push(workflow_chip("wf-child"));

    let mut gate = rollup("wf-child", "gate");
    gate.gate = Some("human".into());
    gate.nodes_green = 1;
    model.handle(haider_tui::app::AppEvent::Envelope(Box::new(
        rollup_envelope(&gate),
    )));
    let (rows, _) = draw_rows(&model, 130, 40);
    assert!(
        rows.iter()
            .any(|row| row.contains("⛩ gate — needs your confirm"))
    );

    let mut running = rollup("wf-child", "running");
    running.nodes_green = 1;
    model.handle(haider_tui::app::AppEvent::Envelope(Box::new(
        rollup_envelope(&running),
    )));
    let (rows, _) = draw_rows(&model, 130, 40);
    assert!(rows.iter().any(|row| row.contains("node 2/3")));
    assert!(
        rows.iter().any(|row| row.contains("· verify")),
        "running shows the current node label"
    );
}

/// MUTATION CHECK: change the OSC number/shape, drop the control strip, or
/// announce something for the launcher. Expected RUNTIME failure: exact
/// byte pins below.
#[test]
fn osc_session_announce_bytes_are_exact_and_sanitized() {
    assert_eq!(
        osc_session_announce(Some("sess-42")),
        "\u{1b}]7791;haider;attached=sess-42\u{1b}\\"
    );
    assert_eq!(
        osc_session_announce(None),
        "\u{1b}]7791;haider;attached=\u{1b}\\"
    );
    assert_eq!(
        osc_session_announce(Some("a\u{1b};]b\u{7}c")),
        "\u{1b}]7791;haider;attached=a]bc\u{1b}\\",
        "control characters and separators never smuggle a second escape"
    );
}
