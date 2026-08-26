//! B1 — the durable Loom registry: agent types + pipe-source workflows.
//! The store is the compiler authority (callers send SOURCE, the registry
//! compiles inside the transaction) and owns the rev law: new id → rev 1,
//! identical content → idempotent no-op, changed content → rev + 1.
#![allow(clippy::expect_used)]

use haider_protocol::ids::{DeviceId, EventId, GraphId, SessionId};
use haider_protocol::loom::LoomAgentType;
use haider_protocol::typed_agent::{TYPED_AGENT_INSTALL_STATUS_MAX_JOBS, TypedAgentInstallState};
use haider_store::{
    ErrorCode, GraphPinCommand, GraphPinOutcome, GraphSwitchCommand, SessionCreateCommand, Store,
    TypedAgentInstallCas, TypedAgentInstallItemCas,
};

fn agent_type(id: &str, in_type: &str, out_type: &str) -> LoomAgentType {
    LoomAgentType {
        id: id.into(),
        name: format!("{id}-name"),
        job: format!("You are the {id} specialist."),
        in_type: in_type.into(),
        out_type: out_type.into(),
        clis: vec!["yt-dlp".into()],
        apis: Vec::new(),
        skills: vec!["transcript-clean".into()],
        scripts: Vec::new(),
        color: "#c2701c".into(),
        glyph: "▲".into(),
        rev: 1,
    }
}

const PIPE: &str =
    "clip-flow: SourceURL -> Transcript\nresearch @researcher \"pull and transcribe\" :cmd";

/// MUTATION CHECK: let callers pick revs, stop no-opping identical content,
/// or skip the rev advance on changed content. Expected RUNTIME failure.
#[test]
fn agent_type_registration_owns_the_rev_law() {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("open");

    // New id lands at rev 1 even when the caller claims rev 9.
    let mut first = agent_type("researcher", "SourceURL", "Transcript");
    first.rev = 9;
    let created = store
        .loom_register_agent_type(&first)
        .expect("first registration");
    assert_eq!((created.rev, created.updated), (1, true));

    // Identical content is an idempotent no-op at the SAME rev.
    let noop = store
        .loom_register_agent_type(&first)
        .expect("noop registration");
    assert_eq!((noop.rev, noop.updated), (1, false));

    // Changed content advances by exactly one and persists the new record.
    let mut regranted = first.clone();
    regranted.apis.push("fal.ai".into());
    let revised = store
        .loom_register_agent_type(&regranted)
        .expect("revised registration");
    assert_eq!((revised.rev, revised.updated), (2, true));
    let listed = store.loom_agent_types().expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].rev, 2);
    assert_eq!(listed[0].apis, vec!["fal.ai".to_string()]);

    // Bounds reject with typed errors.
    let mut bad = agent_type("bad id!", "A", "B");
    bad.id = "bad id!".into();
    assert!(store.loom_register_agent_type(&bad).is_err());
}

