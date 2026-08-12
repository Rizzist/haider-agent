#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::agent::{
    AgentManifest, AgentRole, ChildReport, Grant, Placement, ReportVerification,
};
use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
use haider_protocol::error::{ErrorAction, ErrorCode};
use haider_protocol::ids::{
    AgentId, BranchId, DeviceId, EventId, ItemId, LeaseId, RunId, SessionId,
};
use haider_protocol::state::{RunState, WaitReason};
use haider_store::{
    DelegationCreateOutcome, DelegationRecord, DelegationState, EventStore, SUBAGENT_LIVE_LIMIT,
    SessionCreateCommand, Store,
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
            permission_overrides: None,
            effort: None,
            fast: false,
            cache_policy: Default::default(),
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
        parent_branch_id: None,
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

fn named_record(
    parent: &SessionId,
    child: &SessionId,
    suffix: &str,
    parent_agent_id: Option<AgentId>,
    depth: u32,
) -> DelegationRecord {
    let mut record = record(parent, child);
    record.agent_id = AgentId::new(format!("agent-{suffix}"));
    record.child_run_id = RunId::new(format!("child-run-{suffix}"));
    record.parent_run_id = RunId::new(format!("parent-run-{suffix}"));
    record.call_id = format!("call-{suffix}");
    record.tool_item_id = ItemId::new(format!("item-{suffix}"));
    record.parent_agent_id = parent_agent_id.clone();
    record.depth = depth;
    record.manifest.agent = record.agent_id.clone();
    record.manifest.lease = LeaseId::new(format!("lease-{suffix}"));
    record.manifest.parent = parent_agent_id;
    record
}

fn append_run_state(store: &Store, record: &DelegationRecord, state: RunState, suffix: &str) {
    let mut events = [EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(format!("run-state-{suffix}")),
        seq: 0,
        session_id: record.child_session_id.clone(),
        branch_id: None,
        run_id: Some(record.child_run_id.clone()),
        agent_id: Some(record.agent_id.clone()),
        device_id: DeviceId::new("test-device"),
        authority_epoch: 0,
        worker_generation: store.worker_generation(),
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: serde_json::to_value(EventPayload::RunState(state)).expect("run state JSON"),
    }];
    store.append(&mut events).expect("append child run state");
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

/// E7 local-only law: deleting the durable admission check lets a stale or
/// future remote manifest become runnable even though production has no
/// remote-lane state machine.
#[test]
fn device_placement_is_rejected_by_durable_admission_with_typed_local_only_error() {
    let root = tempfile::tempdir().expect("temp profile");
    let store = Store::open(root.path()).expect("open store");
    let parent = SessionId::new("parent-local-only");
    let child = SessionId::new("child-local-only");
    create_session(&store, &parent);
    create_session(&store, &child);
    let mut requested = record(&parent, &child);
    requested.manifest.placement = Placement::Device {
        device: DeviceId::new("remote-device"),
    };

    let error = store
        .create_delegation(&requested)
        .expect_err("remote placement must be rejected");
    assert_eq!(error.message, "not supported — Haider runs local-only");
    assert_eq!(
        error
            .presentation
            .as_ref()
            .map(|presentation| presentation.subcode.as_str()),
        Some("local-only")
    );
    assert!(
        store
            .delegations_for_parent_run(&parent, &RunId::new("parent-run"))
            .expect("read delegations")
            .is_empty(),
        "rejection commits no remote scaffold"
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

/// MUTATION CHECK: remove the serde default from the additive parent branch
/// coordinate. Expected RUNTIME failure: a pre-B2a durable delegation receipt
/// can no longer be decoded after migration.
#[test]
fn legacy_delegation_receipt_defaults_parent_branch_to_main() {
    let parent = SessionId::new("legacy-parent-branch");
    let child = SessionId::new("legacy-child-main");
    let record = record(&parent, &child);
    let mut json = serde_json::to_value(record).expect("serialize delegation");
    json.as_object_mut()
        .expect("delegation object")
        .remove("parent_branch_id");

    let decoded: DelegationRecord =
        serde_json::from_value(json).expect("decode pre-B2a delegation receipt");
    assert_eq!(decoded.parent_branch_id, None);
}

/// MUTATION CHECK: omit the parent branch from the stored delegation JSON or
/// from its replay query. Expected RUNTIME failure: restart loses the spawn
/// branch and a late parent projection is retargeted to main.
#[test]
fn delegation_parent_branch_survives_durable_replay() {
    let root = tempfile::tempdir().expect("temp profile");
    let parent = SessionId::new("branch-parent-session");
    let child = SessionId::new("branch-child-session");
    let agent;
    {
        let store = Store::open(root.path()).expect("open store");
        create_session(&store, &parent);
        create_session(&store, &child);
        let mut requested = record(&parent, &child);
        requested.parent_branch_id = Some(BranchId::new("parent-branch-a"));
        agent = requested.agent_id.clone();
        store
            .create_delegation(&requested)
            .expect("create branch-pinned delegation");
    }

    let reopened = Store::open(root.path()).expect("reopen store");
    let replayed = reopened
        .delegation(&agent)
        .expect("delegation lookup")
        .expect("durable delegation");
    assert_eq!(
        replayed.parent_branch_id,
        Some(BranchId::new("parent-branch-a"))
    );
    assert_eq!(
        reopened
            .delegations_for_parent_run(&parent, &RunId::new("parent-run"))
            .expect("parent delegation replay"),
        vec![replayed]
    );
}

/// HARD-CAP LAW: the 512th globally live delegation commits, an idempotent
/// replay still wins at capacity, and the 513th receives the owner-pinned
/// typed tool presentation without inserting another relation.
#[test]
fn global_live_cap_admits_512_and_rejects_513_typed() {
    let root = tempfile::tempdir().expect("temp profile");
    let store = Store::open(root.path()).expect("open store");
    let parent = SessionId::new("cap-parent");
    create_session(&store, &parent);
    let mut last = None;
    for index in 0..SUBAGENT_LIVE_LIMIT {
        let child = SessionId::new(format!("cap-child-{index:03}"));
        create_session(&store, &child);
        let depth = u32::try_from(index % 3).expect("bounded depth") + 1;
        let parent_agent =
            (depth > 1).then(|| AgentId::new(format!("cap-parent-agent-{index:03}")));
        let requested = named_record(
            &parent,
            &child,
            &format!("cap-{index:03}"),
            parent_agent,
            depth,
        );
        let outcome = store
            .create_delegation(&requested)
            .expect("delegation within hard cap");
        assert!(matches!(outcome, DelegationCreateOutcome::Committed(_)));
        last = Some(requested);
    }
    assert_eq!(store.live_delegation_count().expect("live count"), 512);

    let replay = store
        .create_delegation(last.as_ref().expect("last admitted record"))
        .expect("replay bypasses admission count");
    assert!(matches!(
        replay,
        DelegationCreateOutcome::IdempotentReplay(_)
    ));

    let overflow_child = SessionId::new("cap-child-overflow");
    create_session(&store, &overflow_child);
    let overflow = named_record(&parent, &overflow_child, "cap-overflow", None, 1);
    let error = store
        .create_delegation(&overflow)
        .expect_err("513th live delegation must be rejected");
    assert_eq!(error.code, ErrorCode::Busy);
    assert!(error.retryable);
    assert_eq!(
        error
            .details
            .as_ref()
            .and_then(|value| value["limit"].as_u64()),
        Some(512)
    );
    assert_eq!(
        error
            .details
            .as_ref()
            .and_then(|value| value["live_count"].as_u64()),
        Some(512)
    );
    let presentation = error.presentation.expect("typed cap presentation");
    assert_eq!(presentation.subcode.as_str(), "subagent-limit-reached");
    assert_eq!(presentation.title, "Subagent limit reached");
    assert!(presentation.detail.contains("512"));
    assert_eq!(presentation.allowed_actions, vec![ErrorAction::Retry]);
    assert!(
        store
            .delegation(&overflow.agent_id)
            .expect("overflow lookup")
            .is_none()
    );
}

/// RESTART LAW: admission liveness is re-derived from durable exact-run
/// truth. Missing and parked child heads remain live; every terminal child
/// state is excluded after reopening the profile.
#[test]
fn live_count_is_rederived_after_restart_and_excludes_terminal_children() {
    let root = tempfile::tempdir().expect("temp profile");
    let parent = SessionId::new("restart-parent");
    {
        let store = Store::open(root.path()).expect("open store");
        create_session(&store, &parent);
        for (suffix, state) in [
            ("missing", None),
            (
                "parked",
                Some(RunState::Waiting {
                    reason: WaitReason::LocalChild,
                }),
            ),
            ("done", Some(RunState::Done)),
            ("failed", Some(RunState::Errored)),
            ("cancelled", Some(RunState::Cancelled)),
        ] {
            let child = SessionId::new(format!("restart-child-{suffix}"));
            create_session(&store, &child);
            let requested = named_record(&parent, &child, suffix, None, 1);
            store
                .create_delegation(&requested)
                .expect("seed delegation");
            if let Some(state) = state {
                append_run_state(&store, &requested, state, suffix);
            }
        }
        assert_eq!(store.live_delegation_count().expect("pre-restart count"), 2);
    }

    let reopened = Store::open(root.path()).expect("reopen store");
    assert_eq!(
        reopened
            .live_delegation_count()
            .expect("post-restart durable count"),
        2
    );
}

/// DESCENDANT LAW: session-wide traversal follows child sessions across
/// parent runs in deterministic breadth-first order, supports subtree roots,
/// and marks both node and depth truncation with a durable edge witness.
#[test]
fn bounded_descendant_reduction_preserves_tree_order_and_truncation() {
    let root = tempfile::tempdir().expect("temp profile");
    let store = Store::open(root.path()).expect("open store");
    let head = SessionId::new("tree-head");
    let child_a = SessionId::new("tree-child-a");
    let child_b = SessionId::new("tree-child-b");
    let grandchild = SessionId::new("tree-grandchild");
    for session in [&head, &child_a, &child_b, &grandchild] {
        create_session(&store, session);
    }
    let a = named_record(&head, &child_a, "tree-a", None, 1);
    let b = named_record(&head, &child_b, "tree-b", None, 1);
    let nested = named_record(
        &child_a,
        &grandchild,
        "tree-nested",
        Some(a.agent_id.clone()),
        2,
    );
    for record in [&a, &b, &nested] {
        store.create_delegation(record).expect("tree delegation");
    }

    let full = store
        .delegation_descendants(&head, 512, 32)
        .expect("full descendants");
    assert!(!full.truncated);
    assert_eq!(
        full.descendants
            .iter()
            .map(|row| (row.record.agent_id.as_str(), row.relative_depth))
            .collect::<Vec<_>>(),
        vec![
            ("agent-tree-a", 1),
            ("agent-tree-b", 1),
            ("agent-tree-nested", 2),
        ]
    );
    assert_eq!(
        full.descendants
            .iter()
            .map(|row| (row.record.agent_id.as_str(), row.direct_child_count))
            .collect::<Vec<_>>(),
        vec![
            ("agent-tree-a", 1),
            ("agent-tree-b", 0),
            ("agent-tree-nested", 0),
        ],
        "each returned row retains its exact durable direct-child count"
    );
    let subtree = store
        .delegation_descendants(&child_a, 512, 32)
        .expect("nested subtree");
    assert_eq!(subtree.descendants.len(), 1);
    assert_eq!(subtree.descendants[0].relative_depth, 1);
    assert_eq!(subtree.descendants[0].record.agent_id, nested.agent_id);

    let node_bounded = store
        .delegation_descendants(&head, 2, 32)
        .expect("node-bounded descendants");
    assert_eq!(node_bounded.descendants.len(), 2);
    assert!(node_bounded.truncated);
    let depth_bounded = store
        .delegation_descendants(&head, 512, 1)
        .expect("depth-bounded descendants");
    assert_eq!(depth_bounded.descendants.len(), 2);
    assert!(depth_bounded.truncated);
    assert_eq!(depth_bounded.descendants[0].direct_child_count, 1);
    assert_eq!(depth_bounded.descendants[1].direct_child_count, 0);
}
