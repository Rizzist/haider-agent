#![allow(clippy::expect_used)]
//! v0.0.963 L3 live workflow DAG rendering.

use haider_client::{
    WorkflowEvidenceRef, WorkflowGraphEdge, WorkflowGraphEdgeKind, WorkflowGraphProjection,
    WorkflowGraphState, WorkflowNodeProjection, WorkflowNodeRejection, WorkflowNodeState,
};
use haider_protocol::envelope::{EventEnvelope, PromptRender, RawEnvelope, RenderTargets};
use haider_protocol::graph::{
    GraphExecutorShape, GraphGateKind, GraphNodeName, GraphNodeSpec, GraphTemplateSpec,
    WorkflowActivationAst, WorkflowActivationCause, WorkflowActivationEdge, WorkflowActivationNode,
    WorkflowEdgeKind, WorkflowGraphJournalEvent, WorkflowGraphStarted, WorkflowGraphWatchEvent,
    WorkflowJoinSemantics, WorkflowNodeActivated, WorkflowNodeInput, WorkflowNodeRejectCode,
    workflow_activation_ast_digest, workflow_input_ledger_digest,
};
use haider_protocol::ids::{ArtifactRef, DeviceId, EventId, GraphId, SessionId};
use haider_protocol::pipe::InstructEvidenceRef;
use haider_rpc::{RequestBody, ResponseBody, WorkflowCatalogEntryV1};
use haider_tui::app::{
    AppEvent, AppModel, AppRequest, LoomPane, RuntimeMode, Screen, WorkflowEvidenceInspection,
};
use haider_tui::link::{CommandContext, map_response, request_body};
use haider_tui::live::{LiveCommand, LiveDriver, LiveReply, WorkflowGraphRead};
use haider_tui::render::{render, workflow_live_dag_lines};
use haider_tui::theme::ThemeKey;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::Color;

fn spec_node(name: &str, dependencies: &[&str]) -> GraphNodeSpec {
    GraphNodeSpec {
        name: GraphNodeName::new(name).expect("valid node name"),
        gate: GraphGateKind::CommandGreen,
        executor: GraphExecutorShape::Inline,
        max_attempts: 1,
        max_evidence_per_attempt: None,
        depends_on: dependencies
            .iter()
            .map(|dependency| GraphNodeName::new(*dependency).expect("valid dependency"))
            .collect(),
        red_target: None,
        verify_slots: Vec::new(),
    }
}

fn template() -> GraphTemplateSpec {
    GraphTemplateSpec {
        name: "release".to_owned(),
        version: 1,
        start_node: None,
        nodes: vec![
            spec_node("PLAN", &[]),
            spec_node("BUILD", &["PLAN"]),
            spec_node("DOCS", &["PLAN"]),
            spec_node("VERIFY", &["BUILD", "DOCS"]),
        ],
    }
}

fn runtime_node(
    node_id: &str,
    status: WorkflowNodeState,
    inputs_present: &[bool],
) -> WorkflowNodeProjection {
    WorkflowNodeProjection {
        node_id: node_id.to_owned(),
        status,
        inputs_present: inputs_present.to_vec(),
        evidence_refs: Vec::new(),
        rejection: None,
    }
}

fn edge(kind: WorkflowGraphEdgeKind, from: Option<&str>, to: &str) -> WorkflowGraphEdge {
    WorkflowGraphEdge {
        kind,
        from: from.map(str::to_owned),
        to: to.to_owned(),
    }
}

fn runtime_state(
    cursor: u64,
    nodes: Vec<WorkflowNodeProjection>,
    edges: Vec<WorkflowGraphEdge>,
) -> WorkflowGraphState {
    WorkflowGraphState {
        graph_id: "graph-release".to_owned(),
        workflow_id: "release".to_owned(),
        workflow_digest: "blake3:compiled-release".to_owned(),
        cursor,
        nodes,
        edges,
    }
}

fn diamond_edges() -> Vec<WorkflowGraphEdge> {
    vec![
        edge(WorkflowGraphEdgeKind::GraphInput, None, "PLAN"),
        edge(WorkflowGraphEdgeKind::Forward, Some("PLAN"), "BUILD"),
        edge(WorkflowGraphEdgeKind::Forward, Some("PLAN"), "DOCS"),
        edge(WorkflowGraphEdgeKind::Forward, Some("BUILD"), "VERIFY"),
        edge(WorkflowGraphEdgeKind::Forward, Some("DOCS"), "VERIFY"),
    ]
}