#[test]
fn typed_agent_registration_atomically_enqueues_frozen_install_work() {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("open");
    let mut first = agent_type("researcher", "SourceURL", "Transcript");
    first.rev = 99;

    let created = store
        .loom_register_agent_type_with_install(&first)
        .expect("typed registration");
    let job = created.install_job.expect("required CLI creates a job");
    assert_eq!(
        (created.registration.rev, created.registration.updated),
        (1, true)
    );
    assert_eq!(job.agent_type_id, "researcher");
    assert_eq!(job.agent_type_rev, 1);
    assert_eq!(job.agent_type_digest, created.registration.digest);
    assert_eq!(
        job.job_id,
        format!("install:researcher:1:{}", created.registration.digest),
        "job identity is deterministic and bound to the frozen revision"
    );
    let items = store
        .typed_agent_install_items(Some(&job.job_id), Some("researcher"))
        .expect("job items");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].required_cli.program, "yt-dlp");
    assert_eq!(items[0].state, TypedAgentInstallState::Queued);
    let snapshot = store
        .typed_agent_install_status(Some(&job.job_id), Some("researcher"))
        .expect("coherent install status");
    assert_eq!(snapshot.jobs, vec![job.clone()]);
    assert_eq!(snapshot.items, items);
    assert!(
        store
            .typed_agent_install_items(Some(&job.job_id), Some("other-type"))
            .expect("combined item filters")
            .is_empty(),
        "item job and type filters are conjunctive"
    );

    let noop = store
        .loom_register_agent_type_with_install(&first)
        .expect("idempotent typed registration");
    assert!(!noop.registration.updated);
    assert!(noop.install_job.is_none());
    assert_eq!(
        store
            .typed_agent_install_jobs(None, Some("researcher"))
            .expect("researcher jobs")
            .len(),
        1,
        "same content must not enqueue duplicate work"
    );

    // Upgrade seam: a pre-feature registry row has no job. Simulate that
    // state and prove startup's idempotent re-registration backfills work
    // without changing the agent-type revision.
    let raw = rusqlite::Connection::open(store.database_path()).expect("raw registry database");
    raw.pragma_update(None, "foreign_keys", true)
        .expect("foreign keys");
    raw.execute(
        "DELETE FROM loom_cli_install_jobs WHERE job_id = ?1",
        [job.job_id.as_str()],
    )
    .expect("remove pre-feature missing job fixture");
    drop(raw);
    let backfilled = store
        .loom_register_agent_type_with_install(&first)
        .expect("backfill missing install work");
    assert_eq!(backfilled.registration.rev, 1);
    assert!(!backfilled.registration.updated);
    assert_eq!(
        backfilled
            .install_job
            .as_ref()
            .map(|created| created.job_id.as_str()),
        Some(job.job_id.as_str())
    );

    let mut revised = first.clone();
    revised.clis.push("jq".into());
    let revised = store
        .loom_register_agent_type_with_install(&revised)
        .expect("changed typed registration");
    let revised_job = revised.install_job.expect("changed type creates a job");
    assert_eq!(revised.registration.rev, 2);
    assert_eq!(revised_job.agent_type_rev, 2);
    assert_eq!(revised_job.progress.total, 2);
    assert_eq!(
        store
            .typed_agent_install_jobs(None, Some("researcher"))
            .expect("all researcher jobs")
            .len(),
        2
    );
    assert!(
        store
            .typed_agent_install_jobs(Some(&revised_job.job_id), Some("other-type"))
            .expect("combined filters")
            .is_empty(),
        "job and type filters are conjunctive"
    );

    let mut no_cli = agent_type("writer", "Notes", "Draft");
    no_cli.clis.clear();
    let no_cli = store
        .loom_register_agent_type_with_install(&no_cli)
        .expect("CLI-free typed registration");
    assert!(no_cli.install_job.is_none());
    assert!(
        store
            .typed_agent_install_jobs(None, Some("writer"))
            .expect("writer jobs")
            .is_empty()
    );

    let mut unsafe_cli = agent_type("unsafe", "A", "B");
    unsafe_cli.clis = vec!["relative/tool".into()];
    let error = store
        .loom_register_agent_type_with_install(&unsafe_cli)
        .expect_err("required CLI contract rejects relative programs");
    assert_eq!(error.code, ErrorCode::InvalidArgument);
    assert!(
        store
            .loom_agent_type("unsafe")
            .expect("unsafe type lookup")
            .is_none(),
        "contract rejection rolls back the registry row too"
    );
    assert!(
        store
            .typed_agent_install_jobs(None, Some("unsafe"))
            .expect("unsafe type jobs")
            .is_empty(),
        "contract rejection leaves no partial install job"
    );
}

#[test]
fn typed_agent_status_bounds_history_before_loading_items() {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("open");
    let mut record = agent_type("bounded-installer", "Input", "Output");
    for revision in 1..=40 {
        record.job = format!("Scoped installer contract revision {revision}.");
        store
            .loom_register_agent_type_with_install(&record)
            .expect("register revision");
    }

    let snapshot = store
        .typed_agent_install_status(None, Some("bounded-installer"))
        .expect("bounded reconnect status");
    assert_eq!(snapshot.jobs.len(), TYPED_AGENT_INSTALL_STATUS_MAX_JOBS);
    assert_eq!(snapshot.items.len(), snapshot.jobs.len());
    assert!(snapshot.jobs.iter().any(|job| job.agent_type_rev == 40));
    assert!(snapshot.jobs.iter().all(|job| job.agent_type_rev >= 9));
}

