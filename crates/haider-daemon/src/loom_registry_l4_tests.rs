#![allow(clippy::expect_used)]

use haider_protocol::error::ErrorCode;
use haider_protocol::loom::{
    LoomAgentType, LoomAuthorKind, LoomRegistryDeltaKind, LoomRegistryEntryKind,
    LoomRevisionExpectation, ValidatedLoomAuthorSpec,
};
use haider_protocol::typed_agent::TypedAgentInstallState;
use haider_store::{
    LoomArchiveResult, LoomRegistryMutation, Store, TypedAgentInstallCancelResult,
    TypedAgentInstallRetryResult, TypedAgentInstallWatchResult,
};

fn agent(id: &str) -> LoomAgentType {
    LoomAgentType {
        id: id.into(),
        name: "Reviewer".into(),
        job: "Review the patch".into(),
        in_type: "Patch".into(),
        out_type: "Verdict".into(),
        clis: vec!["l4-proof-cli".into()],
        apis: Vec::new(),
        denials: Vec::new(),
        skills: Vec::new(),
        scripts: Vec::new(),
        color: "#445566".into(),
        glyph: "R".into(),
        rev: 0,
    }
}

#[test]
fn cancel_is_durable_watchable_and_retry_keeps_registration() {
    let profile = tempfile::tempdir().expect("profile");
    let store = Store::open(profile.path()).expect("store");
    let outcome = store
        .loom_register_agent_type_with_install_cas(
            &agent("cancel-proof"),
            &LoomRevisionExpectation {
                rev: 0,
                digest: None,
            },
        )
        .expect("register");
    let LoomRegistryMutation::Applied { value, .. } = outcome else {
        panic!("new id cannot conflict");
    };
    let job = value.install_job.expect("required CLI creates a job");

    assert_eq!(
        store
            .typed_agent_install_cancel(&job.job_id)
            .expect("cancel"),
        TypedAgentInstallCancelResult::Cancelled
    );
    let TypedAgentInstallWatchResult::Watching(page) = store
        .typed_agent_install_watch(&job.job_id, 0)
        .expect("watch")
    else {
        panic!("known job watches");
    };
    assert_eq!(
        page.events
            .last()
            .map(|event| (event.job.state, event.job.cancelled)),
        Some((TypedAgentInstallState::Failed, true)),
        "the terminal cancellation fact is durable"
    );
    assert!(
        store
            .loom_agent_type("cancel-proof")
            .expect("registration")
            .is_some(),
        "cancellation never removes the registry row"
    );
    assert!(matches!(
        store.typed_agent_install_retry(&job.job_id).expect("retry"),
        TypedAgentInstallRetryResult::Requeued(_)
    ));
}

#[test]
fn unfenced_compatibility_writers_refuse_changed_content() {
    let profile = tempfile::tempdir().expect("profile");
    let store = Store::open(profile.path()).expect("store");
    let first = agent("fenced-only");
    store
        .loom_register_agent_type(&first)
        .expect("compatibility create");
    let mut changed = first;
    changed.job = "A concurrent edit must be fenced".into();
    let error = store
        .loom_register_agent_type(&changed)
        .expect_err("unfenced agent revision is refused");
    assert_eq!(error.code, ErrorCode::RevisionConflict);

    store
        .loom_register_workflow("fenced-flow: A -> A\nstep \"one\" :cmd")
        .expect("compatibility workflow create");
    let error = store
        .loom_register_workflow("fenced-flow: A -> A\nstep \"two\" :cmd")
        .expect_err("unfenced workflow revision is refused");
    assert_eq!(error.code, ErrorCode::RevisionConflict);
}

