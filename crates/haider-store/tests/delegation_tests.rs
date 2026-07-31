#![allow(clippy::expect_used)]

use haider_protocol::agent::{
    AgentManifest, AgentRole, ChildReport, Grant, Placement, ReportVerification,
};
use haider_protocol::ids::{AgentId, DeviceId, EventId, ItemId, LeaseId, RunId, SessionId};
use haider_store::{
    DelegationCreateOutcome, DelegationRecord, DelegationState, SessionCreateCommand, Store,
};

fn create_session(store: &Store, session_id: &SessionId) {
    store
        .create_session(&SessionCreateCommand {
            command_id: format!("create-{session_id}"),
            request_digest: format!("digest-{session_id}"),
            request_json: format!(r#"{{"session":"{session_id}"}}"#),
            session_id: session_id.clone(),
            cwd: std::env::current_dir()
                .expect("cwd")
                .to_string_lossy()
                .into_owned(),
            provider: "fake".into(),
            model: "fake-model".into(),
            max_tokens: 4096,
            system_prompt_version: "test-v1".into(),
            event_id: EventId::new(format!("created-{session_id}")),
            device_id: DeviceId::new("test-device"),
        })
        .expect("create session");
}

fn record(parent: &SessionId, child: &SessionId) -> DelegationRecord {
    let agent = AgentId::new("agent-stable");
    DelegationRecord {
        agent_id: agent.clone(),
        child_session_id: child.clone(),
        child_run_id: RunId::new("child-run"),
        parent_session_id: parent.clone(),
        parent_run_id: RunId::new("parent-run"),
        call_id: "call-stable".into(),
        tool_item_id: ItemId::new("item-stable"),
        parent_agent_id: None,
        root_session_id: parent.clone(),
        depth: 1,
        task: "tests".into(),
        prompt: "run the tests".into(),
        manifest: AgentManifest {
            agent,
            role: AgentRole::Subagent,
            task: "tests".into(),
            callsign: Some("SUB-TEST".into()),
            model_profile: "fake-model".into(),
            grant: Grant {
                tools: vec!["fs_read".into()],
                effect_ceiling: Vec::new(),
            },
            budget_tokens: Some(4096),
            placement: Placement::Local,
            lease: LeaseId::new("lease-stable"),
            fencing_epoch: 1,
            attempt: 0,
            parent: None,
            coordinates: None,
        },
        state: DelegationState::Spawned,
        report: None,
    }
}

/// MUTATION CHECK: remove the durable agent/parent-call replay lookup before
/// insertion. Expected runtime failure: the second create either inserts a
/// duplicate child relation or returns a uniqueness error instead of the
/// original delegation.
#[test]
fn replayed_spawn_returns_the_original_child_relation() {
    let root = tempfile::tempdir().expect("temp profile");
    let store = Store::open(root.path()).expect("open store");
    let parent = SessionId::new("parent-session");
    let child = SessionId::new("child-session");
    create_session(&store, &parent);
    create_session(&store, &child);
    let requested = record(&parent, &child);

    let first = store
        .create_delegation(&requested)
        .expect("first delegation");
    assert!(matches!(first, DelegationCreateOutcome::Committed(_)));
    let replay = store
        .create_delegation(&requested)
        .expect("idempotent replay");
    let DelegationCreateOutcome::IdempotentReplay(replayed) = replay else {
        panic!("same spawn coordinates must replay");
    };
    assert_eq!(replayed.child_session_id, child);
    assert_eq!(
        store
            .delegations_for_parent_run(&parent, &RunId::new("parent-run"))
            .expect("list parent delegations")
            .len(),
        1
    );
}

/// MUTATION CHECK: overwrite an existing report or collect before a report.
/// Expected runtime failure: durable terminal truth changes, or collection
/// succeeds without a report that can settle the parent tool call.
#[test]
fn report_slot_is_exact_once_and_collection_requires_it() {
    let root = tempfile::tempdir().expect("temp profile");
    let store = Store::open(root.path()).expect("open store");
    let parent = SessionId::new("parent-session");
    let child = SessionId::new("child-session");
    create_session(&store, &parent);
    create_session(&store, &child);
    let requested = record(&parent, &child);
    store
        .create_delegation(&requested)
        .expect("create delegation");
    assert!(
        store
            .mark_delegation_collected(&requested.agent_id)
            .is_err(),
        "collection requires a terminal report"
    );
    let report = ChildReport {
        agent: requested.agent_id.clone(),
        summary: "tests passed".into(),
        verified: ReportVerification::Unverified,
        workspace_revision: None,
    };
    store
        .record_delegation_report(&requested.agent_id, &report)
        .expect("record report");
    store
        .record_delegation_report(&requested.agent_id, &report)
        .expect("same report replays");
    let mut changed = report.clone();
    changed.summary = "different".into();
    assert!(
        store
            .record_delegation_report(&requested.agent_id, &changed)
            .is_err(),
        "terminal report cannot change"
    );
    let collected = store
        .mark_delegation_collected(&requested.agent_id)
        .expect("collect after report");
    assert_eq!(collected.state, DelegationState::Collected);
    assert_eq!(collected.report, Some(report));
}
