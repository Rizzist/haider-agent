//! M2e parent-attempt attachment and single-collapse laws.

#![allow(clippy::expect_used, clippy::panic)]

use haider_protocol::EventPayload;
use haider_protocol::agent::{
    AgentManifest, AgentRole, ChildReport, Grant, Placement, ReportVerification,
};
use haider_protocol::effect::EffectClass;
use haider_protocol::graph::{
    ChildContractRef, ChildGraphAttached, ChildTemplateCacheKey, ChildWorkflowSelector,
    EvidenceAuthority, EvidenceSlotSpec, EvidenceVerdict, GraphEvidenceSource, GraphExecutorShape,
    GraphGateKind, GraphNodeName, GraphNodeSpec, GraphTemplateSpec, ParentGraphAttempt,
    SHIP_LOOP_TEMPLATE, SubjectSelector, child_contract_subject_digest, child_gate_structure,
    graph_template_digest, implement_verify_child_template, verify_node,
};
use haider_protocol::ids::{
    AgentId, DeviceId, EventId, GraphId, ItemId, LeaseId, RunId, SessionId, WorkspaceRevision,
};
use haider_store::{
    ChildGraphAttachCommand, ChildGraphAttachOutcome, DelegationRecord, DelegationState,
    EventStore, GraphAbandonCommand, GraphEvidenceCommand, GraphEvidenceOutcome, GraphPinCommand,
    GraphSwitchCommand, SessionCreateCommand, Store,
};