fn rendered(projection: &WorkflowGraphProjection) -> (String, Vec<ratatui::text::Line<'static>>) {
    let theme = ThemeKey::Dark.theme();
    let lines = workflow_live_dag_lines(projection, theme);
    let text = lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    (text, lines)
}

fn rendered_screen(model: &AppModel) -> String {
    let mut terminal = Terminal::new(TestBackend::new(118, 44)).expect("test terminal");
    terminal
        .draw(|frame| {
            render(model, frame);
        })
        .expect("draw");
    let buffer = terminal.backend().buffer();
    (0..buffer.area.height)
        .map(|y| {
            let mut line = String::new();
            for x in 0..buffer.area.width {
                line.push_str(buffer[(x, y)].symbol());
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn raw_workflow_started(session: SessionId, seq: u64) -> RawEnvelope {
    let root = GraphNodeName::new("PLAN").expect("valid node");
    let ast = WorkflowActivationAst {
        workflow_id: "release".to_owned(),
        workflow_digest: "blake3:compiled-release".to_owned(),
        input_type: "brief".to_owned(),
        output_type: "report".to_owned(),
        nodes: vec![WorkflowActivationNode {
            node: root.clone(),
            input_type: "brief".to_owned(),
            output_type: "report".to_owned(),
            join: WorkflowJoinSemantics {
                initial_all: vec![1],
                reactivate_any: Vec::new(),
            },
            convergence_gate: false,
        }],
        edges: vec![WorkflowActivationEdge {
            id: 1,
            kind: WorkflowEdgeKind::GraphInput,
            from: None,
            to: root,
            evidence_type: "brief".to_owned(),
        }],
        max_back_edge_activations: 1,
    };
    let started = WorkflowGraphJournalEvent::WorkflowGraphStarted(Box::new(WorkflowGraphStarted {
        graph_id: GraphId::new("graph-release"),
        ast_digest: workflow_activation_ast_digest(&ast),
        ast,
        seed: Some(InstructEvidenceRef::new(
            ArtifactRef::new(format!("blake3:{}", "a".repeat(64))),
            "brief",
            7,
            Vec::new(),
        )),
    }));
    EventEnvelope {
        schema_version: 1,
        event_id: EventId::new(format!("workflow-event-{seq}")),
        seq,
        session_id: session,
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: DeviceId::new("workflow-view-test"),
        authority_epoch: 1,
        worker_generation: 1,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 1_000,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: started.to_payload_value().expect("workflow payload"),
    }
}

fn l2_state(
    workflow_id: &str,
    graph_id: &str,
    cursor: u64,
) -> haider_protocol::graph::WorkflowGraphState {
    let root = GraphNodeName::new("PLAN").expect("valid node");
    let ast = WorkflowActivationAst {
        workflow_id: workflow_id.to_owned(),
        workflow_digest: format!("blake3:{workflow_id}"),
        input_type: "brief".to_owned(),
        output_type: "report".to_owned(),
        nodes: vec![WorkflowActivationNode {
            node: root.clone(),
            input_type: "brief".to_owned(),
            output_type: "report".to_owned(),
            join: WorkflowJoinSemantics {
                initial_all: vec![1],
                reactivate_any: Vec::new(),
            },
            convergence_gate: false,
        }],
        edges: vec![WorkflowActivationEdge {
            id: 1,
            kind: WorkflowEdgeKind::GraphInput,
            from: None,
            to: root,
            evidence_type: "brief".to_owned(),
        }],
        max_back_edge_activations: 1,
    };
    let seed = InstructEvidenceRef::new(
        ArtifactRef::new(format!("blake3:{}", "a".repeat(64))),
        "brief",
        7,
        Vec::new(),
    );
    haider_protocol::graph::WorkflowGraphState::from_started(
        cursor,
        WorkflowGraphStarted {
            graph_id: GraphId::new(graph_id),
            ast_digest: workflow_activation_ast_digest(&ast),
            ast,
            seed: Some(seed),
        },
    )
    .expect("valid L2 baseline")
}

#[test]
fn runtime_dag_lights_inputs_and_keeps_fork_join_visible() {
    let mut projection = WorkflowGraphProjection::default();
    projection
        .replace(runtime_state(
            73,
            vec![
                runtime_node("PLAN", WorkflowNodeState::Complete, &[true]),
                runtime_node("BUILD", WorkflowNodeState::Active, &[true]),
                runtime_node("DOCS", WorkflowNodeState::Ready, &[true]),
                runtime_node("VERIFY", WorkflowNodeState::Waiting, &[true, false]),
            ],
            diamond_edges(),
        ))
        .expect("valid runtime state");

    let (text, lines) = rendered(&projection);
    assert!(text.contains("LIVE  · cursor 73"), "{text}");
    assert!(text.contains("fork → BUILD + DOCS"), "{text}");
    assert!(text.contains("join ← BUILD + DOCS"), "{text}");
    assert!(text.contains("◉ BUILD  active"), "{text}");
    assert!(text.contains("◆ DOCS  ready"), "{text}");
    assert!(text.contains("VERIFY  waiting  inputs ●○"), "{text}");
    assert!(text.contains("← BUILD + DOCS"), "{text}");

    let theme = ThemeKey::Dark.theme();
    let active = lines
        .iter()
        .flat_map(|line| &line.spans)
        .find(|span| span.content.as_ref() == "BUILD")
        .expect("active node span");
    assert_eq!(active.style.bg, Some(Color::from(theme.sel_bg)));
}

#[test]
fn runtime_edges_do_not_invent_a_fork_for_unrelated_same_layer_nodes() {
    let mut projection = WorkflowGraphProjection::default();
    projection
        .replace(WorkflowGraphState {
            graph_id: "graph-frozen-runtime".to_owned(),
            workflow_id: "catalog-name-can-be-revised".to_owned(),
            workflow_digest: "blake3:frozen-runtime".to_owned(),
            cursor: 9,
            nodes: vec![
                runtime_node("ROOT_A", WorkflowNodeState::Complete, &[true]),
                runtime_node("ROOT_B", WorkflowNodeState::Active, &[true]),
                runtime_node("NEXT", WorkflowNodeState::Ready, &[true]),
            ],
            edges: vec![edge(WorkflowGraphEdgeKind::Forward, Some("ROOT_A"), "NEXT")],
        })
        .expect("valid frozen topology");

    let (text, _) = rendered(&projection);
    assert!(text.contains("ROOT_A"), "{text}");
    assert!(text.contains("ROOT_B"), "{text}");
    assert!(text.contains("NEXT"), "{text}");
    assert!(!text.contains("fork →"), "{text}");
    assert!(!text.contains("join ←"), "{text}");
}

#[test]
fn feature_downgrade_never_renders_a_retained_projection_as_live() {
    let mut model = AppModel::new();
    model.mode = RuntimeMode::Live;
    model.screen = Screen::Loom;
    model.loom_pane = LoomPane::Workflows;
    model.loom_selection = 1;
    model.loom_detail = true;
    model.loom_loaded = true;
    model.daemon_features.extend([
        haider_rpc::FEATURE_LOOM_V1.to_owned(),
        haider_rpc::FEATURE_WORKFLOW_CATALOG_V1.to_owned(),
        haider_rpc::FEATURE_WORKFLOW_GRAPH_V1.to_owned(),
    ]);
    model.workflow_catalog = vec![WorkflowCatalogEntryV1::BuiltIn {
        id: "release".to_owned(),
        main_session_eligible: true,
        template: template(),
    }];
    model
        .workflow_graph
        .replace(runtime_state(
            73,
            vec![
                runtime_node("PLAN", WorkflowNodeState::Complete, &[true]),
                runtime_node("BUILD", WorkflowNodeState::Active, &[true]),
                runtime_node("DOCS", WorkflowNodeState::Ready, &[true]),
                runtime_node("VERIFY", WorkflowNodeState::Waiting, &[false, false]),
            ],
            diamond_edges(),
        ))
        .expect("retained pre-downgrade projection");
    model.workflow_evidence_inspection = Some(WorkflowEvidenceInspection {
        node_id: "DOCS".to_owned(),
        code: "abandoned".to_owned(),
        message: "stale inspection".to_owned(),
        cursor: 72,
        reference: None,
    });
    let mut driver = LiveDriver::new("workflow-feature-downgrade");
    driver.apply(
        &mut model,
        LiveReply::Handshake {
            features: [
                haider_rpc::FEATURE_LOOM_V1.to_owned(),
                haider_rpc::FEATURE_WORKFLOW_CATALOG_V1.to_owned(),
            ]
            .into_iter()
            .collect(),
            version: "pre-workflow-graph".to_owned(),
        },
    );

    let text = rendered_screen(&model);
    assert!(model.workflow_evidence_inspection.is_none());
    assert!(text.contains("DAG"), "{text}");
    assert!(!text.contains("LIVE"), "{text}");
    assert!(!text.contains("◉ BUILD"), "{text}");
    assert!(!text.contains("REJECT EVIDENCE"), "{text}");
}

#[test]
fn tab_into_workflows_resumes_the_retained_runtime_cursor() {
    let session = SessionId::new("session-tab-workflow");
    let mut model = AppModel::new();
    model.mode = RuntimeMode::Live;
    model.screen = Screen::Loom;
    model.loom_pane = LoomPane::Types;
    model.active_session = Some(session.clone());
    model.loom_loaded = true;
    model.daemon_features.extend([
        haider_rpc::FEATURE_LOOM_V1.to_owned(),
        haider_rpc::FEATURE_WORKFLOW_CATALOG_V1.to_owned(),
        haider_rpc::FEATURE_WORKFLOW_GRAPH_V1.to_owned(),
    ]);
    model.workflow_catalog = vec![WorkflowCatalogEntryV1::BuiltIn {
        id: "release".to_owned(),
        main_session_eligible: true,
        template: template(),
    }];
    model.apply_workflow_graph_state(
        &session,
        Some(l2_state("release", "graph-tab-workflow", 61)),
    );
    model.requests.clear();

    model.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Tab,
        KeyModifiers::NONE,
    )));

    assert_eq!(model.loom_pane, LoomPane::Workflows);
    assert_eq!(model.loom_selection, 1, "the live row becomes discoverable");
    assert!(
        model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::WorkflowGraphResume)),
        "Tab entry must consume the retained cursor: {:?}",
        model.requests
    );
}

