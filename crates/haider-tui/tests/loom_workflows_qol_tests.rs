#![allow(clippy::expect_used)]

//! W-flow — the Loom/Workflows QOL wave: the /workflows pane's fixed-head
//! row space (`∅ none` first and undeletable, then the built-in catalog
//! entries, then the REGISTERED section), ⌃P pin-by-name to the bound
//! session, the ⌃N typed authoring editor, ⌥m drafting-model selection, the
//! fleet's typed-agent accent, and the pane-entry snapshot re-request.
//!
//! The registry actions sit on ⌃ rather than on bare letters because the
//! owner's ruling (2026-08-22) keeps a LIVE COMPOSER on both loom panes:
//! every printable key belongs to it, exactly as on the session screen.
//! That is pinned below — a bare `p` must TYPE, never pin.
#![allow(clippy::expect_used)]

use haider_protocol::graph::{GraphPhase, GraphStatus};
use haider_protocol::ids::{GraphId, SessionId};
use haider_protocol::loom::{LoomAgentType, LoomTypeSig, compile_pipe, parse_pipe};
use haider_rpc::{
    FLEET_MAX_DEPTH, FLEET_MAX_NODES, FleetAgentStateWire, FleetMetricsTotalsWire, FleetNodeWire,
    FleetRollupWire, FleetStateCountsWire, RequestBody, ResponseBody, SessionFleetSnapshot,
    WorkflowCatalogEntryV1,
};
use haider_tui::app::{
    AppEvent, AppModel, AppRequest, DraftKey, Hit, LauncherRow, LoomPane, RuntimeMode, Screen,
    WorkflowRow,
};
use haider_tui::composer::Composer;
use haider_tui::fleet;
use haider_tui::link::{CommandContext, map_response, request_body};
use haider_tui::live::{LiveCommand, LiveDriver, LiveReply};
use haider_tui::render::render;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

mod common;
use common::{ctrl, key, launcher_model, run_slash, submit};

fn sid() -> SessionId {
    SessionId::new("s-loomflow")
}

fn author_confirmation(
    kind: haider_protocol::loom::LoomAuthorKind,
) -> haider_protocol::loom::LoomAuthorConfirmed {
    haider_protocol::loom::LoomAuthorConfirmed {
        authoring_id: "author-confirmed".into(),
        kind,
        canonical_text: "confirmed bytes".into(),
        registration: haider_protocol::loom::LoomRegistration {
            id: "reviewer".into(),
            rev: 1,
            digest: "confirmed-digest".into(),
            updated: true,
        },
        execution_digest: "confirmed-execution-digest".into(),
        install_job_id: None,
    }
}

/// A live model with the Loom + Convergence Graph features served and one
/// bound session — the ground every pin/authoring law stands on.
fn live_bound_model() -> AppModel {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.daemon_features = [
        haider_rpc::FEATURE_LOOM_V1.to_owned(),
        haider_rpc::FEATURE_LOOM_AUTHORING_V1.to_owned(),
        haider_rpc::FEATURE_LOOM_REGISTRY_CAS_V1.to_owned(),
        haider_rpc::FEATURE_LOOM_REGISTRY_ARCHIVE_V1.to_owned(),
        haider_rpc::FEATURE_LOOM_VALIDATION_V1.to_owned(),
        haider_rpc::FEATURE_TYPED_AGENT_INSTALL_V1.to_owned(),
        haider_rpc::FEATURE_TYPED_AGENT_INSTALL_CANCEL_V1.to_owned(),
        haider_rpc::FEATURE_WORKFLOW_CATALOG_V1.to_owned(),
        haider_rpc::FEATURE_LOOM_PIPE_DAG_V1.to_owned(),
        haider_rpc::FEATURE_CONVERGENCE_GRAPH_V1.to_owned(),
    ]
    .into_iter()
    .collect();
    model.daemon_version = Some("0.0.933".to_owned());
    model.sessions.clear();
    model.upsert_live_session(&sid());
    model.open_session(&sid());
    model.loom_loaded = true;
    install_catalog(&mut model, Vec::new());
    model
}

