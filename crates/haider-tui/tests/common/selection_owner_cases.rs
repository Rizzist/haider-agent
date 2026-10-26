//! C6: use actual committed Store versions, rather than invented reply epochs.
//! Every variant kills `ignore-catalog-owner-in-no-op`; controls kill
//! `advance-every-selection` and `reject-equal-version-budget-completion`.

use super::*;
use haider_protocol::ids::{DeviceId, EventId, SessionId};
use haider_protocol::output_budget::SessionOutputBudgetSourceV1::{Derived, UserSet};
use haider_protocol::session::SessionMetadataV1;
use haider_store::{
    SessionCreateCommand, SessionSelectModelCommand, SessionSelectModelOutcome, Store,
};

struct Selection {
    _root: tempfile::TempDir,
    store: Store,
    session: SessionId,
    before: SessionMetadataV1,
    after: SessionMetadataV1,
    command: SessionSelectModelCommand,
}

fn selection(before: Option<&str>, after: Option<&str>, settings: bool) -> Selection {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("store");
    let session = SessionId::new("catalog-order-session");
    let source = if settings {
        Derived
    } else {
        UserSet { requested: 4096 }
    };
    store
        .create_session(&SessionCreateCommand {
            command_id: "c6-create".into(),
            request_digest: "c6-create".into(),
            request_json: "{}".into(),
            session_id: session.clone(),
            cwd: "/tmp".into(),
            provider: "custom".into(),
            model: "shared-model".into(),
            resolved_model: None,
            model_catalog_key: before.map(str::to_owned),
            max_tokens: 4096,
            max_tokens_source: Some(source),
            permission_overrides: None,
            effort: None,
            fast: false,
            cache_policy: Default::default(),
            system_prompt_version: "test".into(),
            event_id: EventId::new("c6-created"),
            device_id: DeviceId::new("test"),
        })
        .expect("create");
    let before = store
        .session_metadata(&session)
        .expect("metadata")
        .expect("typed");
    let command = SessionSelectModelCommand {
        command_id: "c6-select".into(),
        request_digest: "c6-select".into(),
        request_json: "{}".into(),
        session_id: session.clone(),
        worker_generation: store.worker_generation(),
        provider: "custom".into(),
        model: "shared-model".into(),
        model_catalog_key: after.map(str::to_owned),
        expected_pair: None,
        automatic_run: None,
        account_alias: None,
        output_budget: Some(source.apply(4096, 4096)),
        event_id: EventId::new("c6-selected"),
        device_id: DeviceId::new("test"),
    };
    store.select_session_model(&command).expect("select");
    let after = store
        .session_metadata(&session)
        .expect("metadata")
        .expect("typed");
    Selection {
        _root: root,
        store,
        session,
        before,
        after,
        command,
    }
}

fn current(f: &Selection) -> (AppModel, LiveDriver) {
    let (mut model, mut driver, session) = catalog_order_setup();
    // Install only the real committed epoch, not setup's illustrative epoch 20.
    model.meter_epoch = Default::default();
    pass(
        &mut driver,
        &mut model,
        catalog_order_reply(
            &session,
            f.after.selection_epoch.expect("epoch"),
            f.after.model_catalog_key.as_deref(),
        ),
    );
    (model, driver)
}

fn old_selection(f: &Selection, model: &mut AppModel, driver: &mut LiveDriver) {
    pass(
        driver,
        model,
        catalog_order_reply(
            &f.session,
            f.before.selection_epoch.expect("epoch"),
            f.before.model_catalog_key.as_deref(),
        ),
    );
}

#[test]
fn c6_owner_replaced_and_durable_old_command_replay_cannot_erase_new_label() {
    let f = selection(
        Some("synthetic-producer-41"),
        Some("synthetic-producer-42"),
        false,
    );
    let (mut model, mut driver) = current(&f);
    let replay = f.store.select_session_model(&f.command).expect("replay");
    assert!(matches!(
        replay,
        SessionSelectModelOutcome::IdempotentReplay { .. }
    ));
    old_selection(&f, &mut model, &mut driver);
    assert_eq!(model.display_identity().model_short, "shared-model");
}

#[test]
fn c6_unowned_to_owned_delayed_reply_preserves_retirement_on_next_account() {
    let f = selection(None, Some("synthetic-producer-42"), false);
    let (mut model, mut driver) = current(&f);
    old_selection(&f, &mut model, &mut driver);
    let mut next = oauth_descriptor("custom", "work", true);
    next.incarnation = 43;
    pass(
        &mut driver,
        &mut model,
        LiveReply::Accounts {
            descriptors: vec![next],
            revision: Some(3),
            sources: vec![],
        },
    );
    pass(
        &mut driver,
        &mut model,
        LiveReply::Providers {
            providers: vec![private_summary(43, "different-model")],
            revision: 3,
        },
    );
    assert!(model.display_identity().model_short.is_empty());
}

#[test]
fn c6_owned_to_unowned_rejects_previous_owned_epoch() {
    let f = selection(Some("synthetic-producer-41"), None, false);
    let (mut model, mut driver) = current(&f);
    old_selection(&f, &mut model, &mut driver);
    assert_eq!(model.display_identity().model_short, "shared-model");
}