#[test]
fn typed_agent_install_lifecycle_is_validated_cas_and_restart_visible() {
    let root = tempfile::tempdir().expect("profile");
    let (terminal_job, terminal_item) = {
        let store = Store::open(root.path()).expect("open");
        let registration = store
            .loom_register_agent_type_with_install(&agent_type(
                "installer",
                "SourceURL",
                "Transcript",
            ))
            .expect("typed registration");
        let queued_job = registration.install_job.expect("queued job");
        let queued_item = store
            .typed_agent_install_items(Some(&queued_job.job_id), None)
            .expect("queued items")
            .into_iter()
            .next()
            .expect("queued item");

        let mut installing_job = queued_job.clone();
        installing_job.state = TypedAgentInstallState::Installing;
        installing_job.progress.current_cli = Some(queued_item.required_cli.program.clone());
        installing_job.updated_at_ms += 1;
        let mut installing_item = queued_item.clone();
        installing_item.state = TypedAgentInstallState::Installing;
        installing_item.updated_at_ms += 1;
        store
            .typed_agent_install_compare_and_swap(&TypedAgentInstallCas {
                expected_job: queued_job.clone(),
                next_job: installing_job.clone(),
                item: Some(TypedAgentInstallItemCas {
                    expected: queued_item.clone(),
                    next: installing_item.clone(),
                }),
            })
            .expect("start install");

        let stale = store
            .typed_agent_install_compare_and_swap(&TypedAgentInstallCas {
                expected_job: queued_job,
                next_job: installing_job.clone(),
                item: None,
            })
            .expect_err("stale job snapshot is rejected");
        assert_eq!(stale.code, ErrorCode::RevisionConflict);

        let mut illegal_job = installing_job.clone();
        illegal_job.state = TypedAgentInstallState::Succeeded;
        illegal_job.progress.completed = illegal_job.progress.total;
        illegal_job.progress.current_cli = None;
        illegal_job.updated_at_ms += 1;
        let illegal = store
            .typed_agent_install_compare_and_swap(&TypedAgentInstallCas {
                expected_job: installing_job.clone(),
                next_job: illegal_job,
                item: None,
            })
            .expect_err("installing cannot skip verification");
        assert_eq!(illegal.code, ErrorCode::InvalidArgument);

        let mut item_verifying_job = installing_job.clone();
        item_verifying_job.updated_at_ms += 1;
        let mut verifying_item = installing_item.clone();
        verifying_item.state = TypedAgentInstallState::Verifying;
        verifying_item.updated_at_ms += 1;
        store
            .typed_agent_install_compare_and_swap(&TypedAgentInstallCas {
                expected_job: installing_job,
                next_job: item_verifying_job.clone(),
                item: Some(TypedAgentInstallItemCas {
                    expected: installing_item,
                    next: verifying_item.clone(),
                }),
            })
            .expect("verify installed CLI");

        let mut aggregate_lie = item_verifying_job.clone();
        aggregate_lie.state = TypedAgentInstallState::Verifying;
        aggregate_lie.progress.completed = aggregate_lie.progress.total;
        aggregate_lie.progress.current_cli = None;
        aggregate_lie.updated_at_ms += 1;
        let invalid_aggregate = store
            .typed_agent_install_compare_and_swap(&TypedAgentInstallCas {
                expected_job: item_verifying_job.clone(),
                next_job: aggregate_lie,
                item: None,
            })
            .expect_err("job cannot claim verifying while its item is incomplete");
        assert_eq!(invalid_aggregate.code, ErrorCode::InvalidArgument);

        let mut verifying_job = item_verifying_job.clone();
        verifying_job.state = TypedAgentInstallState::Verifying;
        verifying_job.progress.completed = verifying_job.progress.total;
        verifying_job.progress.current_cli = None;
        verifying_job.updated_at_ms += 1;
        let mut succeeded_item = verifying_item.clone();
        succeeded_item.state = TypedAgentInstallState::Succeeded;
        succeeded_item.updated_at_ms += 1;
        store
            .typed_agent_install_compare_and_swap(&TypedAgentInstallCas {
                expected_job: item_verifying_job,
                next_job: verifying_job.clone(),
                item: Some(TypedAgentInstallItemCas {
                    expected: verifying_item,
                    next: succeeded_item.clone(),
                }),
            })
            .expect("finish CLI verification");

        let mut succeeded_job = verifying_job.clone();
        succeeded_job.state = TypedAgentInstallState::Succeeded;
        succeeded_job.updated_at_ms += 1;
        store
            .typed_agent_install_compare_and_swap(&TypedAgentInstallCas {
                expected_job: verifying_job,
                next_job: succeeded_job.clone(),
                item: None,
            })
            .expect("finish install job");
        (succeeded_job, succeeded_item)
    };

    let reopened = Store::open(root.path()).expect("reopen");
    let stored_job = reopened
        .typed_agent_install_jobs(Some(&terminal_job.job_id), None)
        .expect("reopened job")
        .into_iter()
        .next()
        .expect("durable terminal job");
    let stored_item = reopened
        .typed_agent_install_items(Some(&terminal_job.job_id), None)
        .expect("reopened item")
        .into_iter()
        .next()
        .expect("durable terminal item");
    assert_eq!(stored_job, terminal_job);
    assert_eq!(stored_item, terminal_item);
    let snapshot = reopened
        .typed_agent_install_status(Some(&stored_job.job_id), Some("installer"))
        .expect("reopened coherent status");
    assert_eq!(snapshot.jobs, vec![stored_job]);
    assert_eq!(snapshot.items, vec![stored_item]);
}