fn install_catalog(model: &mut AppModel, workflows: Vec<haider_protocol::loom::LoomWorkflow>) {
    let mut catalog = haider_protocol::graph::built_in_workflow_catalog()
        .into_iter()
        .map(|entry| WorkflowCatalogEntryV1::BuiltIn {
            id: entry.template.name.clone(),
            main_session_eligible: entry.main_session_eligible,
            template: entry.template,
        })
        .collect::<Vec<_>>();
    catalog.extend(
        workflows
            .iter()
            .cloned()
            .map(|workflow| WorkflowCatalogEntryV1::User {
                id: workflow.id.clone(),
                main_session_eligible: true,
                workflow,
            }),
    );
    model.workflow_catalog = catalog;
    model.loom_workflows = workflows;
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
        denials: Vec::new(),
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
    install_catalog(&mut model, vec![clip_workflow()]);
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
    let staggered = position("⛩ staggered");
    let sec_audit = position("⛩ sec-audit");
    let docs_sweep = position("⛩ docs-sweep");
    let registered = position("REGISTERED");
    let clip = position("@clip");
    assert!(
        none < ship
            && ship < super_ship
            && super_ship < staggered
            && staggered < sec_audit
            && sec_audit < docs_sweep
            && docs_sweep < registered
            && registered < clip,
        "fixed-head order broken:\n{}",
        rows.join("\n")
    );
    for row in [ship, super_ship, staggered, sec_audit, docs_sweep] {
        assert!(
            rows[row].contains("built-in"),
            "built-ins must be marked:\n{}",
            rows.join("\n")
        );
    }
    // The footer carries the new verbs — on ⌃, because the composer below
    // owns every bare letter.
    assert!(
        rows.iter()
            .any(|row| row.contains("⌃P pin") && row.contains("⌃N new")),
        "workflows footer missing ⌃P/⌃N:\n{}",
        rows.join("\n")
    );

    // The row-space authority agrees with the paint: none, all 5 built-ins,
    // then the registered record.
    assert_eq!(model.workflow_row_count(), 7);
    assert_eq!(model.workflow_row(0), Some(WorkflowRow::None));
    assert!(matches!(
        model.workflow_row(1),
        Some(WorkflowRow::BuiltIn(template)) if template.name == "ship-loop"
    ));
    assert!(matches!(
        model.workflow_row(2),
        Some(WorkflowRow::BuiltIn(template)) if template.name == "super-ship-loop"
    ));
    assert!(matches!(
        model.workflow_row(3),
        Some(WorkflowRow::BuiltIn(template)) if template.name == "staggered"
    ));
    assert!(matches!(
        model.workflow_row(4),
        Some(WorkflowRow::BuiltIn(template)) if template.name == "sec-audit"
    ));
    assert!(matches!(
        model.workflow_row(5),
        Some(WorkflowRow::BuiltIn(template)) if template.name == "docs-sweep"
    ));
    assert_eq!(model.workflow_row(6), Some(WorkflowRow::Registered(0)));

    // An EMPTY registry never empties the pane — the head stays, and the
    // registry emptiness moves inside the REGISTERED section (reworded).
    install_catalog(&mut model, Vec::new());
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

#[test]
fn loom_reply_installs_workflow_rows_from_the_published_catalog() {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.daemon_features = [
        haider_rpc::FEATURE_LOOM_V1.to_owned(),
        haider_rpc::FEATURE_WORKFLOW_CATALOG_V1.to_owned(),
    ]
    .into_iter()
    .collect();
    let published_builtin =
        haider_protocol::graph::graph_template(haider_protocol::graph::DOCS_SWEEP_TEMPLATE)
            .expect("published built-in");
    let child_only = haider_protocol::graph::implement_verify_child_template();
    let published_user = clip_workflow();
    let mut driver = LiveDriver::new("test");
    driver.apply(&mut model, LiveReply::Reconnected);
    driver.apply(
        &mut model,
        LiveReply::LoomRegistry {
            agent_types: vec![researcher()],
            // Deliberately empty: workflow rows must come from the new field.
            workflows: Vec::new(),
            workflow_catalog: vec![
                WorkflowCatalogEntryV1::BuiltIn {
                    id: published_builtin.name.clone(),
                    main_session_eligible: true,
                    template: published_builtin.clone(),
                },
                WorkflowCatalogEntryV1::BuiltIn {
                    id: child_only.name.clone(),
                    main_session_eligible: false,
                    template: child_only,
                },
                WorkflowCatalogEntryV1::User {
                    id: published_user.id.clone(),
                    main_session_eligible: true,
                    workflow: published_user.clone(),
                },
            ],
            cli_present: std::collections::BTreeMap::new(),
            epoch: 0,
        },
    );

    assert_eq!(model.builtin_workflow_templates(), vec![published_builtin]);
    assert_eq!(model.loom_workflows, vec![published_user]);
    assert_eq!(model.workflow_row_count(), 3);
}

/// MUTATION CHECK: restore the linked-in built-in fallback, retain legacy
/// loom.list workflows when the catalog feature is absent, or let Tab enter
/// the unsupported pane. Expected runtime failure: rows or navigation appear.
#[test]
fn absent_catalog_feature_never_becomes_a_hardcoded_or_legacy_list() {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.daemon_features = [haider_rpc::FEATURE_LOOM_V1.to_owned()]
        .into_iter()
        .collect();
    model.loom_loaded = true;
    install_catalog(&mut model, vec![clip_workflow()]);

    let mut driver = LiveDriver::new("test");
    driver.apply(&mut model, LiveReply::Reconnected);
    let unadvertised_catalog = model.workflow_catalog.clone();
    driver.apply(
        &mut model,
        LiveReply::LoomRegistry {
            agent_types: Vec::new(),
            workflows: vec![clip_workflow()],
            workflow_catalog: unadvertised_catalog,
            cli_present: std::collections::BTreeMap::new(),
            epoch: 0,
        },
    );

    assert!(model.builtin_workflow_templates().is_empty());
    assert!(model.workflow_catalog.is_empty());
    assert!(model.loom_workflows.is_empty());
    assert_eq!(model.workflow_row_count(), 0);
    assert_eq!(model.workflow_row(0), None);

    model.screen = Screen::Loom;
    model.loom_pane = LoomPane::Types;
    model.handle(key(KeyCode::Tab));
    assert_eq!(model.loom_pane, LoomPane::Types);
    assert!(
        model
            .flash
            .as_deref()
            .is_some_and(|flash| flash.contains("workflow catalog"))
    );
}

/// After a redial, Welcome settles feature absence before `loom.list` can
/// settle registry hydration. The open Workflows pane must render that known
/// absence instead of claiming the catalog is still loading.
///
/// MUTATION CHECK: put the loading branch ahead of the catalog feature gate.
/// Expected RUNTIME failure: the unavailable/loading assertions below.
#[test]
fn reconnect_to_pre_catalog_daemon_renders_typed_absence_immediately() {
    let mut model = live_bound_model();
    model.screen = Screen::Loom;
    model.loom_pane = LoomPane::Workflows;
    let mut driver = LiveDriver::new("test");

    driver.apply(
        &mut model,
        LiveReply::Disconnected {
            reason: "test redial".to_owned(),
        },
    );
    driver.apply(
        &mut model,
        LiveReply::Handshake {
            features: [haider_rpc::FEATURE_LOOM_V1.to_owned()]
                .into_iter()
                .collect(),
            version: "0.0.961".to_owned(),
        },
    );
    driver.apply(&mut model, LiveReply::Reconnected);

    let (rows, _) = draw(&model);
    let all = rows.join("\n");
    assert!(
        all.contains("workflow catalog needs workflow_catalog_v1"),
        "the fresh Welcome establishes typed absence:\n{all}"
    );
    assert!(
        !all.contains("loading registry from the daemon"),
        "known feature absence is not an in-flight catalog:\n{all}"
    );
}

/// MUTATION CHECK: drop the built-in detail derivation (nodes/gates from
/// the GraphTemplateSpec) or the `none` detail one-liner. Expected RUNTIME
/// failure: the node/gate assertions or the default-line assertion below.
#[test]
fn builtin_and_none_details_render_from_the_catalog() {
    let mut model = launcher_model();
    submit(&mut model, "inspect the built-ins");
    install_catalog(&mut model, Vec::new());
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

    model.handle(ctrl(KeyCode::Char('p')));
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
    install_catalog(&mut model, vec![clip_workflow()]);
    model.loom_selection = 6;
    model.requests.clear();
    model.handle(ctrl(KeyCode::Char('p')));
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

    model.handle(ctrl(KeyCode::Char('p')));
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
    model.handle(ctrl(KeyCode::Char('p')));
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
        haider_rpc::FEATURE_WORKFLOW_CATALOG_V1.to_owned(),
        haider_rpc::FEATURE_CONVERGENCE_GRAPH_V1.to_owned(),
    ]
    .into_iter()
    .collect();
    model.loom_loaded = true;
    model.screen = Screen::Loom;
    model.loom_pane = LoomPane::Workflows;
    model.loom_selection = 1;
    model.requests.clear();

    model.handle(ctrl(KeyCode::Char('p')));
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

/// The composer owns every bare printable key on BOTH loom panes (owner
/// 2026-08-22) — which is precisely why pin and new-workflow moved onto ⌃.
/// A bare `p` that pinned would make the composer untypable.
///
/// MUTATION CHECK: drop the ⌃ guard from either registry arm (make them
/// bare `p`/`n`). Expected RUNTIME failure: the composer-text assertion —
/// the letters would fire the actions instead of reaching the buffer.
#[test]
fn bare_letters_type_into_the_composer_and_never_fire_registry_actions() {
    for pane in [LoomPane::Workflows, LoomPane::Types] {
        let mut model = live_bound_model();
        model.screen = Screen::Loom;
        model.loom_pane = pane;
        model.loom_selection = 2;
        model.requests.clear();

        // The two hotkey letters, typed bare, plus a neighbour.
        for c in "pin now".chars() {
            model.handle(key(KeyCode::Char(c)));
        }
        assert_eq!(
            model.composer.text(),
            "pin now",
            "bare letters reach the composer verbatim on {pane:?}"
        );
        assert!(
            model.screen == Screen::Loom,
            "a bare `n` must not navigate away from {pane:?}"
        );
        assert!(
            model.requests.is_empty(),
            "a bare `p` must not reach the wire on {pane:?}: {:?}",
            model.requests
        );
    }
}

// ---- item 3: typed draft/revise/confirm authoring ------------------------

#[test]
fn ctrl_n_opens_typed_authoring_per_pane_and_enter_sends_prose_to_draft() {
    let mut model = live_bound_model();
    model.screen = Screen::Loom;
    model.loom_pane = LoomPane::Workflows;
    model.requests.clear();

    model.handle(ctrl(KeyCode::Char('n')));
    assert!(model.composer.text().is_empty());
    assert_eq!(
        model.loom_authoring.as_ref().map(|state| state.kind),
        Some(haider_protocol::loom::LoomAuthorKind::Workflow)
    );
    assert_eq!(model.screen, Screen::Loom, "authoring stays in the tab");
    assert!(model.requests.is_empty());

    for c in "spin and pin".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter));
    assert!(matches!(
        model.requests.pop(),
        Some(AppRequest::LoomAuthorDraft {
            session,
            kind: haider_protocol::loom::LoomAuthorKind::Workflow,
            prose,
            ..
        }) if session == sid() && prose == "spin and pin"
    ));

    let mut model = live_bound_model();
    model.screen = Screen::Loom;
    model.loom_pane = LoomPane::Types;
    model.handle(ctrl(KeyCode::Char('n')));
    assert_eq!(
        model.loom_authoring.as_ref().map(|state| state.kind),
        Some(haider_protocol::loom::LoomAuthorKind::AgentType)
    );
}

