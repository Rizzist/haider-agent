//! B1 — the durable Loom registry: agent types + pipe-source workflows.
//! The store is the compiler authority (callers send SOURCE, the registry
//! compiles inside the transaction) and owns the rev law: new id → rev 1,
//! identical content → idempotent no-op, changed content → rev + 1.
#![allow(clippy::expect_used)]

use haider_protocol::loom::LoomAgentType;
use haider_store::Store;

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
