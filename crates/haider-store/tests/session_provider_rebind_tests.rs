#![allow(clippy::expect_used)]

use haider_protocol::envelope::PromptRender;
use haider_protocol::error::ErrorCode;
use haider_protocol::ids::{DeviceId, EventId, SessionId};
use haider_protocol::session::{ModelSelected, SessionProviderRebound};
use haider_store::{
    SessionCreateCommand, SessionProviderRebindCommand, SessionProviderRebindOutcome,
    SessionSelectModelCommand, SessionSelectModelOutcome, Store,
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
            max_tokens_source: None,
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

#[test]
fn legacy_omitted_budget_source_does_not_mint_a_noop_selection_epoch() {
    use haider_protocol::output_budget::{SessionOutputBudgetSourceV1, SessionOutputBudgetV1};
    use haider_store::SessionSelectModelOutcome;

    let root = tempfile::tempdir().expect("temporary store");
    let store = Store::open(root.path()).expect("open");
    create(&store, "legacy-budget-source");
    let command = SessionSelectModelCommand {
        command_id: "legacy-noop".into(),
        request_digest: "legacy-noop-digest".into(),
        request_json: "{}".into(),
        session_id: SessionId::new("legacy-budget-source"),
        worker_generation: store.worker_generation(),
        provider: "source".into(),
        model: "test-model".into(),
        expected_pair: None,
        account_alias: None,
        output_budget: Some(SessionOutputBudgetV1 {
            max_tokens: 4096,
            source: SessionOutputBudgetSourceV1::Derived,
            clamped: None,
        }),
        event_id: EventId::new("legacy-noop-event"),
        device_id: DeviceId::new("test-device"),
    };
    let SessionSelectModelOutcome::Committed { selected, .. } = store
        .select_session_model(&command)
        .expect("no-op selection")
    else {
        panic!("commit")
    };
    assert_eq!(selected.selected_seq, 0);
    assert_eq!(
        store
            .session_metadata(&command.session_id)
            .expect("metadata")
            .expect("typed")
            .selection_epoch,
        Some(0)
    );
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
fn cross_provider_pick_clears_pin_and_old_route_with_typed_notice() {
    let root = tempfile::tempdir().expect("temporary store");
    let store = Store::open(root.path()).expect("store");
    create(&store, "session-a");
    let id = SessionId::new("session-a");
    let mut first = SessionSelectModelCommand {
        command_id: "pin-source".into(),
        request_digest: "pin-source-digest".into(),
        request_json: r#"{"pin":"bed-a"}"#.into(),
        session_id: id.clone(),
        worker_generation: store.worker_generation(),
        provider: "source".into(),
        model: "test-model".into(),
        expected_pair: None,
        account_alias: Some("bed-a".into()),
        output_budget: None,
        event_id: EventId::new("pin-source-event"),
        device_id: DeviceId::new("test-device"),
    };
    store.select_session_model(&first).expect("pin source");
    let current = store
        .session_metadata(&id)
        .expect("metadata")
        .expect("typed");
    store
        .commit_resolved_route(
            &id,
            "source",
            "test-model",
            current.selection_epoch.unwrap_or(0),
            Some("old-route"),
            &DeviceId::new("test-device"),
        )
        .expect("served route");
    first.command_id = "same-provider".into();
    first.request_digest = "same-provider-digest".into();
    first.request_json = r#"{"model":"same-provider-model"}"#.into();
    first.model = "same-provider-model".into();
    first.account_alias = None;
    first.event_id = EventId::new("same-provider-event");
    let SessionSelectModelOutcome::Committed { selected: same, .. } = store
        .select_session_model(&first)
        .expect("same-provider selection")
    else {
        panic!("same-provider commit");
    };
    assert_eq!(same.cleared_account_pin, None);
    assert_eq!(
        store
            .session_metadata(&id)
            .expect("metadata")
            .expect("typed")
            .account_alias
            .as_deref(),
        Some("bed-a")
    );
    first.command_id = "cross-provider".into();
    first.request_digest = "cross-provider-digest".into();
    first.request_json = r#"{"provider":"target"}"#.into();
    first.provider = "target".into();
    first.account_alias = None;
    first.event_id = EventId::new("cross-provider-event");
    let SessionSelectModelOutcome::Committed { selected, envelope } = store
        .select_session_model(&first)
        .expect("cross-provider selection")
    else {
        panic!("committed selection");
    };
    assert_eq!(selected.cleared_account_pin.as_deref(), Some("bed-a"));
    assert_eq!(
        ModelSelected::from_payload_value(&envelope.payload)
            .expect("fact")
            .cleared_account_pin
            .as_deref(),
        Some("bed-a")
    );
    let metadata = store
        .session_metadata(&id)
        .expect("metadata")
        .expect("typed");
    assert_eq!(metadata.account_alias, None);
    assert_eq!(metadata.resolved_route_alias, None);
    assert!(!metadata.resolved_route_seen);
    let SessionSelectModelOutcome::IdempotentReplay { selected: replay } =
        store.select_session_model(&first).expect("retry")
    else {
        panic!("receipt replay");
    };
    assert_eq!(replay, selected);
}

#[test]
fn derived_child_inherits_explicit_route_and_rejects_mismatched_provider() {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("store");
    create(&store, "parent");
    let parent = SessionId::new("parent");
    let connection = rusqlite::Connection::open(store.database_path()).expect("fixture database");
    connection.execute("UPDATE sessions SET meta_json = json_set(meta_json, '$.account_alias', 'bed-b', '$.provider_base_url', 'https://synthetic.invalid', '$.provider_rebind_id', 'rebind-parent') WHERE id = ?1", [parent.as_str()]).expect("explicit parent fixture");
    drop(connection);
    let source = store
        .session_metadata(&parent)
        .expect("parent metadata")
        .expect("typed");
    let child = SessionCreateCommand {
        command_id: "create-derived".into(),
        request_digest: "create-derived-digest".into(),
        request_json: r#"{"child":"derived"}"#.into(),
        session_id: SessionId::new("derived"),
        cwd: "/tmp".into(),
        provider: "source".into(),
        model: "test-model".into(),
        max_tokens: 4096,
        max_tokens_source: None,
        permission_overrides: None,
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        system_prompt_version: "test-system".into(),
        event_id: EventId::new("derived-created"),
        device_id: DeviceId::new("test-device"),
    };
    store
        .create_session_with_workspace_configuration(
            &child,
            Default::default(),
            None,
            None,
            Some(source.clone()),
        )
        .expect("derived child");
    let metadata = store
        .session_metadata(&child.session_id)
        .expect("child metadata")
        .expect("typed");
    assert_eq!(metadata.account_alias, source.account_alias);
    assert_eq!(metadata.provider_base_url, source.provider_base_url);
    assert_eq!(metadata.provider_rebind_id, source.provider_rebind_id);
    let mut wrong = child.clone();
    wrong.session_id = SessionId::new("wrong-provider");
    wrong.command_id = "create-wrong-provider".into();
    wrong.request_digest = "create-wrong-provider-digest".into();
    wrong.request_json = r#"{"child":"wrong-provider"}"#.into();
    wrong.provider = "target".into();
    assert!(
        store
            .create_session_with_workspace_configuration(
                &wrong,
                Default::default(),
                None,
                None,
                Some(source)
            )
            .is_err()
    );
    assert!(
        store
            .session_metadata(&wrong.session_id)
            .expect("not created")
            .is_none()
    );
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
        serde_json::json!({"type":"session_provider_rebound","rebind_id":"rebind-2","provider":"proxy","selection_epoch":envelope.seq})
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
        account_alias: None,
        output_budget: None,
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

#[test]
fn selection_epoch_survives_restart_and_advances_for_budget_model_and_route() {
    use haider_protocol::output_budget::{SessionOutputBudgetSourceV1, SessionOutputBudgetV1};
    use haider_protocol::session::ModelSelected;
    use haider_store::SessionSelectModelOutcome;

    let root = tempfile::tempdir().expect("temporary store");
    let store = Store::open(root.path()).expect("open");
    create(&store, "session-a");
    let id = SessionId::new("session-a");
    assert_eq!(
        store
            .session_metadata(&id)
            .expect("metadata")
            .expect("typed")
            .selection_epoch,
        Some(0)
    );
    let mut selection = SessionSelectModelCommand {
        command_id: "budget-1".into(),
        request_digest: "budget-digest-1".into(),
        request_json: r#"{"budget":20000}"#.into(),
        session_id: id.clone(),
        worker_generation: store.worker_generation(),
        provider: "source".into(),
        model: "test-model".into(),
        expected_pair: None,
        account_alias: None,
        output_budget: Some(SessionOutputBudgetV1 {
            max_tokens: 20_000,
            source: SessionOutputBudgetSourceV1::UserSet { requested: 20_000 },
            clamped: None,
        }),
        event_id: EventId::new("budget-event-1"),
        device_id: DeviceId::new("test-device"),
    };
    let SessionSelectModelOutcome::Committed {
        selected: budget,
        envelope,
    } = store
        .select_session_model(&selection)
        .expect("budget commit")
    else {
        panic!("commit")
    };
    assert_eq!(budget.selected_seq, envelope.seq);
    assert_eq!(
        ModelSelected::from_payload_value(&envelope.payload)
            .expect("fact")
            .selection_epoch,
        Some(budget.selected_seq)
    );
    selection.command_id = "noop-1".into();
    selection.request_digest = "noop-digest-1".into();
    selection.request_json = r#"{"noop":true}"#.into();
    selection.event_id = EventId::new("noop-event-1");
    let SessionSelectModelOutcome::Committed {
        selected: noop,
        envelope,
    } = store
        .select_session_model(&selection)
        .expect("no-op commit")
    else {
        panic!("commit")
    };
    assert_eq!(noop.selected_seq, budget.selected_seq);
    assert!(envelope.seq > noop.selected_seq);
    assert_eq!(
        ModelSelected::from_payload_value(&envelope.payload)
            .expect("no-op fact")
            .selection_epoch,
        Some(budget.selected_seq)
    );
    selection.command_id = "account-bind-1".into();
    selection.request_digest = "account-bind-digest-1".into();
    selection.request_json = r#"{"account":"account-A"}"#.into();
    selection.account_alias = Some("account-A".into());
    selection.event_id = EventId::new("account-bind-event-1");
    let SessionSelectModelOutcome::Committed {
        selected: account_bound,
        ..
    } = store
        .select_session_model(&selection)
        .expect("account binding commit")
    else {
        panic!("commit")
    };
    assert!(account_bound.selected_seq > noop.selected_seq);
    assert_eq!(
        store
            .session_metadata(&id)
            .expect("metadata")
            .expect("typed")
            .account_alias
            .as_deref(),
        Some("account-A")
    );
    selection.command_id = "model-2".into();
    selection.request_digest = "model-digest-2".into();
    selection.request_json = r#"{"model":"another"}"#.into();
    selection.model = "another".into();
    selection.account_alias = None;
    selection.event_id = EventId::new("model-event-2");
    let SessionSelectModelOutcome::Committed {
        selected: model, ..
    } = store
        .select_session_model(&selection)
        .expect("model commit")
    else {
        panic!("commit")
    };
    assert!(model.selected_seq > budget.selected_seq);
    let mut route = command(&store);
    route.provider = "source".into();
    let SessionProviderRebindOutcome::Committed {
        selected: rebound,
        envelope,
    } = store.rebind_session_provider(&route).expect("route commit")
    else {
        panic!("commit")
    };
    assert!(rebound.selected_seq > model.selected_seq);
    assert_eq!(
        SessionProviderRebound::from_payload_value(&envelope.payload)
            .expect("route fact")
            .selection_epoch,
        Some(rebound.selected_seq)
    );
    drop(store);
    let reopened = Store::open(root.path()).expect("restart");
    assert_eq!(
        reopened
            .session_metadata(&id)
            .expect("metadata")
            .expect("typed")
            .selection_epoch,
        Some(rebound.selected_seq)
    );
    selection.command_id = "budget-3".into();
    selection.request_digest = "budget-digest-3".into();
    selection.request_json = r#"{"budget":12000}"#.into();
    selection.worker_generation = reopened.worker_generation();
    selection.output_budget = Some(SessionOutputBudgetV1 {
        max_tokens: 12_000,
        source: SessionOutputBudgetSourceV1::UserSet { requested: 12_000 },
        clamped: None,
    });
    selection.event_id = EventId::new("budget-event-3");
    let SessionSelectModelOutcome::Committed {
        selected: after_restart,
        ..
    } = reopened
        .select_session_model(&selection)
        .expect("post-restart commit")
    else {
        panic!("commit")
    };
    assert!(after_restart.selected_seq > rebound.selected_seq);
}