#[test]
fn authoring_feature_absence_never_calls_or_falls_back_to_chat() {
    let mut model = live_bound_model();
    model
        .daemon_features
        .remove(haider_rpc::FEATURE_LOOM_AUTHORING_V1);
    model.screen = Screen::Loom;
    model.loom_pane = LoomPane::Workflows;
    model.requests.clear();

    model.handle(ctrl(KeyCode::Char('n')));
    assert!(model.loom_authoring.is_none());
    assert!(model.requests.is_empty());
    assert!(model.composer.text().is_empty());
    assert!(
        model
            .flash
            .as_deref()
            .is_some_and(|text| text.contains("newer daemon"))
    );
}

#[test]
fn authoring_without_a_bound_session_never_chooses_a_model_implicitly() {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.daemon_features = [
        haider_rpc::FEATURE_LOOM_V1.to_owned(),
        haider_rpc::FEATURE_LOOM_AUTHORING_V1.to_owned(),
    ]
    .into_iter()
    .collect();
    model.loom_loaded = true;
    model.screen = Screen::Loom;
    model.loom_pane = LoomPane::Types;
    model.requests.clear();

    model.handle(ctrl(KeyCode::Char('n')));
    assert!(model.loom_authoring.is_none());
    assert!(model.requests.is_empty());
    assert!(
        model
            .flash
            .as_deref()
            .is_some_and(|flash| flash.contains("open one"))
    );
    assert_eq!(model.screen, Screen::Loom, "the tab stays put");
}