#[test]
fn c6_equal_settings_and_source_from_a_new_owner_advance_version() {
    for source in [Derived, UserSet { requested: 8192 }] {
        let mut f = selection(
            Some("synthetic-producer-41"),
            Some("synthetic-producer-41"),
            true,
        );
        f.command.command_id = format!("c6-settings-{source:?}");
        f.command.event_id = EventId::new("c6-settings");
        f.command.output_budget = Some(source.apply(4096, 4096));
        f.store
            .select_session_model(&f.command)
            .expect("initial settings");
        f.command.command_id.push_str("-new-owner");
        f.command.event_id = EventId::new("c6-new-settings-owner");
        f.command.model_catalog_key = Some("synthetic-producer-42".into());
        let before = f
            .store
            .session_metadata(&f.session)
            .expect("metadata")
            .expect("typed");
        f.store
            .select_session_model(&f.command)
            .expect("select settings");
        let after = f
            .store
            .session_metadata(&f.session)
            .expect("metadata")
            .expect("typed");
        assert!(after.selection_epoch > before.selection_epoch);
    }
}

#[test]
fn c6_normalized_selectors_share_the_canonical_owner_version_rule() {
    for spelling in [
        "SHARED_MODEL",
        "shared.model",
        "shared model",
        "shared\u{2003}model",
    ] {
        assert_eq!(
            haider_rpc::normalize_model_selector(spelling),
            haider_rpc::normalize_model_selector("shared-model")
        );
        let f = selection(
            Some("synthetic-producer-41"),
            Some("synthetic-producer-42"),
            false,
        );
        assert!(
            f.after.selection_epoch > f.before.selection_epoch,
            "{spelling}"
        );
    }
}

#[test]
fn c6_parked_session_ignores_the_previous_owner_on_reopen() {
    let f = selection(
        Some("synthetic-producer-41"),
        Some("synthetic-producer-42"),
        false,
    );
    let (mut model, mut driver) = current(&f);
    let other = SessionId::new("parked-other");
    model.upsert_live_session(&other);
    model.open_session(&other);
    old_selection(&f, &mut model, &mut driver);
    model.open_session(&f.session);
    assert_eq!(model.display_identity().model_short, "shared-model");
}

#[test]
fn c6_delayed_created_receipt_cannot_replace_the_selected_owner() {
    let f = selection(
        Some("synthetic-producer-41"),
        Some("synthetic-producer-42"),
        false,
    );
    let (mut model, mut driver) = current(&f);
    pass(
        &mut driver,
        &mut model,
        LiveReply::Created {
            command_id: haider_rpc::CommandId::new("old-create"),
            session: f.session.clone(),
            worker_generation: 1,
            cwd: "/tmp".into(),
            provider: "custom".into(),
            model: "shared-model".into(),
            model_catalog_key: f.before.model_catalog_key.clone(),
            metadata: Some(f.before),
        },
    );
    assert_eq!(model.display_identity().model_short, "shared-model");
}

#[test]
fn c6_delayed_session_list_cannot_replace_active_or_parked_owner() {
    for parked in [false, true] {
        let f = selection(
            Some("synthetic-producer-41"),
            Some("synthetic-producer-42"),
            false,
        );
        let (mut model, mut driver) = current(&f);
        if parked {
            let other = SessionId::new("list-other");
            model.upsert_live_session(&other);
            model.open_session(&other);
        }
        let summary = serde_json::from_value(serde_json::json!({
            "session_id":f.session, "head_seq":1, "worker_generation":1, "metadata":f.before
        }))
        .expect("summary");
        pass(
            &mut driver,
            &mut model,
            LiveReply::Listed {
                sessions: vec![summary],
                next_cursor: None,
            },
        );
        if parked {
            model.open_session(&f.session);
        }
        assert_eq!(model.display_identity().model_short, "shared-model");
    }
}

#[test]
fn c6_previous_owner_route_completion_cannot_append_a_current_fact() {
    let f = selection(
        Some("synthetic-producer-41"),
        Some("synthetic-producer-42"),
        false,
    );
    let (_, facts) = f
        .store
        .commit_resolved_route_fact(
            &f.session,
            "custom",
            "shared-model",
            f.before.selection_epoch.expect("epoch"),
            Some("account-a"),
            &DeviceId::new("test"),
        )
        .expect("route");
    assert!(facts.is_empty());
}

#[test]
fn c6_previous_owner_context_snapshot_is_classified_previous() {
    let f = selection(
        Some("synthetic-producer-41"),
        Some("synthetic-producer-42"),
        false,
    );
    let (model, _) = current(&f);
    let snapshot = haider_protocol::context::ContextFootprint {
        selection_epoch: f.before.selection_epoch,
        reserved_output_tokens: 4096,
        input_tokens: 0,
        output_tokens: 0,
        cached_input_tokens: 0,
        used_tokens: 0,
        context_window: Some(32000),
        soft_threshold_tokens: None,
        estimated_turns_to_threshold: None,
        truth: haider_protocol::context::ContextFootprintTruth::Estimated,
        accounting: None,
    };
    assert_eq!(
        model.meter_epoch.snapshot_epoch(Some(&snapshot)),
        haider_tui::context_meter::SnapshotEpoch::Previous
    );
}

#[test]
fn c6_true_no_ops_keep_the_epoch_and_equal_version_budget_can_complete() {
    for owner in [None, Some("synthetic-producer-42")] {
        let f = selection(owner, owner, false);
        assert_eq!(f.after.selection_epoch, f.before.selection_epoch);
        let (mut model, _) = current(&f);
        model.meter_epoch.reserve_assumed = true;
        model.meter_epoch.output_budget = None;
        assert!(model.meter_epoch.admit(
            ("custom".into(), "shared-model".into()),
            f.after.selection_epoch,
            Some(4096),
            Some(true)
        ));
        assert_eq!(model.meter_epoch.output_budget, Some(4096));
    }
}