#[test]
fn dark_waiting_node_and_reject_evidence_are_explicit() {
    let mut rejected = runtime_node("DOCS", WorkflowNodeState::Rejected, &[true]);
    let rejection_artifact = ArtifactRef::new(format!("blake3:{}", "c".repeat(64)));
    rejected.evidence_refs = vec![WorkflowEvidenceRef::new(rejection_artifact.clone())];
    rejected.rejection = Some(WorkflowNodeRejection {
        code: WorkflowNodeRejectCode::EvidenceRejected,
        message: "docs evidence failed verification".to_owned(),
        cursor: 84,
        evidence: Some(WorkflowEvidenceRef::new(rejection_artifact.clone())),
    });
    let mut projection = WorkflowGraphProjection::default();
    projection
        .replace(runtime_state(
            84,
            vec![
                runtime_node("PLAN", WorkflowNodeState::Complete, &[true]),
                runtime_node("BUILD", WorkflowNodeState::Waiting, &[false]),
                rejected,
                runtime_node("VERIFY", WorkflowNodeState::Waiting, &[false, false]),
            ],
            diamond_edges(),
        ))
        .expect("valid runtime state");

    let (text, _) = rendered(&projection);
    assert!(text.contains("○ BUILD  waiting on evidence"), "{text}");
    assert!(text.contains("✗ DOCS  rejected"), "{text}");
    assert!(
        text.contains("↳ reject evidence rejected · journal cursor 84"),
        "{text}"
    );
    assert!(
        text.contains(&format!("evidence {}", rejection_artifact.as_str())),
        "{text}"
    );
}