#[test]
fn alt_m_still_selects_the_session_model_used_for_ai_drafting() {
    let mut model = live_bound_model();
    model.screen = Screen::Loom;
    model.loom_pane = LoomPane::Workflows;
    model.handle(ctrl(KeyCode::Char('n')));

    model.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Char('m'),
        KeyModifiers::ALT,
    )));
    assert!(model.model_picker.is_some());
    assert!(model.loom_authoring.is_some());
    assert!(model.composer.text().is_empty());
}

#[test]
fn loom_paste_and_seed_preserve_exact_pretyped_prose() {
    let mut model = live_bound_model();
    model.screen = Screen::Loom;
    model.loom_pane = LoomPane::Workflows;
    let prose = "describe a typed review workflow ".repeat(16);
    model.handle(AppEvent::Paste(prose.clone().into()));
    assert!(model.loom_authoring.is_none());
    assert_eq!(model.composer.text(), prose);
    assert!(!model.composer.text().contains("[Pasted text #"));

    model.handle(ctrl(KeyCode::Char('n')));
    assert_eq!(model.composer.text(), prose, "Ctrl-N preserves typed prose");
    model.requests.clear();
    model.handle(key(KeyCode::Enter));
    assert!(matches!(
        model.requests.pop(),
        Some(AppRequest::LoomAuthorDraft { prose: sent, .. }) if sent == prose
    ));
}

#[test]
fn open_authoring_owns_empty_revision_and_blocks_hidden_registry_actions() {
    let mut model = live_bound_model();
    model.screen = Screen::Loom;
    model.loom_pane = LoomPane::Workflows;
    model.handle(ctrl(KeyCode::Char('n')));
    let authoring = model.loom_authoring.as_mut().expect("authoring");
    authoring.authoring_id = Some("author-empty".into());
    authoring.revision = Some(3);
    authoring.validated = true;
    model.composer.clear();
    model.requests.clear();

    model.handle(ctrl(KeyCode::Char('p')));
    assert!(
        model.requests.is_empty(),
        "hidden registry selection is locked"
    );
    assert!(
        model
            .flash
            .as_deref()
            .is_some_and(|flash| flash.contains("close the Loom editor"))
    );

    model.handle(key(KeyCode::Enter));
    assert!(matches!(
        model.requests.pop(),
        Some(AppRequest::LoomAuthorRevise {
            authoring_id,
            expected_revision: 3,
            text,
            ..
        }) if authoring_id == "author-empty" && text.is_empty()
    ));
}

