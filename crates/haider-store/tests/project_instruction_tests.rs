#![allow(clippy::expect_used)]

use haider_protocol::DeliveryMode;
use haider_protocol::envelope::{PromptRender, RawEnvelope, RenderTargets, SCHEMA_VERSION};
use haider_protocol::ids::{BranchId, DeviceId, EventId, RunId, SessionId};
use haider_protocol::project_instructions::{
    ProjectInstructionFileFact, ProjectInstructionsLoaded,
};
use haider_store::{EventStore, SessionCreateCommand, Store, TurnAcceptCommand};

fn accepted_store() -> (tempfile::TempDir, Store, SessionId, RunId) {
    let root = tempfile::tempdir().expect("store root");
    let store = Store::open(root.path()).expect("store");
    let session_id = SessionId::new("project-instruction-worker-fact");
    let run_id = RunId::new("project-instruction-run");
    store
        .create_session(&SessionCreateCommand {
            command_id: "create-project-instruction-store".into(),
            request_digest: "create-project-instruction-store-digest".into(),
            request_json: r#"{"session":"project-instruction-store"}"#.into(),
            session_id: session_id.clone(),
            cwd: "/tmp".into(),
            provider: "fake".into(),
            model: "fake-model".into(),
            max_tokens: 4096,
            permission_overrides: None,
            system_prompt_version: "haider-system-v2".into(),
            event_id: EventId::new("created-project-instruction-store"),
            device_id: DeviceId::new("project-instruction-store-device"),
        })
        .expect("create session");
    store
        .accept_turn(&TurnAcceptCommand {
            command_id: "accept-project-instruction-store".into(),
            request_digest: "accept-project-instruction-store-digest".into(),
            request_json: r#"{"turn":"project-instruction-store"}"#.into(),
            session_id: session_id.clone(),
            worker_generation: store.worker_generation(),
            run_id: run_id.clone(),
            agent_id: None,
            branch_id: None,
            text: "load instructions".into(),
            attachments: Vec::new(),
            mode: DeliveryMode::Queue,
            queued_event_id: EventId::new("queued-project-instruction-store"),
            user_event_id: EventId::new("user-project-instruction-store"),
            active_event_id: EventId::new("active-project-instruction-store"),
            device_id: DeviceId::new("project-instruction-store-device"),
        })
        .expect("accept turn");
    (root, store, session_id, run_id)
}

fn envelope(
    store: &Store,
    session_id: &SessionId,
    run_id: &RunId,
    branch_id: Option<BranchId>,
    event_id: &str,
    payload: serde_json::Value,
) -> RawEnvelope {
    RawEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(event_id),
        seq: 0,
        session_id: session_id.clone(),
        branch_id,
        run_id: Some(run_id.clone()),
        agent_id: None,
        device_id: DeviceId::new("project-instruction-store-device"),
        authority_epoch: 0,
        worker_generation: store.worker_generation(),
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: false,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload,
    }
}

fn fact() -> ProjectInstructionsLoaded {
    ProjectInstructionsLoaded {
        files: vec![ProjectInstructionFileFact {
            path: "/tmp/HAIDER.md".into(),
            digest: blake3::hash(b"policy").to_hex().to_string(),
            bytes: 6,
            truncated: false,
        }],
    }
}

/// MUTATION CHECK: reject the recognized additive fact as an unknown core
/// payload. Expected RUNTIME failure: the live worker append returns an
/// invalid-payload error or the raw fact is not preserved byte-for-byte.
#[test]
fn worker_append_accepts_project_instruction_fact_for_an_active_run() {
    let (_root, store, session_id, run_id) = accepted_store();
    let payload = fact().to_payload_value().expect("fact payload");
    let mut batch = [envelope(
        &store,
        &session_id,
        &run_id,
        None,
        "project-instruction-fact",
        payload.clone(),
    )];
    store.append_worker(&mut batch).expect("worker fact append");
    let read = store.read(&session_id, 0, 32).expect("read journal");
    assert_eq!(read.last().expect("fact event").payload, payload);
    assert_eq!(
        read.last().expect("fact event").render.prompt,
        PromptRender::Omit
    );
}

/// MUTATION CHECK: exempt the supplemental fact from accepted-branch
/// validation. Expected RUNTIME failure: a main-branch run accepts a fact
/// stamped with an unrelated named branch.
#[test]
fn worker_append_rejects_project_instruction_fact_on_the_wrong_branch() {
    let (_root, store, session_id, run_id) = accepted_store();
    let mut batch = [envelope(
        &store,
        &session_id,
        &run_id,
        Some(BranchId::new("wrong-branch")),
        "wrong-branch-project-instruction-fact",
        fact().to_payload_value().expect("fact payload"),
    )];
    let error = store
        .append_worker(&mut batch)
        .expect_err("wrong branch must fail");
    assert_eq!(
        error.code,
        haider_protocol::error::ErrorCode::InvalidArgument
    );
}

/// MUTATION CHECK: accept any raw unknown payload merely because its type tag
/// resembles the B3 fact. Expected RUNTIME failure: malformed file entries
/// commit through the worker transition gate.
#[test]
fn worker_append_rejects_malformed_project_instruction_fact() {
    let (_root, store, session_id, run_id) = accepted_store();
    let mut batch = [envelope(
        &store,
        &session_id,
        &run_id,
        None,
        "malformed-project-instruction-fact",
        serde_json::json!({
            "type": "project_instructions_loaded",
            "files": "not-an-ordered-file-list"
        }),
    )];
    let error = store
        .append_worker(&mut batch)
        .expect_err("malformed fact must fail");
    assert_eq!(
        error.code,
        haider_protocol::error::ErrorCode::InvalidArgument
    );
}