/// MUTATION CHECK: compile outside the transaction's registry view, accept a
/// pipe with unresolved @types, or lose the compiled template. Expected
/// RUNTIME failure.
#[test]
fn workflow_registration_compiles_source_against_the_live_registry() {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("open");

    // Unresolved @researcher: the pipe rejects with the compile error list.
    let rejected = store.loom_register_workflow(PIPE);
    assert!(rejected.is_err());
    let message = rejected.expect_err("rejects").message;
    assert!(
        message.contains("unregistered agent type @researcher"),
        "{message}"
    );

    // Register the type, then the same source compiles and lands at rev 1.
    store
        .loom_register_agent_type(&agent_type("researcher", "SourceURL", "Transcript"))
        .expect("agent type");
    let created = store.loom_register_workflow(PIPE).expect("workflow");
    assert_eq!((created.rev, created.updated), (1, true));
    assert_eq!(created.id, "clip-flow");

    // Same source again: no-op.
    let noop = store.loom_register_workflow(PIPE).expect("noop");
    assert_eq!((noop.rev, noop.updated), (1, false));

    // The compiled record round-trips with template + meta + typed IO.
    let workflows = store.loom_workflows().expect("list");
    assert_eq!(workflows.len(), 1);
    let workflow = &workflows[0];
    assert_eq!(workflow.template.nodes.len(), 1);
    assert_eq!(workflow.template.nodes[0].name.as_str(), "RESEARCH");
    assert_eq!(workflow.meta[0].out_type.as_deref(), Some("Transcript"));
    assert_eq!(workflow.meta[0].agent_type_rev, Some(1));
    let researcher = store
        .loom_agent_type("researcher")
        .expect("researcher lookup")
        .expect("researcher record");
    let researcher_digest = researcher.digest();
    assert_eq!(
        workflow.meta[0].agent_type_digest.as_deref(),
        Some(researcher_digest.as_str())
    );

    // A registry change that alters the RESOLVED signature is a new content
    // digest: the same source re-registers as rev 2 (the digest binds the
    // resolved types, not just the text).
    let mut widened = agent_type("researcher", "SourceURL + PlaylistURL", "Transcript");
    widened.apis.push("elevenlabs".into());
    store
        .loom_register_agent_type(&widened)
        .expect("widened type");
    let recompiled = store.loom_register_workflow(PIPE).expect("recompiled");
    assert_eq!((recompiled.rev, recompiled.updated), (2, true));
}

/// The registry survives a store reopen (durability, not cache).
#[test]
fn registry_survives_reopen() {
    let root = tempfile::tempdir().expect("profile");
    {
        let store = Store::open(root.path()).expect("open");
        store
            .loom_register_agent_type(&agent_type("researcher", "SourceURL", "Transcript"))
            .expect("agent type");
        store.loom_register_workflow(PIPE).expect("workflow");
    }
    let reopened = Store::open(root.path()).expect("reopen");
    assert_eq!(reopened.loom_agent_types().expect("types").len(), 1);
    assert_eq!(reopened.loom_workflows().expect("workflows").len(), 1);
}