#[test]
fn enter_opens_a_rejects_exact_evidence_coordinate_for_inspection() {
    let mut rejected = runtime_node("DOCS", WorkflowNodeState::Rejected, &[true]);
    let evidence = ArtifactRef::new(format!("blake3:{}", "c".repeat(64)));
    rejected.evidence_refs = vec![WorkflowEvidenceRef::new(evidence.clone())];
    rejected.rejection = Some(WorkflowNodeRejection {
        code: WorkflowNodeRejectCode::EvidenceRejected,
        message: "docs evidence failed verification".to_owned(),
        cursor: 84,
        evidence: Some(WorkflowEvidenceRef::new(evidence.clone())),
    });
    let mut model = AppModel::new();
    model.mode = RuntimeMode::Live;
    model.screen = Screen::Loom;
    model.loom_pane = LoomPane::Workflows;
    model.loom_selection = 1;
    model.loom_detail = true;
    model.loom_loaded = true;
    model.daemon_features.extend([
        haider_rpc::FEATURE_WORKFLOW_CATALOG_V1.to_owned(),
        haider_rpc::FEATURE_WORKFLOW_GRAPH_V1.to_owned(),
    ]);
    model.workflow_catalog = vec![WorkflowCatalogEntryV1::BuiltIn {
        id: "release".to_owned(),
        main_session_eligible: true,
        template: template(),
    }];
    model
        .workflow_graph
        .replace(runtime_state(84, vec![rejected], Vec::new()))
        .expect("valid rejection projection");

    model.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )));
    let inspection = model
        .workflow_evidence_inspection
        .as_ref()
        .expect("Enter opens reject evidence");
    assert_eq!(inspection.node_id, "DOCS");
    assert_eq!(inspection.reference.as_deref(), Some(evidence.as_str()));

    model.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Esc,
        KeyModifiers::NONE,
    )));
    assert!(model.workflow_evidence_inspection.is_none());
    assert!(model.loom_detail, "Esc closes evidence before row detail");
}

