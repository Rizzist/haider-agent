#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::envelope::{
    EventEnvelope, PromptRender, RawEnvelope, RenderTargets, SCHEMA_VERSION,
};
use haider_protocol::hook::{HookEventPayload, HookNotice};
use haider_protocol::ids::{DeviceId, EventId, RunId, SessionId};
use haider_protocol::state::RunState;
use haider_store::{EventStore, SessionCreateCommand, SessionCreateOutcome, Store};

fn event(
    session_id: &SessionId,
    event_id: &str,
    payload: serde_json::Value,
    generation: u64,
) -> RawEnvelope {
    EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(event_id),
        seq: 0,
        session_id: session_id.clone(),
        branch_id: None,
        run_id: Some(RunId::new("hook-outbox-run")),
        agent_id: None,
        device_id: DeviceId::new("hook-outbox-device"),
        authority_epoch: 0,
        worker_generation: generation,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: false,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: payload.into(),
    }
}

/// MUTATION CHECK: enqueue after commit, keep the outbox only in memory, or
/// enqueue a rejected duplicate. Expected RUNTIME failure: the survived fact
/// is absent after reopen, acknowledgement does not remove it, or rollback
/// leaves phantom work.
#[test]
fn hook_dispatch_outbox_is_atomic_persistent_and_idempotently_acknowledged() {
    let profile = tempfile::tempdir().expect("profile");
    let session_id = SessionId::new("hook-outbox-session");
    let store = Store::open(profile.path()).expect("store");
    let generation = store.worker_generation();
    let created = store
        .create_session(&SessionCreateCommand {
            command_id: "create-hook-outbox".into(),
            request_digest: "create-hook-outbox-digest".into(),
            request_json: r#"{"session":"hook-outbox"}"#.into(),
            session_id: session_id.clone(),
            cwd: std::fs::canonicalize(profile.path())
                .expect("canonical")
                .to_str()
                .expect("UTF-8")
                .to_owned(),
            provider: "fake".into(),
            model: "fake-model".into(),
            max_tokens: 4096,
            permission_overrides: None,
            effort: None,
            fast: false,
            cache_policy: Default::default(),
            system_prompt_version: "hook-outbox-v1".into(),
            event_id: EventId::new("hook-outbox-created"),
            device_id: DeviceId::new("hook-outbox-device"),
        })
        .expect("create");
    let SessionCreateOutcome::Committed { envelope, .. } = created else {
        panic!("fresh create must commit");
    };
    store
        .complete_hook_dispatch(&session_id, envelope.seq)
        .expect("ack create");

    let mut survived = [event(
        &session_id,
        "hook-outbox-survived",
        serde_json::to_value(EventPayload::RunState(RunState::Thinking)).expect("payload"),
        generation,
    )];
    store.append(&mut survived).expect("commit survived fact");
    let survived_seq = survived[0].seq;
    drop(store);

    let store = Store::open(profile.path()).expect("reopen");
    let pending = store.pending_hook_dispatches(16).expect("pending");
    assert_eq!(pending, survived);
    store
        .complete_hook_dispatch(&session_id, survived_seq)
        .expect("ack");
    store
        .complete_hook_dispatch(&session_id, survived_seq)
        .expect("idempotent ack");
    assert!(store.pending_hook_dispatches(16).expect("empty").is_empty());

    let mut rejected = [event(
        &session_id,
        "hook-outbox-survived",
        serde_json::to_value(EventPayload::RunState(RunState::Done)).expect("payload"),
        store.worker_generation(),
    )];
    assert!(store.append(&mut rejected).is_err());
    assert!(
        store
            .pending_hook_dispatches(16)
            .expect("rollback empty")
            .is_empty()
    );
}