/// C1 MUTATION CHECK: drop the registry fallback from template resolution.
/// Expected RUNTIME failure: a registered pipe workflow stops being pinnable
/// BY NAME through the ordinary graph machinery.
#[test]
fn registered_workflow_is_pinnable_by_name() {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("open");
    store
        .loom_register_agent_type(&agent_type("researcher", "SourceURL", "Transcript"))
        .expect("agent type");
    store.loom_register_workflow(PIPE).expect("workflow");

    let session_id = SessionId::new("loom-pin-session");
    store
        .create_session(&SessionCreateCommand {
            command_id: "create-loom-pin".into(),
            request_digest: "create-loom-pin-digest".into(),
            request_json: r#"{"session":"loom-pin"}"#.into(),
            session_id: session_id.clone(),
            cwd: "/tmp".into(),
            provider: "fake".into(),
            model: "fake-v1".into(),
            max_tokens: 4096,
            permission_overrides: None,
            effort: None,
            fast: false,
            cache_policy: Default::default(),
            system_prompt_version: "loom-test-v1".into(),
            event_id: EventId::new("created-loom-pin"),
            device_id: DeviceId::new("loom-test"),
        })
        .expect("create typed session");

    // An unknown name still rejects...
    let bad = store.pin_graph(&GraphPinCommand {
        command_id: "pin-ghost".into(),
        request_digest: "pin-ghost-digest".into(),
        request_json: r#"{"template":"ghost-flow"}"#.into(),
        session_id: session_id.clone(),
        worker_generation: store.worker_generation(),
        graph_id: GraphId::new("graph-ghost"),
        template: "ghost-flow".into(),
        device_id: DeviceId::new("loom-test"),
    });
    assert!(bad.is_err());

    // ...but the REGISTERED pipe workflow pins exactly like a catalog entry.
    let outcome = store
        .pin_graph(&GraphPinCommand {
            command_id: "pin-clip-flow".into(),
            request_digest: "pin-clip-flow-digest".into(),
            request_json: r#"{"template":"clip-flow"}"#.into(),
            session_id: session_id.clone(),
            worker_generation: store.worker_generation(),
            graph_id: GraphId::new("graph-clip-flow"),
            template: "clip-flow".into(),
            device_id: DeviceId::new("loom-test"),
        })
        .expect("pin registered workflow");
    let GraphPinOutcome::Committed { pinned, .. } = outcome else {
        panic!("fresh pin must commit");
    };
    assert_eq!(pinned.template, "clip-flow");
}

