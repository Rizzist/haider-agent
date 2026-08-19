//! B1 — the durable Loom registry: agent types + pipe-source workflows.
//! The store is the compiler authority (callers send SOURCE, the registry
//! compiles inside the transaction) and owns the rev law: new id → rev 1,
//! identical content → idempotent no-op, changed content → rev + 1.
#![allow(clippy::expect_used)]

use haider_protocol::ids::{DeviceId, EventId, GraphId, SessionId};
use haider_protocol::loom::LoomAgentType;
use haider_store::{GraphPinCommand, GraphPinOutcome, SessionCreateCommand, Store};

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
}