#[test]
fn evidence_less_and_concurrent_rejects_are_each_inspectable() {
    let mut plan = runtime_node("PLAN", WorkflowNodeState::Rejected, &[true]);
    plan.rejection = Some(WorkflowNodeRejection {
        code: WorkflowNodeRejectCode::TypedInputMissing,
        message: "PLAN did not receive its typed brief".to_owned(),
        cursor: 85,
        evidence: None,
    });
    let mut docs = runtime_node("DOCS", WorkflowNodeState::Rejected, &[true]);
    docs.rejection = Some(WorkflowNodeRejection {
        code: WorkflowNodeRejectCode::Abandoned,
        message: "DOCS was abandoned".to_owned(),
        cursor: 86,
        evidence: None,
    });
    let mut model = AppModel::new();
    model.mode = RuntimeMode::Live;
    model.screen = Screen::Loom;
    model.loom_pane = LoomPane::Workflows;
    model.loom_selection = 1;
    model.loom_detail = true;
    model.loom_loaded = true;
    model.daemon_features.extend([
        haider_rpc::FEATURE_WORKFLOW_CATALOG_V1.to_owned(),
        haider_rpc::FEATURE_WORKFLOW_GRAPH_V1.to_owned(),
    ]);
    model.workflow_catalog = vec![WorkflowCatalogEntryV1::BuiltIn {
        id: "release".to_owned(),
        main_session_eligible: true,
        template: template(),
    }];
    model
        .workflow_graph
        .replace(runtime_state(86, vec![plan, docs], Vec::new()))
        .expect("valid concurrent rejection view");

    model.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )));
    let first = model
        .workflow_evidence_inspection
        .as_ref()
        .expect("first rejection opens");
    assert_eq!(first.node_id, "PLAN");
    assert_eq!(first.code, "typed input missing");
    assert_eq!(first.cursor, 85);
    assert!(first.reference.is_none());

    model.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )));
    let second = model
        .workflow_evidence_inspection
        .as_ref()
        .expect("second rejection opens");
    assert_eq!(second.node_id, "DOCS");
    assert_eq!(second.message, "DOCS was abandoned");
    assert!(second.reference.is_none());
}