#[test]
fn drafted_text_is_editable_and_ctrl_s_confirms_the_exact_revision() {
    let mut model = live_bound_model();
    model.screen = Screen::Loom;
    model.loom_pane = LoomPane::Workflows;
    model.handle(ctrl(KeyCode::Char('n')));
    let authoring = model.loom_authoring.as_mut().expect("authoring");
    authoring.authoring_id = Some("author-1".into());
    authoring.revision = Some(7);
    authoring.validated = true;
    model.composer.set_text("{\"kind\":\"workflow\"}");
    model.requests.clear();

    model.handle(ctrl(KeyCode::Char('s')));
    assert!(matches!(
        model.requests.pop(),
        Some(AppRequest::LoomAuthorConfirm {
            authoring_id,
            expected_revision,
            text,
            ..
        }) if authoring_id == "author-1"
            && expected_revision == 7
            && text == "{\"kind\":\"workflow\"}"
    ));
}

#[test]
fn draft_validation_locations_render_inline_and_transport_failure_releases_pending() {
    let mut model = live_bound_model();
    model.screen = Screen::Loom;
    model.loom_pane = LoomPane::Workflows;
    model.handle(ctrl(KeyCode::Char('n')));
    let generation = model.loom_authoring.as_ref().expect("authoring").generation;
    model.loom_authoring.as_mut().expect("authoring").pending = true;
    let mut driver = LiveDriver::new("test");
    driver.apply(
        &mut model,
        LiveReply::LoomAuthorDrafted {
            generation,
            epoch: 0,
            draft: haider_protocol::loom::LoomAuthorDraft {
                authoring_id: "author-inline".into(),
                revision: 2,
                kind: haider_protocol::loom::LoomAuthorKind::Workflow,
                text: "{\n  \"kind\": \"workflow\"\n}".into(),
                errors: vec![haider_protocol::loom::LoomAuthorValidationError {
                    code: haider_protocol::loom::LoomAuthorValidationCode::InvalidField,
                    message: "required_green must be positive".into(),
                    location: haider_protocol::loom::LoomAuthorLocation {
                        line: 12,
                        column: 9,
                        field: "nodes[1].evidence.required_green".into(),
                    },
                }],
            },
        },
    );
    assert_eq!(model.composer.text(), "{\n  \"kind\": \"workflow\"\n}");
    assert!(!model.loom_authoring.as_ref().expect("authoring").pending);
    let (rows, _) = draw(&model);
    let all = rows.join("\n");
    assert!(all.contains("12:9"), "coordinate missing:\n{all}");
    assert!(
        all.contains("nodes[1].evidence.required_green"),
        "typed field missing:\n{all}"
    );

    model.requests.clear();
    model.handle(key(KeyCode::Char(' ')));
    let revised_text = model.composer.text().to_owned();
    model.handle(key(KeyCode::Enter));
    assert!(matches!(
        model.requests.pop(),
        Some(AppRequest::LoomAuthorRevise {
            generation: request_generation,
            authoring_id,
            expected_revision: 2,
            kind: haider_protocol::loom::LoomAuthorKind::Workflow,
            text,
        }) if request_generation == generation
            && authoring_id == "author-inline"
            && text == revised_text
    ));
    driver.apply(
        &mut model,
        LiveReply::LoomAuthorFailed {
            generation,
            epoch: 0,
            message: "connection reset".into(),
        },
    );
    assert!(!model.loom_authoring.as_ref().expect("authoring").pending);
    assert!(
        model
            .flash
            .as_deref()
            .is_some_and(|flash| flash.contains("connection reset"))
    );
}

#[test]
fn authoring_wire_failure_is_routed_to_the_editor_without_a_command_id() {
    let context = CommandContext::of(&LiveCommand::LoomAuthorDraft {
        generation: 9,
        epoch: 4,
        session: sid(),
        kind: haider_protocol::loom::LoomAuthorKind::Workflow,
        prose: "draft it".into(),
    });
    assert_eq!(
        map_response(
            &context,
            ResponseBody::Error {
                code: "overloaded".into(),
                message: "try again".into(),
                retryable: true,
                data: None,
            },
        ),
        vec![LiveReply::LoomAuthorFailed {
            generation: 9,
            epoch: 4,
            message: "try again".into(),
        }]
    );
}