#[test]
fn cas_archive_and_registry_replay_preserve_exact_revision() {
    let profile = tempfile::tempdir().expect("profile");
    let store = Store::open(profile.path()).expect("store");
    let expected_absent = LoomRevisionExpectation {
        rev: 7,
        digest: Some("verbatim-stale-digest".into()),
    };
    let missing_conflict = store
        .loom_set_archived(
            LoomRegistryEntryKind::AgentType,
            "missing-archive-proof",
            true,
            &expected_absent,
        )
        .expect("missing archive CAS");
    let LoomArchiveResult::Conflict(conflict) = missing_conflict else {
        panic!("positive expectation against absence conflicts");
    };
    assert_eq!(conflict.expected, expected_absent);
    assert_eq!(conflict.current_rev, None);
    assert_eq!(conflict.current_digest, None);
    assert_eq!(
        store
            .loom_set_archived(
                LoomRegistryEntryKind::AgentType,
                "missing-archive-proof",
                true,
                &LoomRevisionExpectation {
                    rev: 0,
                    digest: None,
                },
            )
            .expect("absence-compatible archive"),
        LoomArchiveResult::NotFound
    );
    let outcome = store
        .loom_register_agent_type_with_install_cas(
            &agent("archive-proof"),
            &LoomRevisionExpectation {
                rev: 0,
                digest: None,
            },
        )
        .expect("register");
    let LoomRegistryMutation::Applied { value, .. } = outcome else {
        panic!("new id cannot conflict");
    };
    let registration = value.registration;
    let stale = store
        .loom_register_agent_type_with_install_cas(
            &agent("archive-proof"),
            &LoomRevisionExpectation {
                rev: registration.rev + 1,
                digest: None,
            },
        )
        .expect("typed conflict");
    let LoomRegistryMutation::Conflict(conflict) = stale else {
        panic!("stale save must conflict");
    };
    assert_eq!(conflict.current_rev, Some(registration.rev));
    assert_eq!(
        conflict.current_digest.as_deref(),
        Some(registration.digest.as_str())
    );

    let archived = store
        .loom_set_archived(
            LoomRegistryEntryKind::AgentType,
            &registration.id,
            true,
            &LoomRevisionExpectation {
                rev: registration.rev,
                digest: Some(registration.digest.clone()),
            },
        )
        .expect("archive");
    assert!(matches!(archived, LoomArchiveResult::Changed { .. }));
    assert!(
        store
            .loom_agent_type(&registration.id)
            .expect("default lookup")
            .is_none(),
        "archived rows leave default catalogs and selection"
    );
    assert!(
        store
            .loom_agent_type_revision(&registration.id, registration.rev, &registration.digest,)
            .expect("retained lookup")
            .is_some(),
        "an archived current revision remains exactly resolvable"
    );

    let snapshot = store.loom_registry_snapshot().expect("baseline");
    assert!(snapshot.entries.iter().any(|entry| entry.entry.archived));
    let page = store
        .loom_registry_watch_page(0, snapshot.through_cursor)
        .expect("replay");
    assert!(page.deltas.iter().any(|delta| {
        delta.entry.id == registration.id && delta.change == LoomRegistryDeltaKind::Archived
    }));

    let restored = store
        .loom_set_archived(
            LoomRegistryEntryKind::AgentType,
            &registration.id,
            false,
            &LoomRevisionExpectation {
                rev: registration.rev,
                digest: Some(registration.digest.clone()),
            },
        )
        .expect("unarchive");
    assert!(matches!(restored, LoomArchiveResult::Changed { .. }));
    assert!(
        store
            .loom_agent_type(&registration.id)
            .expect("restored default lookup")
            .is_some()
    );
    let head = store.loom_registry_head().expect("registry head");
    let page = store
        .loom_registry_watch_page(snapshot.through_cursor, head)
        .expect("unarchive replay");
    assert!(page.deltas.iter().any(|delta| {
        delta.entry.id == registration.id && delta.change == LoomRegistryDeltaKind::Unarchived
    }));
}

#[test]
fn archived_workflow_leaves_selection_but_retained_revision_resolves() {
    let profile = tempfile::tempdir().expect("profile");
    let store = Store::open(profile.path()).expect("store");
    let outcome = store
        .loom_register_workflow_cas(
            "archive-flow: A -> A\nstep \"one\" :cmd",
            &LoomRevisionExpectation {
                rev: 0,
                digest: None,
            },
        )
        .expect("workflow register");
    let LoomRegistryMutation::Applied {
        value: registration,
        ..
    } = outcome
    else {
        panic!("new workflow cannot conflict");
    };
    let registered = store
        .loom_workflow_registered_revision(&registration.id, registration.rev, &registration.digest)
        .expect("registered workflow revision")
        .expect("registered workflow exists");
    let pinned_digest = haider_protocol::graph::graph_template_digest(&registered.template);
    let archived = store
        .loom_set_archived(
            LoomRegistryEntryKind::Workflow,
            &registration.id,
            true,
            &LoomRevisionExpectation {
                rev: registration.rev,
                digest: Some(registration.digest.clone()),
            },
        )
        .expect("workflow archive");
    assert!(matches!(archived, LoomArchiveResult::Changed { .. }));
    assert!(
        store
            .loom_workflow(&registration.id)
            .expect("default workflow lookup")
            .is_none(),
        "fresh selection excludes an archived workflow"
    );
    assert!(
        store
            .loom_workflow_revision(&registration.id, &pinned_digest)
            .expect("retained workflow lookup")
            .is_some(),
        "an exact pinned workflow address remains resolvable"
    );
}

#[test]
fn validate_preview_uses_author_validator_without_registry_mutation() {
    let profile = tempfile::tempdir().expect("profile");
    let store = Store::open(profile.path()).expect("store");
    let head_before = store.loom_registry_head().expect("registry head");
    let text = serde_json::json!({
        "kind": "agent_type",
        "id": "preview-only",
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
    let invalid = text.replace("\"name\":\"Preview\"", "\"name\":\"\"");
    let preview_errors = crate::loom_author::validate(&invalid, LoomAuthorKind::AgentType, &[])
        .expect_err("empty name is located");
    let l1_draft = crate::loom_author::revise(
        "same-validator".into(),
        1,
        LoomAuthorKind::AgentType,
        invalid,
        &[],
    );
    assert_eq!(
        preview_errors, l1_draft.errors,
        "validate and the L1 editor return the same located errors"
    );
    let validated = crate::loom_author::validate(&text, LoomAuthorKind::AgentType, &[])
        .expect("located validator accepts document");
    let digest = crate::loom_author::canonical_digest(&validated, &[]).expect("digest preview");
    assert_eq!(digest.len(), 32);
    assert!(
        store
            .loom_agent_type("preview-only")
            .expect("registry read")
            .is_none(),
        "validation never writes the registry"
    );
    assert_eq!(
        store.loom_registry_head().expect("registry head"),
        head_before,
        "validation never publishes a registry delta"
    );

    let ValidatedLoomAuthorSpec::AgentType { record, .. } = validated else {
        panic!("agent-type validation preserves its kind");
    };
    let saved = store
        .loom_register_agent_type_with_install_cas(
            &record,
            &LoomRevisionExpectation {
                rev: 0,
                digest: None,
            },
        )
        .expect("save the preview");
    let LoomRegistryMutation::Applied { value, .. } = saved else {
        panic!("new id cannot conflict");
    };
    assert_eq!(
        value.registration.digest, digest,
        "the preview digest is exactly the digest a later save produces"
    );
}