#[test]
fn state_and_cursor_watch_cross_the_link_as_typed_rpc_variants() {
    let session = SessionId::new("session-live-workflow");
    let state_command = LiveCommand::WorkflowGraphState {
        session: session.clone(),
    };
    let state_context = CommandContext::of(&state_command);
    assert!(matches!(
        request_body(state_command),
        RequestBody::WorkflowGraphState {
            session_id,
            graph_id: None,
        } if session_id == session
    ));
    assert!(matches!(
        map_response(
            &state_context,
            ResponseBody::WorkflowGraphState { state: None }
        )
        .as_slice(),
        [LiveReply::WorkflowGraphState {
            session: reply_session,
            state,
        }] if reply_session == &session && state.is_none()
    ));

    let watch_command = LiveCommand::WorkflowGraphWatch {
        session: session.clone(),
        after_cursor: 91,
    };
    let watch_context = CommandContext::of(&watch_command);
    assert!(matches!(
        request_body(watch_command),
        RequestBody::WorkflowGraphWatch {
            session_id,
            after_cursor: 91,
            limit,
        } if session_id == session
            && limit == haider_tui::live::WORKFLOW_GRAPH_WATCH_PAGE
    ));
    let page = haider_protocol::graph::WorkflowGraphWatchPage {
        requested_after_cursor: 91,
        replay_through_cursor: 91,
        next_cursor: 91,
        events: Vec::new(),
    };
    assert!(matches!(
        map_response(
            &watch_context,
            ResponseBody::WorkflowGraphWatch { page }
        )
        .as_slice(),
        [LiveReply::WorkflowGraphPage {
            session: reply_session,
            page,
        }] if reply_session == &session && page.next_cursor == 91
    ));
}

#[test]
fn workflow_journal_facts_are_known_noops_in_active_and_background_sessions() {
    let active = SessionId::new("session-active-workflow");
    let background = SessionId::new("session-background-workflow");
    let mut model = AppModel::new();
    model.mode = RuntimeMode::Live;
    model.active_session = Some(active.clone());
    model.upsert_live_session(&background);

    model.route_raw(&raw_workflow_started(active, 1));
    assert_eq!(model.projection.unknown_payloads(), 0);

    model.route_raw(&raw_workflow_started(background.clone(), 1));
    let background_projection = &model
        .sessions
        .iter()
        .find(|session| session.id == background)
        .expect("background session")
        .projection;
    assert_eq!(background_projection.unknown_payloads(), 0);
}

#[test]
fn stale_page_during_session_switch_resumes_the_new_sessions_retained_cursor() {
    let session_a = SessionId::new("session-a");
    let session_b = SessionId::new("session-b");
    let mut model = AppModel::new();
    model.mode = RuntimeMode::Live;
    model.screen = Screen::Loom;
    model.loom_pane = LoomPane::Workflows;
    model.active_session = Some(session_a.clone());
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_WORKFLOW_GRAPH_V1.to_owned());
    let mut driver = LiveDriver::new("workflow-view-test");

    assert!(matches!(
        driver
            .handle_request(&mut model, AppRequest::WorkflowGraphRefresh)
            .as_slice(),
        [LiveCommand::WorkflowGraphState { session }] if session == &session_a
    ));
    model.active_session = Some(session_b.clone());
    model.apply_workflow_graph_state(
        &session_b,
        Some(l2_state("release-b", "graph-release-b", 77)),
    );
    assert!(
        driver
            .handle_request(&mut model, AppRequest::WorkflowGraphResume)
            .is_empty(),
        "B's cursor resume folds behind A's in-flight read"
    );

    let follow = driver.apply(
        &mut model,
        LiveReply::WorkflowGraphPage {
            session: session_a,
            page: Box::new(haider_protocol::graph::WorkflowGraphWatchPage {
                requested_after_cursor: 0,
                replay_through_cursor: 0,
                next_cursor: 0,
                events: Vec::new(),
            }),
        },
    );
    assert!(matches!(
        follow.as_slice(),
        [LiveCommand::WorkflowGraphWatch {
            session,
            after_cursor: 77,
        }] if session == &session_b
    ));
}