/// MUTATION CHECK: split the batched acknowledgement into per-row autocommit
/// deletes, drop the rollback on a poisoned row, or skip rows for a second
/// session. Expected RUNTIME failure: the poisoned batch leaves a partial
/// delete behind, or the reopened store's pending set is not exactly the
/// unacknowledged remainder.
#[test]
fn batched_acknowledgement_is_one_atomic_durable_transaction() {
    let profile = tempfile::tempdir().expect("profile");
    let store = Store::open(profile.path()).expect("store");
    let cwd = std::fs::canonicalize(profile.path())
        .expect("canonical")
        .to_str()
        .expect("UTF-8")
        .to_owned();
    let mut sessions = Vec::new();
    for name in ["hook-batch-a", "hook-batch-b"] {
        let session_id = SessionId::new(name);
        let created = store
            .create_session(&SessionCreateCommand {
                command_id: format!("create-{name}"),
                request_digest: format!("create-{name}-digest"),
                request_json: format!(r#"{{"session":"{name}"}}"#),
                session_id: session_id.clone(),
                cwd: cwd.clone(),
                provider: "fake".into(),
                model: "fake-model".into(),
                max_tokens: 4096,
                permission_overrides: None,
                effort: None,
                fast: false,
                cache_policy: Default::default(),
                system_prompt_version: "hook-batch-v1".into(),
                event_id: EventId::new(format!("{name}-created")),
                device_id: DeviceId::new("hook-outbox-device"),
            })
            .expect("create");
        let SessionCreateOutcome::Committed { .. } = created else {
            panic!("fresh create must commit");
        };
        sessions.push(session_id);
    }
    let generation = store.worker_generation();
    let (session_a, session_b) = (sessions[0].clone(), sessions[1].clone());
    for (session_id, event_id) in [
        (&session_a, "hook-batch-a-2"),
        (&session_a, "hook-batch-a-3"),
        (&session_b, "hook-batch-b-2"),
    ] {
        let mut batch = [event(
            session_id,
            event_id,
            serde_json::to_value(EventPayload::RunState(RunState::Thinking)).expect("payload"),
            generation,
        )];
        store.append(&mut batch).expect("commit fact");
    }
    let pending_pairs = |store: &Store| {
        let mut pairs: Vec<(String, u64)> = store
            .pending_hook_dispatches(16)
            .expect("pending")
            .into_iter()
            .map(|envelope| (envelope.session_id.as_str().to_owned(), envelope.seq))
            .collect();
        pairs.sort();
        pairs
    };
    let all_rows = vec![
        ("hook-batch-a".to_owned(), 1),
        ("hook-batch-a".to_owned(), 2),
        ("hook-batch-a".to_owned(), 3),
        ("hook-batch-b".to_owned(), 1),
        ("hook-batch-b".to_owned(), 2),
    ];
    assert_eq!(pending_pairs(&store), all_rows);

    // A poisoned batch (seq beyond SQLite INTEGER) must delete NOTHING, even
    // though a valid row precedes the poison — one transaction, all or none.
    assert!(
        store
            .complete_hook_dispatches(&[(session_a.clone(), 2), (session_b.clone(), u64::MAX)])
            .is_err()
    );
    assert_eq!(pending_pairs(&store), all_rows);

    // Empty batch is a no-op; a valid cross-session batch lands durably.
    store.complete_hook_dispatches(&[]).expect("empty batch");
    let acked = [
        (session_a.clone(), 1),
        (session_a.clone(), 3),
        (session_b.clone(), 1),
    ];
    store.complete_hook_dispatches(&acked).expect("batch ack");
    drop(store);

    let store = Store::open(profile.path()).expect("reopen");
    assert_eq!(
        pending_pairs(&store),
        vec![
            ("hook-batch-a".to_owned(), 2),
            ("hook-batch-b".to_owned(), 2),
        ]
    );
    store
        .complete_hook_dispatches(&acked)
        .expect("idempotent batch re-ack");
    store
        .complete_hook_dispatches(&[(session_a, 2), (session_b, 2)])
        .expect("ack remainder");
    assert!(store.pending_hook_dispatches(16).expect("empty").is_empty());
}

/// MUTATION CHECK: recursively enqueue hook lifecycle facts. Expected RUNTIME
/// failure: a HookNotice produces new hook-dispatch work.
#[test]
fn hook_engine_facts_do_not_reenter_the_dispatch_outbox() {
    let profile = tempfile::tempdir().expect("profile");
    let session_id = SessionId::new("hook-outbox-engine-session");
    let store = Store::open(profile.path()).expect("store");
    let mut notice = [event(
        &session_id,
        "hook-outbox-notice",
        HookEventPayload::HookNotice(HookNotice {
            hook: Some("example".into()),
            digest: Some("a".repeat(64)),
            source: "/workspace/hooks.json".into(),
            reason: "test".into(),
        })
        .to_payload_value()
        .expect("payload"),
        store.worker_generation(),
    )];
    store.append(&mut notice).expect("append notice");
    assert!(
        store
            .pending_hook_dispatches(16)
            .expect("pending")
            .is_empty()
    );
}