#[test]
fn loom_editor_has_a_dedicated_draft_surface() {
    let mut model = live_bound_model();
    let session_key = DraftKey::Session(model.ui_generation());
    let mut chat = Composer::new();
    chat.set_text("unfinished session prompt");
    model.drafts.insert(session_key, chat);
    model.screen = Screen::Loom;
    model.loom_return = Some(Screen::Session);
    model.composer.set_text("loom-only text");

    model.handle(key(KeyCode::Esc));
    assert_eq!(model.screen, Screen::Session);
    assert_eq!(model.composer.text(), "unfinished session prompt");
    assert_eq!(
        model.drafts.get(&DraftKey::Loom).map(Composer::text),
        Some("loom-only text")
    );
}

#[test]
fn pending_authoring_locks_edits_and_navigation() {
    let mut model = live_bound_model();
    model.screen = Screen::Loom;
    model.loom_pane = LoomPane::Workflows;
    model.handle(ctrl(KeyCode::Char('n')));
    model.composer.set_text("submitted bytes");
    model.loom_authoring.as_mut().expect("authoring").pending = true;

    model.handle(key(KeyCode::Char('x')));
    model.handle(AppEvent::Paste("replacement".to_owned().into()));
    model.handle(ctrl(KeyCode::Char('c')));
    model.handle_hit(Hit::GraphStrip);
    assert_eq!(model.composer.text(), "submitted bytes");
    assert_eq!(model.screen, Screen::Loom);
    assert!(model.loom_authoring.as_ref().expect("authoring").pending);
}

#[test]
fn correlated_authoring_replies_update_the_parked_editor_offscreen() {
    let mut model = live_bound_model();
    model.screen = Screen::Loom;
    model.loom_pane = LoomPane::Workflows;
    model.handle(ctrl(KeyCode::Char('n')));
    let generation = model.loom_authoring.as_ref().expect("authoring").generation;
    model.loom_authoring.as_mut().expect("authoring").pending = true;
    model.composer.set_text("submitted prose");
    model.handle(AppEvent::Envelope(Box::new(
        haider_protocol::EventPayload::UserMessage {
            text: "queued message".into(),
            attachments: Vec::new(),
            mode: haider_protocol::DeliveryMode::Steer,
        },
    )));
    assert_eq!(model.screen, Screen::Session);

    let mut driver = LiveDriver::new("test");
    driver.apply(
        &mut model,
        LiveReply::LoomAuthorDrafted {
            generation,
            epoch: 0,
            draft: haider_protocol::loom::LoomAuthorDraft {
                authoring_id: "author-confirmed".into(),
                revision: 1,
                kind: haider_protocol::loom::LoomAuthorKind::Workflow,
                text: "draft reply".into(),
                errors: Vec::new(),
            },
        },
    );
    assert!(!model.loom_authoring.as_ref().expect("authoring").pending);
    assert_eq!(
        model.drafts.get(&DraftKey::Loom).map(Composer::text),
        Some("draft reply")
    );

    model.loom_authoring.as_mut().expect("authoring").pending = true;
    let commands = driver.apply(
        &mut model,
        LiveReply::LoomAuthorConfirmed {
            generation,
            epoch: 0,
            confirmed: Some(author_confirmation(
                haider_protocol::loom::LoomAuthorKind::Workflow,
            )),
            errors: Vec::new(),
        },
    );
    assert!(matches!(
        commands.as_slice(),
        [LiveCommand::LoomList { .. }]
    ));
    assert!(
        model
            .loom_authoring
            .as_ref()
            .expect("authoring")
            .confirmed
            .is_some()
    );
    assert_eq!(
        model.drafts.get(&DraftKey::Loom).map(Composer::text),
        Some("confirmed bytes")
    );

    model.loom_authoring.as_mut().expect("authoring").pending = true;
    driver.apply(
        &mut model,
        LiveReply::LoomAuthorFailed {
            generation,
            epoch: 0,
            message: "late failure".into(),
        },
    );
    assert!(!model.loom_authoring.as_ref().expect("authoring").pending);
}

#[test]
fn graph_strip_parks_nonpending_loom_text_and_restores_the_session_draft() {
    let mut model = live_bound_model();
    model.composer.set_text("unfinished session prompt");
    model.handle(ctrl(KeyCode::Char('c')));
    model.handle_hit(Hit::ExtraRow(LauncherRow::Loom));
    model.handle(ctrl(KeyCode::Char('n')));
    model.composer.set_text("loom typed text");

    model.handle_hit(Hit::GraphStrip);
    assert_eq!(model.screen, Screen::Graph);
    assert_eq!(model.composer.text(), "unfinished session prompt");
    assert_eq!(
        model.drafts.get(&DraftKey::Loom).map(Composer::text),
        Some("loom typed text")
    );
}