/// ITEM #3 MUTATION CHECK: overwrite the sole registry row without archiving
/// it, or make pinned execution resolve current-by-name. The rev-1 lookup by
/// the digest frozen in the pin disappears (the original strand bug).
#[test]
fn pinned_workflow_revision_survives_registry_edit_and_stale_fences_name_current() {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("open");
    store
        .loom_register_workflow("retained: A -> A\nstep \"one\" :cmd")
        .expect("register rev 1");
    let first = store
        .loom_workflow("retained")
        .expect("read rev 1")
        .expect("rev 1 exists");
    let first_template_digest = haider_protocol::graph::graph_template_digest(&first.template);

    let session_id = SessionId::new("retained-revision-session");
    store
        .create_session(&SessionCreateCommand {
            command_id: "create-retained-revision".into(),
            request_digest: "create-retained-revision-digest".into(),
            request_json: r#"{"session":"retained-revision"}"#.into(),
            session_id: session_id.clone(),
            cwd: "/tmp".into(),
            provider: "fake".into(),
            model: "fake-v1".into(),
            max_tokens: 4096,
            permission_overrides: None,
            effort: None,
            fast: false,
            cache_policy: Default::default(),
            system_prompt_version: "loom-test-v1".into(),
            event_id: EventId::new("created-retained-revision"),
            device_id: DeviceId::new("loom-test"),
        })
        .expect("create session");
    let pin = store
        .pin_graph_matching_digest(
            &GraphPinCommand {
                command_id: "pin-retained-rev-1".into(),
                request_digest: "pin-retained-rev-1-digest".into(),
                request_json: r#"{"template":"retained","expected":"rev-1"}"#.into(),
                session_id: session_id.clone(),
                worker_generation: store.worker_generation(),
                graph_id: GraphId::new("graph-retained-rev-1"),
                template: "retained".into(),
                device_id: DeviceId::new("loom-test"),
            },
            &first_template_digest,
        )
        .expect("pin rev 1");
    let pinned = match pin {
        GraphPinOutcome::Committed { pinned, .. }
        | GraphPinOutcome::IdempotentReplay { pinned } => pinned,
    };

    let revised = store
        .loom_register_workflow("retained: A -> A\nstep \"two\" :cmd")
        .expect("register rev 2");
    assert_eq!(revised.rev, 2);
    let current = store
        .loom_workflow("retained")
        .expect("read current")
        .expect("current exists");
    let current_template_digest = haider_protocol::graph::graph_template_digest(&current.template);

    drop(store);
    let store = Store::open(root.path()).expect("reopen with retained revisions");

    let retained = store
        .loom_workflow_revision("retained", &pinned.digest)
        .expect("read pinned revision")
        .expect("pinned revision remains executable");
    assert_eq!(retained.rev, 1);
    assert_eq!(retained.source, "retained: A -> A\nstep \"one\" :cmd");
    assert_ne!(pinned.digest, current_template_digest);

    let stale_pin = store
        .pin_graph_matching_digest(
            &GraphPinCommand {
                command_id: "pin-stale-retained".into(),
                request_digest: "pin-stale-retained-digest".into(),
                request_json: r#"{"template":"retained","expected":"stale"}"#.into(),
                session_id: session_id.clone(),
                worker_generation: store.worker_generation(),
                graph_id: GraphId::new("graph-stale-retained"),
                template: "retained".into(),
                device_id: DeviceId::new("loom-test"),
            },
            &first_template_digest,
        )
        .expect_err("stale pin fence rejects");
    assert_eq!(stale_pin.code, ErrorCode::RevisionConflict);
    assert_eq!(
        stale_pin
            .details
            .as_ref()
            .and_then(|value| value["current_revision"].as_u64()),
        Some(2)
    );
    assert_eq!(
        stale_pin
            .details
            .as_ref()
            .and_then(|value| value["current_digest"].as_str()),
        Some(current_template_digest.as_str())
    );

    let stale_switch = store
        .switch_graph_matching_digest(
            &GraphSwitchCommand {
                command_id: "switch-stale-retained".into(),
                request_digest: "switch-stale-retained-digest".into(),
                request_json: r#"{"template":"retained","expected":"stale"}"#.into(),
                session_id,
                worker_generation: store.worker_generation(),
                old_graph_id: pinned.graph_id,
                new_graph_id: GraphId::new("graph-replacement-retained"),
                template: "retained".into(),
                template_spec: None,
                device_id: DeviceId::new("loom-test"),
            },
            &first_template_digest,
        )
        .expect_err("stale switch fence rejects");
    assert_eq!(stale_switch.code, ErrorCode::RevisionConflict);
    assert_eq!(
        stale_switch
            .details
            .as_ref()
            .and_then(|value| value["current_revision"].as_u64()),
        Some(2)
    );
    assert_eq!(
        stale_switch
            .details
            .as_ref()
            .and_then(|value| value["current_digest"].as_str()),
        Some(current_template_digest.as_str())
    );

    let reverted = store
        .loom_register_workflow("retained: A -> A\nstep \"one\" :cmd")
        .expect("register content reversion");
    assert_eq!(reverted.rev, 3);
    let retained_after_reversion = store
        .loom_workflow_revision("retained", &pinned.digest)
        .expect("read retained revision after reversion")
        .expect("rev 1 remains distinct from reverted rev 3");
    assert_eq!(retained_after_reversion.rev, 1);
    assert_eq!(
        retained_after_reversion.source,
        "retained: A -> A\nstep \"one\" :cmd"
    );
}

