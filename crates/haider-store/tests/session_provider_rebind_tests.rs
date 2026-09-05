#![allow(clippy::expect_used)]

use haider_protocol::envelope::PromptRender;
use haider_protocol::error::ErrorCode;
use haider_protocol::ids::{DeviceId, EventId, SessionId};
use haider_protocol::session::SessionProviderRebound;
use haider_store::{
    SessionCreateCommand, SessionProviderRebindCommand, SessionProviderRebindOutcome,
    SessionSelectModelCommand, Store,
};

fn create(store: &Store, session: &str) {
    store
        .create_session(&SessionCreateCommand {
            command_id: format!("create-{session}"),
            request_digest: format!("create-digest-{session}"),
            request_json: format!(r#"{{"session":"{session}"}}"#),
            session_id: SessionId::new(session),
            cwd: "/tmp".into(),
            provider: "source".into(),
            model: "test-model".into(),
            max_tokens: 4096,
            permission_overrides: None,
            effort: None,
            fast: false,
            cache_policy: Default::default(),
            system_prompt_version: "test-system".into(),
            event_id: EventId::new(format!("created-{session}")),
            device_id: DeviceId::new("test-device"),
        })
        .expect("create session");
}

fn command(store: &Store) -> SessionProviderRebindCommand {
    SessionProviderRebindCommand {
        command_id: "rebind-1".into(),
        request_digest: "rebind-digest-1".into(),
        request_json:
            r#"{"provider":"proxy","base_url":"http://127.0.0.1:4242/v1","account":"row-account"}"#
                .into(),
        session_id: SessionId::new("session-a"),
        worker_generation: store.worker_generation(),
        provider: "proxy".into(),
        base_url: Some("http://127.0.0.1:4242/v1".into()),
        account: Some("row-account".into()),
        event_id: EventId::new("rebound-1"),
        device_id: DeviceId::new("test-device"),
    }
}

#[test]
fn provider_rebind_durable_event_replays_to_identical_metadata_and_receipt() {
    let root = tempfile::tempdir().expect("temporary store");
    let store = Store::open(root.path()).expect("open");
    create(&store, "session-a");
    create(&store, "session-b");
    let command = command(&store);
    let mut replayed_metadata = store
        .session_metadata(&command.session_id)
        .expect("read metadata")
        .expect("typed metadata");
    let other_before = store
        .session_metadata(&SessionId::new("session-b"))
        .expect("other metadata");
    let SessionProviderRebindOutcome::Committed { selected, envelope } = store
        .rebind_session_provider(&command)
        .expect("rebind commits")
    else {
        panic!("new command must commit")
    };
    assert_eq!(envelope.seq, 2);
    assert_eq!(selected.selected_seq, envelope.seq);
    assert!(matches!(envelope.render.prompt, PromptRender::Omit));
    assert_eq!(envelope.payload["type"], "session_provider_rebound");
    let fact =
        SessionProviderRebound::from_payload_value(&envelope.payload).expect("typed replay fact");
    fact.apply_to_metadata(&mut replayed_metadata);
    assert_eq!(replayed_metadata.model, "test-model");
    assert_eq!(replayed_metadata.provider, "proxy");
    assert_eq!(replayed_metadata.provider_base_url, command.base_url);
    assert_eq!(replayed_metadata.account_alias, command.account);
    assert_eq!(
        replayed_metadata.provider_rebind_id.as_deref(),
        Some(command.command_id.as_str())
    );
    assert_eq!(
        store
            .session_metadata(&command.session_id)
            .expect("metadata"),
        Some(replayed_metadata.clone())
    );
    assert_eq!(
        store
            .session_metadata(&SessionId::new("session-b"))
            .expect("other metadata"),
        other_before
    );
    assert_eq!(
        store
            .rebind_session_provider(&command)
            .expect("idempotent retry"),
        SessionProviderRebindOutcome::IdempotentReplay {
            selected: selected.clone()
        }
    );
    let journal_before = store.journal_replay(&command.session_id).expect("journal");
    assert_eq!(journal_before.len(), 2);
    drop(store);
    let reopened = Store::open(root.path()).expect("reopen");
    assert_eq!(
        reopened
            .journal_replay(&command.session_id)
            .expect("replayed journal"),
        journal_before
    );
    assert_eq!(
        reopened
            .session_metadata(&command.session_id)
            .expect("reopened metadata"),
        Some(replayed_metadata)
    );
    assert_eq!(
        reopened
            .session_provider_rebind_receipt(
                &command.command_id,
                &command.request_digest,
                &command.request_json
            )
            .expect("receipt across generation change"),
        Some(selected)
    );
}

#[test]
fn provider_rebind_receipt_conflict_and_stale_generation_leave_journal_unchanged() {
    let root = tempfile::tempdir().expect("temporary store");
    let store = Store::open(root.path()).expect("open");
    create(&store, "session-a");
    let mut command = command(&store);
    command.worker_generation = command.worker_generation.saturating_add(1);
    assert_eq!(
        store
            .rebind_session_provider(&command)
            .expect_err("stale generation")
            .code,
        ErrorCode::SingleWriterViolation
    );
    assert_eq!(
        store
            .journal_replay(&command.session_id)
            .expect("journal")
            .len(),
        1
    );
    command.worker_generation = store.worker_generation();
    store.rebind_session_provider(&command).expect("rebind");
    let journal = store.journal_replay(&command.session_id).expect("journal");
    command.request_digest = "different".into();
    command.request_json = r#"{"different":true}"#.into();
    command.provider = "must-not-commit".into();
    assert!(store.rebind_session_provider(&command).is_err());
    assert_eq!(
        store.journal_replay(&command.session_id).expect("journal"),
        journal
    );
    assert_eq!(
        store
            .session_metadata(&command.session_id)
            .expect("metadata")
            .expect("typed")
            .provider,
        "proxy"
    );
}

#[test]
fn provider_rebind_omitted_coordinates_clear_only_the_session_override() {
    let root = tempfile::tempdir().expect("temporary store");
    let store = Store::open(root.path()).expect("open");
    create(&store, "session-a");
    let mut command = command(&store);
    store
        .rebind_session_provider(&command)
        .expect("first rebind");
    command.command_id = "rebind-2".into();
    command.event_id = EventId::new("rebound-2");
    command.request_digest = "rebind-digest-2".into();
    command.request_json = r#"{"provider":"proxy"}"#.into();
    command.base_url = None;
    command.account = None;
    let SessionProviderRebindOutcome::Committed { envelope, .. } = store
        .rebind_session_provider(&command)
        .expect("clear rebind")
    else {
        panic!("clear must commit")
    };
    assert_eq!(
        *envelope.payload,
        serde_json::json!({"type":"session_provider_rebound","rebind_id":"rebind-2","provider":"proxy"})
    );
    let metadata = store
        .session_metadata(&command.session_id)
        .expect("metadata")
        .expect("typed");
    assert_eq!(metadata.provider_base_url, None);
    assert_eq!(metadata.account_alias, None);
    assert_eq!(metadata.model, "test-model");
}

#[test]
fn model_provider_switch_clears_rebind_override_but_same_provider_preserves_it() {
    let root = tempfile::tempdir().expect("temporary store");
    let store = Store::open(root.path()).expect("open");
    create(&store, "session-a");
    let rebound = command(&store);
    store.rebind_session_provider(&rebound).expect("rebind");
    let mut selected = SessionSelectModelCommand {
        command_id: "model-1".into(),
        request_digest: "model-digest-1".into(),
        request_json: r#"{"provider":"proxy","model":"model-2"}"#.into(),
        session_id: rebound.session_id.clone(),
        worker_generation: store.worker_generation(),
        provider: "proxy".into(),
        model: "model-2".into(),
        expected_pair: None,
        event_id: EventId::new("model-selected-1"),
        device_id: DeviceId::new("test-device"),
    };
    store
        .select_session_model(&selected)
        .expect("same-provider model selection");
    let same_provider = store
        .session_metadata(&rebound.session_id)
        .expect("metadata")
        .expect("typed");
    assert_eq!(same_provider.provider_base_url, rebound.base_url);
    assert_eq!(
        same_provider.provider_rebind_id.as_deref(),
        Some("rebind-1")
    );
    assert_eq!(same_provider.account_alias, rebound.account);
    selected.command_id = "model-2".into();
    selected.request_digest = "model-digest-2".into();
    selected.request_json = r#"{"provider":"other","model":"model-2"}"#.into();
    selected.provider = "other".into();
    selected.event_id = EventId::new("model-selected-2");
    store
        .select_session_model(&selected)
        .expect("different-provider model selection");
    let switched = store
        .session_metadata(&rebound.session_id)
        .expect("metadata")
        .expect("typed");
    assert_eq!(switched.provider, "other");
    assert_eq!(switched.provider_base_url, None);
    assert_eq!(switched.provider_rebind_id, None);
    assert_eq!(switched.account_alias, None);
}