#[test]
fn reconnect_resumes_the_retained_workflow_cursor_and_empty_state_rebaselines() {
    let session = SessionId::new("session-reconnect-workflow");
    let mut model = AppModel::new();
    model.mode = RuntimeMode::Live;
    model.screen = Screen::Loom;
    model.loom_pane = LoomPane::Workflows;
    model.active_session = Some(session.clone());
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_WORKFLOW_GRAPH_V1.to_owned());
    let baseline = l2_state("release", "graph-release", 91);
    model.apply_workflow_graph_state(&session, Some(baseline.clone()));
    model.workflow_evidence_inspection = Some(WorkflowEvidenceInspection {
        node_id: "PLAN".to_owned(),
        code: "evidence rejected".to_owned(),
        message: "obsolete after retry".to_owned(),
        cursor: 91,
        reference: None,
    });
    let mut driver = LiveDriver::new("workflow-reconnect-test");

    let resumed = driver.apply(&mut model, LiveReply::Reconnected);
    assert!(
        resumed.iter().any(|command| matches!(
            command,
            LiveCommand::WorkflowGraphWatch {
                session: watched,
                after_cursor: 91,
            } if watched == &session
        )),
        "reconnect must reduce the offline suffix from the retained cursor: {resumed:?}"
    );
    let input = WorkflowNodeInput {
        edge_id: 1,
        evidence: baseline.seed.clone().expect("baseline seed"),
    };
    let activated = WorkflowGraphJournalEvent::WorkflowNodeActivated(WorkflowNodeActivated {
        graph_id: baseline.graph_id,
        node: GraphNodeName::new("PLAN").expect("valid node"),
        iteration: 1,
        activation_order: 1,
        cause: WorkflowActivationCause::ForwardJoin,
        input_ledger_digest: workflow_input_ledger_digest(std::slice::from_ref(&input)),
        inputs: vec![input],
    });
    let follow = driver.apply(
        &mut model,
        LiveReply::WorkflowGraphPage {
            session: session.clone(),
            page: Box::new(haider_protocol::graph::WorkflowGraphWatchPage {
                requested_after_cursor: 91,
                replay_through_cursor: 92,
                next_cursor: 92,
                events: vec![WorkflowGraphWatchEvent {
                    cursor: 92,
                    event: activated,
                }],
            }),
        },
    );
    assert!(follow.is_empty());
    assert_eq!(model.workflow_graph.cursor(), Some(92));
    assert!(
        model.workflow_evidence_inspection.is_none(),
        "a changed node cannot retain an obsolete reject inspector"
    );
    assert_eq!(
        model.workflow_graph.node("PLAN").map(|node| node.status),
        Some(WorkflowNodeState::Active)
    );

    let empty_session = SessionId::new("session-empty-workflow");
    let mut empty = AppModel::new();
    empty.mode = RuntimeMode::Live;
    empty.screen = Screen::Loom;
    empty.loom_pane = LoomPane::Workflows;
    empty.active_session = Some(empty_session.clone());
    empty
        .daemon_features
        .insert(haider_rpc::FEATURE_WORKFLOW_GRAPH_V1.to_owned());
    let mut empty_driver = LiveDriver::new("workflow-empty-reconnect-test");
    let rebaseline = empty_driver.apply(&mut empty, LiveReply::Reconnected);
    assert!(
        rebaseline.iter().any(|command| matches!(
            command,
            LiveCommand::WorkflowGraphState { session } if session == &empty_session
        )),
        "a reconnect without a cursor must establish a baseline: {rebaseline:?}"
    );
}

#[test]
fn rejected_watch_cursor_rebaselines_without_discarding_last_good_view() {
    let session = SessionId::new("session-watch-repair");
    let mut model = AppModel::new();
    model.mode = RuntimeMode::Live;
    model.screen = Screen::Loom;
    model.loom_pane = LoomPane::Workflows;
    model.active_session = Some(session.clone());
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_WORKFLOW_GRAPH_V1.to_owned());
    model.apply_workflow_graph_state(
        &session,
        Some(l2_state("release", "graph-watch-repair", 31)),
    );
    let before = model.workflow_graph.clone();
    let mut driver = LiveDriver::new("workflow-watch-repair-test");
    assert!(matches!(
        driver
            .handle_request(&mut model, AppRequest::WorkflowGraphResume)
            .as_slice(),
        [LiveCommand::WorkflowGraphWatch {
            after_cursor: 31,
            ..
        }]
    ));

    let repair = driver.apply(
        &mut model,
        LiveReply::WorkflowGraphFailed {
            session: session.clone(),
            operation: WorkflowGraphRead::Watch,
            message: "cursor is ahead of durable head".to_owned(),
        },
    );

    assert!(matches!(
        repair.as_slice(),
        [LiveCommand::WorkflowGraphState { session: repaired }] if repaired == &session
    ));
    assert_eq!(model.workflow_graph, before);
    assert_eq!(
        model.workflow_graph_error.as_deref(),
        Some("cursor is ahead of durable head")
    );
}