/// ITEM #3 upgrade mutation check: migration 21 backfills the single v20
/// current row as an immutable revision. A later pre-stamp heal must append a
/// new current instance instead of rewriting the digest already in a pin.
#[test]
fn migration_backfill_and_legacy_heal_preserve_the_pinned_digest() {
    let root = tempfile::tempdir().expect("profile");
    let (database_path, legacy_json) = {
        let store = Store::open(root.path()).expect("open");
        store
            .loom_register_workflow("legacy-retained: A -> A\nstep \"one\" :cmd")
            .expect("register rev 1");
        let registration = store
            .loom_register_workflow("legacy-retained: A -> A\nstep \"two\" :cmd")
            .expect("register rev 2");
        assert_eq!(registration.rev, 2);
        let mut legacy = store
            .loom_workflow("legacy-retained")
            .expect("read rev 2")
            .expect("rev 2 exists");
        legacy.template.version = 1;
        (
            store.database_path().to_path_buf(),
            serde_json::to_string(&legacy).expect("encode legacy workflow"),
        )
    };

    // Plant the exact shape a v20 database could contain: one current rev-2
    // row whose compiled template predates revision stamping, and no history
    // table. Reopening must run migration 21 and archive those exact bytes.
    let raw = rusqlite::Connection::open(&database_path).expect("open legacy database");
    raw.execute(
        "UPDATE loom_workflows SET record_json = ?2 WHERE id = ?1",
        rusqlite::params!["legacy-retained", legacy_json],
    )
    .expect("plant pre-stamp row");
    raw.execute_batch(
        "DROP TABLE loom_workflow_revisions;
         DELETE FROM schema_migrations WHERE version = 21;
         PRAGMA user_version = 20;",
    )
    .expect("rewind migration 21 fixture");
    drop(raw);

    let store = Store::open(root.path()).expect("migrate legacy database");
    assert_eq!(store.schema_version().expect("schema version"), 21);
    let legacy = store
        .loom_workflow("legacy-retained")
        .expect("read migrated current")
        .expect("migrated current exists");
    assert_eq!(legacy.rev, 2);
    assert_eq!(legacy.template.version, 1);
    let legacy_digest = haider_protocol::graph::graph_template_digest(&legacy.template);

    let session_id = SessionId::new("legacy-retained-session");
    store
        .create_session(&SessionCreateCommand {
            command_id: "create-legacy-retained".into(),
            request_digest: "create-legacy-retained-digest".into(),
            request_json: r#"{"session":"legacy-retained"}"#.into(),
            session_id: session_id.clone(),
            cwd: "/tmp".into(),
            provider: "fake".into(),
            model: "fake-v1".into(),
            max_tokens: 4096,
            permission_overrides: None,
            effort: None,
            fast: false,
            cache_policy: Default::default(),
            system_prompt_version: "loom-test-v1".into(),
            event_id: EventId::new("created-legacy-retained"),
            device_id: DeviceId::new("loom-test"),
        })
        .expect("create legacy pin session");
    let pinned = store
        .pin_graph_matching_digest(
            &GraphPinCommand {
                command_id: "pin-legacy-retained".into(),
                request_digest: "pin-legacy-retained-digest".into(),
                request_json: r#"{"template":"legacy-retained"}"#.into(),
                session_id,
                worker_generation: store.worker_generation(),
                graph_id: GraphId::new("graph-legacy-retained"),
                template: "legacy-retained".into(),
                device_id: DeviceId::new("loom-test"),
            },
            &legacy_digest,
        )
        .expect("pin migrated pre-stamp instance");
    let pinned = match pinned {
        GraphPinOutcome::Committed { pinned, .. }
        | GraphPinOutcome::IdempotentReplay { pinned } => pinned,
    };

    let healed = store
        .loom_register_workflow("legacy-retained: A -> A\nstep \"two\" :cmd")
        .expect("append healed current revision");
    assert_eq!((healed.rev, healed.updated), (3, true));
    let retained = store
        .loom_workflow_revision("legacy-retained", &pinned.digest)
        .expect("read migrated pinned revision")
        .expect("pre-heal pinned digest remains retained");
    assert_eq!(retained.rev, 2);
    assert_eq!(retained.template.version, 1);
    assert_eq!(
        haider_protocol::graph::graph_template_digest(&retained.template),
        pinned.digest
    );
    let current = store
        .loom_workflow("legacy-retained")
        .expect("read healed current")
        .expect("healed current exists");
    assert_eq!(current.rev, 3);
    assert_eq!(current.template.version, 3);
    assert_ne!(
        haider_protocol::graph::graph_template_digest(&current.template),
        pinned.digest
    );
}