#[test]
fn revise_reply_clears_prior_confirmation_and_cli_seed_cannot_overwrite_editor() {
    let mut model = live_bound_model();
    model.screen = Screen::Loom;
    model.loom_pane = LoomPane::Types;
    model.handle(ctrl(KeyCode::Char('n')));
    let generation = model.loom_authoring.as_ref().expect("authoring").generation;
    let confirmed = author_confirmation(haider_protocol::loom::LoomAuthorKind::AgentType);
    let authoring = model.loom_authoring.as_mut().expect("authoring");
    authoring.authoring_id = Some("author-confirmed".into());
    authoring.revision = Some(1);
    authoring.confirmed = Some(confirmed);
    authoring.pending = true;
    model.composer.set_text("submitted edit");

    let mut driver = LiveDriver::new("test");
    driver.apply(
        &mut model,
        LiveReply::LoomAuthorDrafted {
            generation,
            epoch: 0,
            draft: haider_protocol::loom::LoomAuthorDraft {
                authoring_id: "author-confirmed".into(),
                revision: 2,
                kind: haider_protocol::loom::LoomAuthorKind::AgentType,
                text: "revised bytes".into(),
                errors: Vec::new(),
            },
        },
    );
    assert!(
        model
            .loom_authoring
            .as_ref()
            .expect("authoring")
            .confirmed
            .is_none()
    );
    model.handle(ctrl(KeyCode::Char('i')));
    assert_eq!(model.composer.text(), "revised bytes");
    assert!(
        model
            .flash
            .as_deref()
            .is_some_and(|flash| flash.contains("close the Loom editor"))
    );
}

#[test]
fn closing_a_confirmed_editor_reports_that_registration_is_retained() {
    let mut model = live_bound_model();
    model.screen = Screen::Loom;
    model.loom_pane = LoomPane::Workflows;
    model.handle(ctrl(KeyCode::Char('n')));
    model.loom_authoring.as_mut().expect("authoring").confirmed = Some(author_confirmation(
        haider_protocol::loom::LoomAuthorKind::Workflow,
    ));

    model.handle(key(KeyCode::Esc));
    assert!(model.loom_authoring.is_none());
    assert!(
        model
            .flash
            .as_deref()
            .is_some_and(|flash| flash.contains("remains registered"))
    );

    let mut edited = live_bound_model();
    edited.screen = Screen::Loom;
    edited.loom_pane = LoomPane::Workflows;
    edited.handle(ctrl(KeyCode::Char('n')));
    edited.loom_authoring.as_mut().expect("authoring").confirmed = Some(author_confirmation(
        haider_protocol::loom::LoomAuthorKind::Workflow,
    ));
    edited.handle(key(KeyCode::Char('x')));
    assert!(
        edited
            .loom_authoring
            .as_ref()
            .expect("edited authoring")
            .confirmed
            .is_none()
    );
    edited.handle(key(KeyCode::Esc));
    assert!(
        edited
            .flash
            .as_deref()
            .is_some_and(|flash| flash.contains("no new revision registered"))
    );
}

#[test]
fn late_reply_cannot_bind_to_a_reopened_editor() {
    let mut model = live_bound_model();
    model.screen = Screen::Loom;
    model.loom_pane = LoomPane::Workflows;
    model.handle(ctrl(KeyCode::Char('n')));
    let old_generation = model
        .loom_authoring
        .as_ref()
        .expect("old authoring")
        .generation;
    model
        .loom_authoring
        .as_mut()
        .expect("old authoring")
        .pending = false;
    model.handle(key(KeyCode::Esc));
    model.handle(ctrl(KeyCode::Char('n')));
    let new_generation = model
        .loom_authoring
        .as_ref()
        .expect("new authoring")
        .generation;
    assert_ne!(old_generation, new_generation);
    model.composer.set_text("new prose");
    model
        .loom_authoring
        .as_mut()
        .expect("new authoring")
        .pending = true;

    let mut driver = LiveDriver::new("test");
    driver.apply(
        &mut model,
        LiveReply::LoomAuthorDrafted {
            generation: old_generation,
            epoch: 0,
            draft: haider_protocol::loom::LoomAuthorDraft {
                authoring_id: "stale".into(),
                revision: 1,
                kind: haider_protocol::loom::LoomAuthorKind::Workflow,
                text: "stale bytes".into(),
                errors: Vec::new(),
            },
        },
    );
    assert_eq!(model.composer.text(), "new prose");
    let authoring = model.loom_authoring.as_ref().expect("new authoring");
    assert!(authoring.pending);
    assert!(authoring.authoring_id.is_none());
}