fn create_session(store: &Store, name: &str) -> SessionId {
    let session_id = SessionId::new(name);
    store
        .create_session(&SessionCreateCommand {
            command_id: format!("create-{name}"),
            request_digest: format!("create-digest-{name}"),
            request_json: format!(r#"{{"session":"{name}"}}"#),
            session_id: session_id.clone(),
            cwd: "/tmp".into(),
            provider: "fake".into(),
            model: "fake-v1".into(),
            max_tokens: 4096,
            permission_overrides: None,
            effort: None,
            fast: false,
            cache_policy: Default::default(),
            system_prompt_version: "m2e-test-v1".into(),
            event_id: EventId::new(format!("created-{name}")),
            device_id: DeviceId::new("m2e-test"),
        })
        .expect("create session");
    session_id
}

#[test]
fn m2e_authored_replacement_cannot_launder_model_testimony_into_daemon_authority() {
    // MUTATION CHECK: trust only the initial child template, or skip final
    // process-proof validation. Expected failure: the green collapse below is
    // accepted after workflow_author replaces it with a model-only graph.
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("open store");
    let parent = create_session(&store, "m2e-authority-parent");
    let child = create_session(&store, "m2e-authority-child");
    let parent_graph = GraphId::new("m2e-authority-parent-graph");
    let child_graph = GraphId::new("m2e-authority-child-graph");
    pin(
        &store,
        &parent,
        &parent_graph,
        SHIP_LOOP_TEMPLATE,
        "authority-parent",
    );
    pin(
        &store,
        &child,
        &child_graph,
        "child-implement-verify",
        "authority-child",
    );
    store
        .record_graph_evidence(&GraphEvidenceCommand {
            command_id: "m2e-authority-parent-build".into(),
            request_digest: "m2e-authority-parent-build-digest".into(),
            request_json: r#"{"node":"BUILD"}"#.into(),
            session_id: parent.clone(),
            worker_generation: store.worker_generation(),
            run_id: RunId::new("m2e-authority-parent-build-run"),
            call_id: "m2e-authority-parent-build-call".into(),
            graph_id: parent_graph.clone(),
            node: haider_protocol::graph::build_node(),
            verdict: EvidenceVerdict::Green,
            detail: "parent build ready".into(),
            slot: None,
            subject_digest: None,
            signal: None,
            child_contract: None,
            device_id: DeviceId::new("m2e-test"),
        })
        .expect("advance parent");
    let record = delegation(&parent, &child);
    store.create_delegation(&record).expect("create delegation");
    let template = implement_verify_child_template();
    let attachment = ChildGraphAttached {
        parent_run_id: record.parent_run_id.clone(),
        parent_call_id: record.call_id.clone(),
        parent_tool_item_id: record.tool_item_id.clone(),
        parent_attempt: ParentGraphAttempt {
            graph_id: parent_graph.clone(),
            node: verify_node(),
            attempt: 1,
        },
        parent_slot: "tests".into(),
        parent_authority: EvidenceAuthority::DaemonVerified,
        child_session_id: child.clone(),
        child_run_id: record.child_run_id.clone(),
        child_graph_id: child_graph.clone(),
        workflow: ChildWorkflowSelector::Deeper,
        template: template.name.clone(),
        digest: graph_template_digest(&template),
        gate_reason: "distinct_review".into(),
        cache_key: ChildTemplateCacheKey {
            task_shape: "distinct_review".into(),
            effective_grant_digest: "grant".into(),
            gate_structure: child_gate_structure(&template),
        },
        cache_hit: false,
        workflow_author: true,
    };
    store
        .attach_child_graph(&ChildGraphAttachCommand {
            command_id: "m2e-authority-attach".into(),
            request_digest: "m2e-authority-attach-digest".into(),
            request_json: serde_json::to_string(&attachment).expect("attachment json"),
            session_id: parent.clone(),
            parent_branch_id: None,
            worker_generation: store.worker_generation(),
            attachment,
            device_id: DeviceId::new("m2e-test"),
        })
        .expect("attach workflow child");

    let authored_node = GraphNodeName::new("MODEL_ONLY").expect("node");
    let authored = GraphTemplateSpec {
        name: "authored-model-only".into(),
        version: 1,
        start_node: Some(authored_node.clone()),
        nodes: vec![GraphNodeSpec {
            name: authored_node.clone(),
            gate: GraphGateKind::AllOfN { n: 1 },
            executor: GraphExecutorShape::Inline,
            max_attempts: 1,
            max_evidence_per_attempt: Some(1),
            depends_on: Vec::new(),
            verify_slots: vec![EvidenceSlotSpec {
                id: "model".into(),
                authority: EvidenceAuthority::ModelAttested,
                subject_selector: SubjectSelector::WorkspaceRevision,
            }],
        }],
    };
    let replacement_graph = GraphId::new("m2e-authority-model-only");
    store
        .switch_graph(&GraphSwitchCommand {
            command_id: "m2e-authority-switch".into(),
            request_digest: "m2e-authority-switch-digest".into(),
            request_json: r#"{"template":"authored-model-only"}"#.into(),
            session_id: child.clone(),
            worker_generation: store.worker_generation(),
            old_graph_id: child_graph,
            new_graph_id: replacement_graph.clone(),
            template: authored.name.clone(),
            template_spec: Some(authored),
            device_id: DeviceId::new("m2e-test"),
        })
        .expect("author model-only replacement");
    store
        .record_graph_evidence(&GraphEvidenceCommand {
            command_id: "m2e-authority-model-green".into(),
            request_digest: "m2e-authority-model-green-digest".into(),
            request_json: r#"{"slot":"model"}"#.into(),
            session_id: child.clone(),
            worker_generation: store.worker_generation(),
            run_id: record.child_run_id.clone(),
            call_id: "m2e-authority-model-call".into(),
            graph_id: replacement_graph.clone(),
            node: authored_node,
            verdict: EvidenceVerdict::Green,
            detail: "model says replacement passed".into(),
            slot: Some("model".into()),
            subject_digest: Some("model-revision".into()),
            signal: None,
            child_contract: None,
            device_id: DeviceId::new("m2e-test"),
        })
        .expect("complete model-only replacement");
    let revision = WorkspaceRevision::new("model-revision");
    let report = ChildReport {
        agent: record.agent_id.clone(),
        summary: "model-only workflow claimed success".into(),
        verified: ReportVerification::Verified,
        workspace_revision: Some(revision.clone()),
    };
    store
        .record_delegation_report(&record.agent_id, &report)
        .expect("record report");
    let contract = ChildContractRef {
        child_session_id: child,
        child_run_id: record.child_run_id,
        child_graph_id: replacement_graph,
        report_digest: blake3::hash(&serde_json::to_vec(&report).expect("report json"))
            .to_hex()
            .to_string(),
        workspace_revision: Some(revision),
    };
    let error = store
        .record_graph_evidence(&GraphEvidenceCommand {
            command_id: "m2e-authority-collapse".into(),
            request_digest: "m2e-authority-collapse-digest".into(),
            request_json: r#"{"child":"model-only"}"#.into(),
            session_id: parent,
            worker_generation: store.worker_generation(),
            run_id: record.parent_run_id,
            call_id: record.call_id,
            graph_id: parent_graph,
            node: verify_node(),
            verdict: EvidenceVerdict::Green,
            detail: "model-only child claims daemon slot".into(),
            slot: Some("tests".into()),
            subject_digest: Some(child_contract_subject_digest(&contract)),
            signal: None,
            child_contract: Some(contract),
            device_id: DeviceId::new("m2e-test"),
        })
        .expect_err("model-only replacement cannot grow daemon authority");
    assert_eq!(
        error
            .details
            .as_ref()
            .and_then(|details| details.get("kind"))
            .and_then(serde_json::Value::as_str),
        Some("child_authority_growth")
    );
}

fn pin(store: &Store, session: &SessionId, graph: &GraphId, template: &str, suffix: &str) {
    store
        .pin_graph(&GraphPinCommand {
            command_id: format!("pin-{suffix}"),
            request_digest: format!("pin-digest-{suffix}"),
            request_json: format!(r#"{{"template":"{template}"}}"#),
            session_id: session.clone(),
            worker_generation: store.worker_generation(),
            graph_id: graph.clone(),
            template: template.into(),
            device_id: DeviceId::new("m2e-test"),
        })
        .expect("pin graph");
}

fn delegation(parent: &SessionId, child: &SessionId) -> DelegationRecord {
    let agent = AgentId::new("m2e-child-agent");
    DelegationRecord {
        agent_id: agent.clone(),
        child_session_id: child.clone(),
        child_run_id: RunId::new("m2e-child-run"),
        parent_session_id: parent.clone(),
        parent_run_id: RunId::new("m2e-parent-run"),
        parent_branch_id: None,
        call_id: "m2e-parent-call".into(),
        tool_item_id: ItemId::new("m2e-parent-tool"),
        parent_agent_id: None,
        root_session_id: parent.clone(),
        depth: 1,
        task: "workflow child".into(),
        prompt: "perform the workflow".into(),
        manifest: AgentManifest {
            agent,
            role: AgentRole::Subagent,
            task: "workflow child".into(),
            callsign: None,
            model_profile: "fake-v1".into(),
            grant: Grant {
                tools: vec!["graph_evidence".into(), "process_exec".into()],
                effect_ceiling: vec![EffectClass::ProcessExec],
            },
            budget_tokens: Some(4096),
            placement: Placement::Local,
            lease: LeaseId::new("m2e-child-lease"),
            fencing_epoch: store_generation_placeholder(),
            attempt: 0,
            parent: None,
            coordinates: None,
        },
        state: DelegationState::Spawned,
        report: None,
    }
}

const fn store_generation_placeholder() -> u64 {
    1
}

#[test]
fn m2e_exact_parent_attempt_attaches_once_and_collapses_once_without_authority_growth() {
    // MUTATION CHECK: accept another parent epoch, emit zero/two collapsed
    // evidence facts, drop child report/revision provenance, or trust a child
    // to upgrade ModelAttested into DaemonVerified. Expected failures are the
    // assertions at each boundary below.
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("open store");
    let parent = create_session(&store, "m2e-parent");
    let child = create_session(&store, "m2e-child");
    let parent_graph = GraphId::new("m2e-parent-graph");
    let child_graph = GraphId::new("m2e-child-graph");
    pin(&store, &parent, &parent_graph, SHIP_LOOP_TEMPLATE, "parent");
    pin(
        &store,
        &child,
        &child_graph,
        "child-implement-verify",
        "child",
    );

    // Move the parent to its declared daemon-verified VERIFY slots.
    store
        .record_graph_evidence(&GraphEvidenceCommand {
            command_id: "m2e-parent-build".into(),
            request_digest: "m2e-parent-build-digest".into(),
            request_json: r#"{"node":"BUILD"}"#.into(),
            session_id: parent.clone(),
            worker_generation: store.worker_generation(),
            run_id: RunId::new("m2e-parent-build-run"),
            call_id: "m2e-parent-build-call".into(),
            graph_id: parent_graph.clone(),
            node: haider_protocol::graph::build_node(),
            verdict: EvidenceVerdict::Green,
            detail: "parent build ready".into(),
            slot: None,
            subject_digest: None,
            signal: None,
            child_contract: None,
            device_id: DeviceId::new("m2e-test"),
        })
        .expect("advance parent to verify");

    let record = delegation(&parent, &child);
    store.create_delegation(&record).expect("create delegation");
    let child_template = implement_verify_child_template();
    let cache_key = ChildTemplateCacheKey {
        task_shape: "mutation_verify".into(),
        effective_grant_digest: "grant".into(),
        gate_structure: child_gate_structure(&child_template),
    };
    let attachment = ChildGraphAttached {
        parent_run_id: record.parent_run_id.clone(),
        parent_call_id: record.call_id.clone(),
        parent_tool_item_id: record.tool_item_id.clone(),
        parent_attempt: ParentGraphAttempt {
            graph_id: parent_graph.clone(),
            node: verify_node(),
            attempt: 1,
        },
        parent_slot: "tests".into(),
        parent_authority: EvidenceAuthority::DaemonVerified,
        child_session_id: child.clone(),
        child_run_id: record.child_run_id.clone(),
        child_graph_id: child_graph.clone(),
        workflow: ChildWorkflowSelector::ImplementVerify,
        template: child_template.name.clone(),
        digest: graph_template_digest(&child_template),
        gate_reason: "mutation_with_independent_verification".into(),
        cache_key,
        cache_hit: false,
        workflow_author: false,
    };

    let mut wrong_attempt = attachment.clone();
    wrong_attempt.parent_attempt.attempt = 2;
    let error = store
        .attach_child_graph(&ChildGraphAttachCommand {
            command_id: "m2e-wrong-attempt".into(),
            request_digest: "m2e-wrong-attempt-digest".into(),
            request_json: serde_json::to_string(&wrong_attempt).expect("attachment json"),
            session_id: parent.clone(),
            parent_branch_id: None,
            worker_generation: store.worker_generation(),
            attachment: wrong_attempt,
            device_id: DeviceId::new("m2e-test"),
        })
        .expect_err("a different parent attempt cannot adopt the child");
    assert_eq!(
        error
            .details
            .as_ref()
            .and_then(|details| details.get("kind"))
            .and_then(serde_json::Value::as_str),
        Some("stale_child_attachment")
    );

    let mut grown = attachment.clone();
    grown.parent_authority = EvidenceAuthority::ModelAttested;
    let error = store
        .attach_child_graph(&ChildGraphAttachCommand {
            command_id: "m2e-grown-authority".into(),
            request_digest: "m2e-grown-authority-digest".into(),
            request_json: serde_json::to_string(&grown).expect("attachment json"),
            session_id: parent.clone(),
            parent_branch_id: None,
            worker_generation: store.worker_generation(),
            attachment: grown,
            device_id: DeviceId::new("m2e-test"),
        })
        .expect_err("attachment cannot rewrite the declared parent authority");
    assert_eq!(
        error
            .details
            .as_ref()
            .and_then(|details| details.get("kind"))
            .and_then(serde_json::Value::as_str),
        Some("child_authority_growth")
    );

    let attached = store
        .attach_child_graph(&ChildGraphAttachCommand {
            command_id: "m2e-attach".into(),
            request_digest: "m2e-attach-digest".into(),
            request_json: serde_json::to_string(&attachment).expect("attachment json"),
            session_id: parent.clone(),
            parent_branch_id: None,
            worker_generation: store.worker_generation(),
            attachment: attachment.clone(),
            device_id: DeviceId::new("m2e-test"),
        })
        .expect("attach exact attempt");
    assert!(matches!(
        attached,
        ChildGraphAttachOutcome::Committed { .. }
    ));
    let error = store
        .attach_child_graph(&ChildGraphAttachCommand {
            command_id: "m2e-duplicate-logical-attach".into(),
            request_digest: "m2e-duplicate-logical-attach-digest".into(),
            request_json: serde_json::to_string(&attachment).expect("attachment json"),
            session_id: parent.clone(),
            parent_branch_id: None,
            worker_generation: store.worker_generation(),
            attachment: attachment.clone(),
            device_id: DeviceId::new("m2e-test"),
        })
        .expect_err("another command cannot duplicate one logical attachment");
    assert_eq!(
        error
            .details
            .as_ref()
            .and_then(|details| details.get("kind"))
            .and_then(serde_json::Value::as_str),
        Some("duplicate_child_attachment")
    );
    let mut colliding_call = attachment.clone();
    colliding_call.parent_run_id = RunId::new("m2e-other-parent-run");
    colliding_call.parent_call_id = "m2e-other-parent-call".into();
    colliding_call.parent_tool_item_id = ItemId::new("m2e-other-parent-tool");
    let error = store
        .attach_child_graph(&ChildGraphAttachCommand {
            command_id: "m2e-colliding-slot-attach".into(),
            request_digest: "m2e-colliding-slot-attach-digest".into(),
            request_json: serde_json::to_string(&colliding_call).expect("attachment json"),
            session_id: parent.clone(),
            parent_branch_id: None,
            worker_generation: store.worker_generation(),
            attachment: colliding_call,
            device_id: DeviceId::new("m2e-test"),
        })
        .expect_err("a distinct call cannot attach another child to the owned parent slot");
    assert_eq!(
        error
            .details
            .as_ref()
            .and_then(|details| details.get("kind"))
            .and_then(serde_json::Value::as_str),
        Some("colliding_child_attachment")
    );
    let error = store
        .record_graph_evidence(&GraphEvidenceCommand {
            command_id: "m2e-steal-reserved-slot".into(),
            request_digest: "m2e-steal-reserved-slot-digest".into(),
            request_json: r#"{"slot":"tests"}"#.into(),
            session_id: parent.clone(),
            worker_generation: store.worker_generation(),
            run_id: record.parent_run_id.clone(),
            call_id: record.call_id.clone(),
            graph_id: parent_graph.clone(),
            node: verify_node(),
            verdict: EvidenceVerdict::Green,
            detail: "competing ordinary evidence".into(),
            slot: Some("tests".into()),
            subject_digest: Some("competing-subject".into()),
            signal: None,
            child_contract: None,
            device_id: DeviceId::new("m2e-test"),
        })
        .expect_err("ordinary evidence cannot steal an attached child slot");
    assert_eq!(
        error
            .details
            .as_ref()
            .and_then(|details| details.get("kind"))
            .and_then(serde_json::Value::as_str),
        Some("child_slot_reserved")
    );

    store
        .abandon_graph(&GraphAbandonCommand {
            command_id: "m2e-child-abandon".into(),
            request_digest: "m2e-child-abandon-digest".into(),
            request_json: r#"{"why":"test red contract"}"#.into(),
            session_id: child.clone(),
            worker_generation: store.worker_generation(),
            why: "test red contract".into(),
            device_id: DeviceId::new("m2e-test"),
        })
        .expect("abandon child graph");
    let revision = WorkspaceRevision::new("revision-child-17");
    let report = ChildReport {
        agent: record.agent_id.clone(),
        summary: "child found a blocker".into(),
        verified: ReportVerification::Red,
        workspace_revision: Some(revision.clone()),
    };
    store
        .record_delegation_report(&record.agent_id, &report)
        .expect("record child report");
    let report_digest = blake3::hash(&serde_json::to_vec(&report).expect("report json"))
        .to_hex()
        .to_string();
    let contract = ChildContractRef {
        child_session_id: child,
        child_run_id: record.child_run_id,
        child_graph_id: child_graph,
        report_digest: report_digest.clone(),
        workspace_revision: Some(revision.clone()),
    };
    let collapse = GraphEvidenceCommand {
        command_id: "m2e-collapse".into(),
        request_digest: "m2e-collapse-digest".into(),
        request_json: r#"{"child":"m2e-child"}"#.into(),
        session_id: parent.clone(),
        worker_generation: store.worker_generation(),
        run_id: record.parent_run_id,
        call_id: record.call_id,
        graph_id: parent_graph,
        node: verify_node(),
        verdict: EvidenceVerdict::Red,
        detail: "child workflow abandoned with blocker".into(),
        slot: Some("tests".into()),
        subject_digest: Some(child_contract_subject_digest(&contract)),
        signal: None,
        child_contract: Some(contract),
        device_id: DeviceId::new("m2e-test"),
    };
    assert!(matches!(
        store
            .record_graph_evidence(&collapse)
            .expect("first collapse"),
        GraphEvidenceOutcome::Committed { .. }
    ));
    assert!(matches!(
        store
            .record_graph_evidence(&collapse)
            .expect("collapse replay"),
        GraphEvidenceOutcome::IdempotentReplay { .. }
    ));

    let events = store.read(&parent, 0, 1024).expect("read parent journal");
    let collapsed = events
        .into_iter()
        .filter_map(|envelope| serde_json::from_value::<EventPayload>(envelope.payload).ok())
        .filter_map(|payload| match payload {
            EventPayload::EvidenceRecorded(evidence)
                if matches!(evidence.source, GraphEvidenceSource::ChildContract { .. }) =>
            {
                Some(evidence)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        collapsed.len(),
        1,
        "terminal child collapses to exactly one item"
    );
    assert!(matches!(
        &collapsed[0].source,
        GraphEvidenceSource::ChildContract {
            report_digest: found,
            workspace_revision: Some(found_revision),
            ..
        } if found == &report_digest && found_revision == &revision
    ));
}