/// Review round 2 MUTATION CHECK: stop stamping `template.version = rev`, or
/// drop color/glyph from the agent digest. Expected RUNTIME failures: a
/// content revision keeps the same template digest (the tail/TUI join key),
/// or an accent-only edit silently no-ops.
#[test]
fn revisions_move_the_template_digest_and_display_edits_are_real() {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("open");
    let registration = store
        .loom_register_workflow("wf: A -> A\nstep \"one\" :cmd")
        .expect("registers");
    assert_eq!(registration.rev, 1);
    let first = store
        .loom_workflow("wf")
        .expect("reads")
        .expect("workflow present");
    assert_eq!(first.template.version, 1, "rev 1 stamps version 1");
    let first_key = haider_protocol::graph::graph_template_digest(&first.template);

    // A task-only change: the template SHAPE is identical, but the rev bump
    // stamps a new version, so the join key must move.
    let second = store
        .loom_register_workflow("wf: A -> A\nstep \"two\" :cmd")
        .expect("re-registers");
    assert_eq!(second.rev, 2);
    let second_record = store
        .loom_workflow("wf")
        .expect("reads")
        .expect("workflow present");
    assert_eq!(second_record.template.version, 2);
    assert_ne!(
        haider_protocol::graph::graph_template_digest(&second_record.template),
        first_key,
        "a content revision must move the pinned-instance join key"
    );

    // Display-only agent edit = real revision.
    let mut record = agent_type("painter", "A", "A");
    assert!(
        store
            .loom_register_agent_type(&record)
            .expect("registers")
            .updated
    );
    record.color = "#00ff00".into();
    let repaint = store
        .loom_register_agent_type(&record)
        .expect("re-registers");
    assert!(repaint.updated, "a color edit must persist");
    assert_eq!(repaint.rev, 2);
}

/// Review round 2 MUTATION CHECK: drop any C5 bound (type-expr law, color
/// shape, glyph control chars). Expected RUNTIME failure: the bad record
/// registers.
#[test]
fn registration_bounds_types_color_and_glyph() {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("open");
    let mut record = agent_type("bounded", "A", "B");
    record.in_type = "x".repeat(65);
    assert!(store.loom_register_agent_type(&record).is_err(), "65B type");
    let mut record = agent_type("bounded", "A + B", "C");
    record.color = "red".into();
    assert!(
        store.loom_register_agent_type(&record).is_err(),
        "bad color"
    );
    record.color = "#12ab34".into();
    record.glyph = "\n".into();
    assert!(
        store.loom_register_agent_type(&record).is_err(),
        "ctrl glyph"
    );
    record.glyph = "▲".into();
    assert!(
        store.loom_register_agent_type(&record).is_ok(),
        "composite in-type with sane display fields registers"
    );
    // Round 4: shell-expandable CLI declarations are rejected up front —
    // `$SHELL`, quotes, and backslashes would re-resolve at exec time.
    for bad in ["$SHELL", "\"ffmpeg\"", "\\ffmpeg", "ffm*peg"] {
        let mut record = agent_type("shelly", "A", "B");
        record.clis = vec![bad.to_owned()];
        assert!(
            store.loom_register_agent_type(&record).is_err(),
            "cli `{bad}` must be rejected"
        );
    }
    // Round 5: shell builtins/dispatchers grant everything — rejected even
    // though they fit the charset. Same for a path form of one.
    for dispatcher in [
        ".", "eval", "xargs", "zsh", "/bin/sh", "env", "busybox", "toybox",
    ] {
        let mut record = agent_type("dispatchy", "A", "B");
        record.clis = vec![(*dispatcher).to_owned()];
        assert!(
            store.loom_register_agent_type(&record).is_err(),
            "dispatcher `{dispatcher}` must be rejected"
        );
    }
    // Bare names and absolute paths remain declarable.
    let mut record = agent_type("shelly", "A", "B");
    record.clis = vec!["ffmpeg".to_owned(), "/opt/homebrew/bin/yt-dlp".to_owned()];
    assert!(store.loom_register_agent_type(&record).is_ok());
    // Rounds 4-5: invisible/format/variation characters never reach a
    // glyph cell, and a glyph never LEADS with a combining mark.
    for bad_glyph in [
        "\u{2060}",
        "\u{061C}",
        "\u{00AD}",
        "\u{034F}",
        "\u{180E}",
        "\u{FE0F}",
        "\u{0301}x",
    ] {
        let mut record = agent_type("ghosty", "A", "B");
        record.glyph = (*bad_glyph).to_owned();
        assert!(
            store.loom_register_agent_type(&record).is_err(),
            "glyph {bad_glyph:?} must be rejected"
        );
    }
}