#[test]
fn disconnected_epoch_rejects_a_queued_old_authoring_reply() {
    let mut model = live_bound_model();
    model.screen = Screen::Loom;
    model.loom_pane = LoomPane::Workflows;
    model.handle(ctrl(KeyCode::Char('n')));
    let generation = model.loom_authoring.as_ref().expect("authoring").generation;
    model.loom_authoring.as_mut().expect("authoring").pending = true;
    let mut driver = LiveDriver::new("test");
    driver.apply(
        &mut model,
        LiveReply::Disconnected {
            reason: "socket reset".into(),
        },
    );
    model.composer.set_text("retry prose");
    model.loom_authoring.as_mut().expect("authoring").pending = true;
    driver.apply(
        &mut model,
        LiveReply::LoomAuthorDrafted {
            generation,
            epoch: 0,
            draft: haider_protocol::loom::LoomAuthorDraft {
                authoring_id: "old-connection".into(),
                revision: 1,
                kind: haider_protocol::loom::LoomAuthorKind::Workflow,
                text: "old connection bytes".into(),
                errors: Vec::new(),
            },
        },
    );
    assert_eq!(model.composer.text(), "retry prose");
    assert!(model.loom_authoring.as_ref().expect("authoring").pending);
}

#[test]
fn feature_loss_hides_confirm_affordance_for_an_open_editor() {
    let mut model = live_bound_model();
    model.screen = Screen::Loom;
    model.handle(ctrl(KeyCode::Char('n')));
    model
        .daemon_features
        .remove(haider_rpc::FEATURE_LOOM_AUTHORING_V1);
    let (rows, _) = draw(&model);
    let all = rows.join("\n");
    assert!(all.contains("authoring unavailable"), "{all}");
    assert!(!all.contains("confirm/register"), "{all}");
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

/// MUTATION CHECK: remove the editor's L4 hotkeys or drop their CAS/job
/// coordinates. Expected runtime failure: one of the three exact requests
/// below disappears or fabricates a default coordinate.
#[test]
fn editor_surfaces_validate_archive_and_install_cancel() {
    let mut model = live_bound_model();
    model.screen = Screen::Loom;
    model.loom_pane = LoomPane::Types;
    model.loom_types = vec![researcher()];
    model.loom_selection = 1;
    model.requests.clear();
    model.handle(ctrl(KeyCode::Char('a')));
    assert!(matches!(
        model.requests.pop(),
        Some(AppRequest::LoomArchive {
            kind: haider_protocol::loom::LoomRegistryEntryKind::AgentType,
            id,
            expected_rev: 1,
            expected_digest,
        }) if id == "researcher" && expected_digest == researcher().digest()
    ));

    model.handle(ctrl(KeyCode::Char('n')));
    let text = serde_json::json!({
        "kind": "agent_type",
        "id": "preview",
        "name": "Preview",
        "job": "Preview only",
        "in_type": "Patch",
        "out_type": "Verdict",
        "capability_keys": [],
        "grants": [],
        "denials": [],
        "skills": [],
        "scripts": [],
        "color": "",
        "glyph": ""
    })
    .to_string();
    model.composer.set_text(&text);
    model.requests.clear();
    model.handle(ctrl(KeyCode::Char('v')));
    assert!(matches!(
        model.requests.pop(),
        Some(AppRequest::LoomValidate { text: sent, .. }) if sent == text
    ));

    let authoring = model.loom_authoring.as_mut().expect("editor");
    authoring.pending = false;
    let mut confirmed = author_confirmation(haider_protocol::loom::LoomAuthorKind::AgentType);
    confirmed.install_job_id = Some("install:preview:1".into());
    authoring.confirmed = Some(confirmed);
    authoring.install_job = Some(haider_protocol::typed_agent::TypedAgentInstallJob {
        job_id: "install:preview:1".into(),
        agent_type_id: "preview".into(),
        agent_type_rev: 1,
        agent_type_digest: "0123456789abcdef0123456789abcdef".into(),
        state: haider_protocol::typed_agent::TypedAgentInstallState::Queued,
        cancelled: false,
        progress: haider_protocol::typed_agent::TypedAgentInstallProgress {
            total: 1,
            completed: 0,
            current_cli: None,
        },
        error: None,
        created_at_ms: 1,
        updated_at_ms: 1,
    });
    let generation = authoring.generation;
    model.requests.clear();
    model.handle(ctrl(KeyCode::Char('a')));
    assert!(matches!(
        model.requests.pop(),
        Some(AppRequest::LoomArchive {
            kind: haider_protocol::loom::LoomRegistryEntryKind::AgentType,
            id,
            expected_rev: 1,
            expected_digest,
        }) if id == "reviewer" && expected_digest == "confirmed-digest"
    ));

    model.requests.clear();
    model.handle(ctrl(KeyCode::Char('x')));
    let Some(AppRequest::LoomInstallCancel {
        generation: requested_generation,
        job_id,
    }) = model.requests.pop()
    else {
        panic!("cancel request missing");
    };
    assert_eq!(requested_generation, generation);
    assert_eq!(job_id, "install:preview:1");

    let mut driver = LiveDriver::new("test");
    let command = driver
        .handle_request(
            &mut model,
            AppRequest::LoomInstallCancel {
                generation,
                job_id: job_id.clone(),
            },
        )
        .pop()
        .expect("cancel command");
    assert_eq!(
        request_body(command),
        RequestBody::LoomInstallCancel {
            install_job_id: job_id
        }
    );
}
