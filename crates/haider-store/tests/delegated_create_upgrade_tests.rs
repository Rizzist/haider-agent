#![allow(clippy::expect_used)]
//! Exact 8ffc5866 create schema, including the crash-before-establishment case.
use haider_protocol::ids::{DeviceId, EventId, SessionId};
use haider_protocol::session::{SessionInteractionModeV1, SessionPermissionOverridesV1};
use haider_store::{SessionCreateCommand, SessionProviderRebindCommand, Store};
use serde_json::{Value, json};

fn create_command(id: &str, body: &Value) -> SessionCreateCommand {
    let request_json = body.to_string();
    SessionCreateCommand {
        command_id: format!("delegation-session-{id}"),
        request_digest: blake3::hash(request_json.as_bytes()).to_hex().to_string(),
        request_json,
        session_id: SessionId::new(id),
        cwd: "/tmp".into(),
        provider: "bedrock".into(),
        model: "anthropic.claude-opus-5".into(),
        max_tokens: 4096,
        max_tokens_source: None,
        permission_overrides: None,
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        system_prompt_version: "test".into(),
        event_id: EventId::new(format!("created-{id}")),
        device_id: DeviceId::new("test"),
    }
}

fn old_body() -> Value {
    json!({"cwd":"/tmp", "provider":"bedrock", "model":"anthropic.claude-opus-5", "max_tokens":4096,
        "permission_overrides":SessionPermissionOverridesV1 { read_only:false, allow_writes:true,
            allow_exec:true, allow_mobile:false, auto_allow:false },
        "delegation_agent":"agent-synthetic", "effort":null, "fast":false,
        "cache_policy":haider_protocol::cache::CachePolicySettingsV1::default(), "interaction_mode":"interactive"})
}

fn replay(
    store: &Store,
    id: &str,
    body: &Value,
) -> haider_store::StoreResult<Option<haider_store::CreatedSession>> {
    let json = body.to_string();
    store.session_create_receipt(id, blake3::hash(json.as_bytes()).to_hex().as_ref(), &json)
}

fn upgrade_variant(pin: bool, endpoint: bool, autonomous: bool) {
    upgrade_variant_with_budget_source(pin, endpoint, autonomous, false);
}

fn upgrade_variant_with_budget_source(
    pin: bool,
    endpoint: bool,
    autonomous: bool,
    source_present: bool,
) {
    let root = tempfile::tempdir().expect("private store");
    let store = Store::open(root.path()).expect("store");
    let parent = create_command("parent", &json!({"parent":true}));
    store.create_session(&parent).expect("parent");
    let rebind = SessionProviderRebindCommand {
        command_id: "parent-route".into(),
        request_digest: "parent-route-digest".into(),
        request_json: "{}".into(),
        session_id: parent.session_id.clone(),
        worker_generation: store.worker_generation(),
        provider: "bedrock".into(),
        base_url: endpoint.then(|| "http://127.0.0.1:8000".into()),
        account: pin.then(|| "synthetic-pin".into()),
        event_id: EventId::new("parent-rebound"),
        device_id: DeviceId::new("test"),
        max_tokens: 4096,
        max_tokens_source: haider_protocol::output_budget::SessionOutputBudgetSourceV1::Derived,
        clamped: None,
        active_output_budget: None,
    };
    store
        .rebind_session_provider(&rebind)
        .expect("parent route");
    let mut old = old_body();
    if source_present {
        old["max_tokens_source"] = Value::Null;
    }
    if autonomous {
        old["interaction_mode"] = json!("autonomous");
    }
    let command = create_command("child", &old);
    let mode = if autonomous {
        SessionInteractionModeV1::Autonomous
    } else {
        SessionInteractionModeV1::Interactive
    };
    store
        .create_session_with_interaction_mode(&command, mode)
        .expect("old daemon child create");
    // No delegation row exists: the old daemon can crash right after create.
    let mut new = old.clone();
    if source_present {
        new["max_tokens_source"] =
            json!(haider_protocol::output_budget::SessionOutputBudgetSourceV1::Derived);
    }
    new["inheritance_parent_session_id"] = json!("parent");
    new["inherited_account_alias"] = json!(rebind.account);
    new["inherited_provider_base_url"] = json!(rebind.base_url);
    new["inherited_provider_rebind_id"] = json!("parent-route");
    let journal = store.journal_replay(&command.session_id).expect("journal");
    drop(store);
    let store = Store::open(root.path()).expect("restart / upgrade");
    assert!(
        replay(&store, &command.command_id, &old)
            .expect("old unchanged control")
            .is_some()
    );
    assert!(
        replay(&store, &command.command_id, &new)
            .expect("upgrade replay")
            .is_some()
    );
    assert_eq!(
        store
            .journal_replay(&command.session_id)
            .expect("unchanged journal"),
        journal
    );
    // Replaying the new schema cannot accidentally rewrite the old child's route.
    assert_eq!(
        store
            .session_metadata(&command.session_id)
            .expect("child")
            .expect("typed")
            .account_alias,
        None
    );
    for key in [
        "provider",
        "model",
        "cwd",
        "permission_overrides",
        "max_tokens",
        "max_tokens_source",
        "effort",
        "fast",
        "cache_policy",
        "interaction_mode",
        "delegation_agent",
        "inherited_account_alias",
        "inherited_provider_base_url",
        "inherited_provider_rebind_id",
        "inheritance_parent_session_id",
    ] {
        let mut changed = new.clone();
        changed[key] = json!("changed");
        assert!(
            replay(&store, &command.command_id, &changed).is_err(),
            "semantic change in {key}"
        );
    }
    let mut changed = new.clone();
    changed["max_tokens_source"] = json!({"kind":"user_set", "requested":4096});
    assert!(
        replay(&store, &command.command_id, &changed).is_err(),
        "a valid source with the same effective cap cannot change admitted intent"
    );
    // Today's changed parent cannot legitimise a changed old request.
    let mut later = rebind.clone();
    later.command_id = "later-route".into();
    later.event_id = EventId::new("later-route-event");
    later.request_digest = "later-digest".into();
    later.account = Some("later-pin".into());
    later.worker_generation = store.worker_generation();
    store
        .rebind_session_provider(&later)
        .expect("later parent change");
    assert!(
        replay(&store, &command.command_id, &new)
            .expect("original history still replays")
            .is_some()
    );
    new["inherited_account_alias"] = json!("later-pin");
    assert!(replay(&store, &command.command_id, &new).is_err());
}

#[test]
fn upgrade_unpinned_delegated_create() {
    upgrade_variant(false, false, false);
}
#[test]
fn upgrade_explicit_account_delegated_create() {
    upgrade_variant(true, false, false);
}
#[test]
fn upgrade_endpoint_only_delegated_create() {
    upgrade_variant(false, true, false);
}
#[test]
fn upgrade_pin_and_endpoint_delegated_create() {
    upgrade_variant(true, true, false);
}
#[test]
fn upgrade_public_spawn_recovery() {
    upgrade_variant(true, true, true);
}
#[test]
fn upgrade_account_pinned_at_parent_creation_uses_the_original_receipt() {
    let root = tempfile::tempdir().expect("store");
    let store = Store::open(root.path()).expect("open");
    let parent = create_command("creation-pin-parent", &json!({"parent":true}));
    store
        .create_session_with_configuration(
            &parent,
            SessionInteractionModeV1::Interactive,
            Some("creation-pin".into()),
        )
        .expect("pinned parent");
    let old = old_body();
    let child = create_command("creation-pin-child", &old);
    store.create_session(&child).expect("old child");
    let mut new = old;
    new["inheritance_parent_session_id"] = json!(parent.session_id);
    new["inherited_account_alias"] = json!("creation-pin");
    new["inherited_provider_base_url"] = Value::Null;
    new["inherited_provider_rebind_id"] = Value::Null;
    drop(store);
    let store = Store::open(root.path()).expect("upgrade");
    assert!(
        replay(&store, &child.command_id, &new)
            .expect("creation pin replay")
            .is_some()
    );
    new["inherited_account_alias"] = Value::Null;
    assert!(
        replay(&store, &child.command_id, &new).is_err(),
        "dropping the original pin is a semantic change"
    );
}
#[test]
fn upgrade_unpinned_receipt_without_parent_witness_and_new_receipts_are_strict() {
    let root = tempfile::tempdir().expect("private store");
    let store = Store::open(root.path()).expect("store");
    let old = old_body();
    let command = create_command("no-witness", &old);
    store.create_session(&command).expect("create");
    let mut new = old;
    for key in [
        "inherited_account_alias",
        "inherited_provider_base_url",
        "inherited_provider_rebind_id",
    ] {
        new[key] = Value::Null;
    }
    assert!(
        replay(&store, &command.command_id, &new)
            .expect("null defaults")
            .is_some()
    );
    new["inherited_account_alias"] = json!("unprovable-pin");
    assert!(replay(&store, &command.command_id, &new).is_err());
    let new_command = create_command("new-schema", &new);
    store.create_session(&new_command).expect("new create");
    new["inherited_account_alias"] = Value::Null;
    assert!(
        replay(&store, &new_command.command_id, &new).is_err(),
        "new-schema route changes remain strict"
    );
}

#[test]
fn upgrade_intermediate_route_digest_schema_accepts_only_the_added_witness() {
    for (pin, endpoint) in [(false, false), (true, false), (false, true), (true, true)] {
        let root = tempfile::tempdir().expect("private store");
        let store = Store::open(root.path()).expect("store");
        let mut old = old_body();
        old["inherited_account_alias"] = if pin {
            json!("synthetic-pin")
        } else {
            Value::Null
        };
        old["inherited_provider_base_url"] = if endpoint {
            json!("http://127.0.0.1:8010")
        } else {
            Value::Null
        };
        old["inherited_provider_rebind_id"] = if endpoint {
            json!("synthetic-route")
        } else {
            Value::Null
        };
        let command = create_command("intermediate", &old);
        store.create_session(&command).expect("6223198e receipt");
        let mut new = old;
        new["inheritance_parent_session_id"] = json!("parent-synthetic");
        drop(store);
        let store = Store::open(root.path()).expect("upgrade");
        assert!(
            replay(&store, &command.command_id, &new)
                .expect("added witness replay")
                .is_some()
        );
        for key in [
            "inherited_account_alias",
            "inherited_provider_base_url",
            "inherited_provider_rebind_id",
            "model",
        ] {
            let mut changed = new.clone();
            changed[key] = json!("changed");
            assert!(
                replay(&store, &command.command_id, &changed).is_err(),
                "intermediate schema keeps {key} strict"
            );
        }
    }
}

#[test]
fn upgrade_child_of_fork_uses_the_fork_receipt_before_its_copied_history() {
    use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
    use haider_protocol::ids::RunId;
    use haider_protocol::{DeliveryMode, EventPayload};
    use haider_store::{
        SessionForkCommand, SessionForkOutcome, TurnAcceptCommand, TurnAcceptOutcome,
    };
    let root = tempfile::tempdir().expect("private store");
    let store = Store::open(root.path()).expect("store");
    let source = create_command("source", &json!({"source":true}));
    store.create_session(&source).expect("source");
    let mut route = SessionProviderRebindCommand {
        command_id: "before-cut".into(),
        request_digest: "before-cut".into(),
        request_json: "{}".into(),
        session_id: source.session_id.clone(),
        worker_generation: store.worker_generation(),
        provider: "bedrock".into(),
        base_url: Some("http://127.0.0.1:8010".into()),
        account: Some("old-source-pin".into()),
        event_id: EventId::new("before-cut-event"),
        device_id: DeviceId::new("test"),
        max_tokens: 4096,
        max_tokens_source: haider_protocol::output_budget::SessionOutputBudgetSourceV1::Derived,
        clamped: None,
        active_output_budget: None,
    };
    store
        .rebind_session_provider(&route)
        .expect("copied old route");
    let run = RunId::new("source-turn");
    let TurnAcceptOutcome::Committed { envelopes, .. } = store
        .accept_turn(&TurnAcceptCommand {
            command_id: "source-turn".into(),
            request_digest: "source-turn".into(),
            request_json: "{}".into(),
            session_id: source.session_id.clone(),
            worker_generation: store.worker_generation(),
            run_id: run.clone(),
            agent_id: None,
            branch_id: None,
            text: "source prompt".into(),
            attachments: Vec::new(),
            mode: DeliveryMode::Queue,
            queued_event_id: EventId::new("source-queued"),
            user_event_id: EventId::new("source-user"),
            active_event_id: EventId::new("source-active"),
            device_id: DeviceId::new("test"),
        })
        .expect("turn")
    else {
        panic!("new turn must commit");
    };
    let (node, seq) = envelopes
        .iter()
        .find_map(|envelope| {
            let EventPayload::NodeCommitted(node) =
                serde_json::from_value(envelope.payload.clone().into()).ok()?
            else {
                return None;
            };
            Some((node.node, envelope.seq))
        })
        .expect("fork cut");
    let mut done = [EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new("source-done"),
        seq: 0,
        session_id: source.session_id.clone(),
        branch_id: None,
        run_id: Some(run),
        agent_id: None,
        device_id: DeviceId::new("test"),
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
        payload: serde_json::to_value(EventPayload::RunState(
            haider_protocol::state::RunState::Done,
        ))
        .expect("done payload")
        .into(),
    }];
    store.append_worker(&mut done).expect("finish source");
    route.command_id = "after-cut".into();
    route.request_digest = "after-cut".into();
    route.event_id = EventId::new("after-cut-event");
    route.account = Some("fork-pin".into());
    store
        .rebind_session_provider(&route)
        .expect("fork current route");
    let SessionForkOutcome::Committed { created, .. } = store
        .fork_session(&SessionForkCommand {
            command_id: "fork-parent".into(),
            request_digest: "fork-parent".into(),
            request_json: "{}".into(),
            source_session_id: source.session_id.clone(),
            session_id: SessionId::new("fork-parent"),
            worker_generation: store.worker_generation(),
            source_branch_id: None,
            fork_node_id: node,
            fork_seq: seq,
            name: None,
            metafork: None,
            audit_event_id: EventId::new("fork-audit"),
            device_id: DeviceId::new("test"),
        })
        .expect("fork parent")
    else {
        panic!("new fork must commit");
    };
    assert_eq!(created.metadata.account_alias.as_deref(), Some("fork-pin"));
    let old = old_body();
    let child = create_command("fork-grandchild", &old);
    store.create_session(&child).expect("legacy child");
    let mut new = old;
    new["inheritance_parent_session_id"] = json!(created.session_id);
    new["inherited_account_alias"] = json!(created.metadata.account_alias);
    new["inherited_provider_base_url"] = json!(created.metadata.provider_base_url);
    new["inherited_provider_rebind_id"] = json!(created.metadata.provider_rebind_id);
    drop(store);
    let store = Store::open(root.path()).expect("upgrade");
    assert!(
        replay(&store, &child.command_id, &new)
            .expect("fork receipt replay")
            .is_some()
    );
    new["inherited_account_alias"] = json!("old-source-pin");
    assert!(
        replay(&store, &child.command_id, &new).is_err(),
        "copied history cannot roll back fork inheritance"
    );
}

#[test]
fn upgrade_cross_provider_child_does_not_inherit_incompatible_parent_choices() {
    let root = tempfile::tempdir().expect("private store");
    let store = Store::open(root.path()).expect("store");
    let parent = create_command("cross-parent", &json!({"parent":true}));
    store.create_session(&parent).expect("parent");
    store
        .rebind_session_provider(&SessionProviderRebindCommand {
            command_id: "cross-parent-route".into(),
            request_digest: "cross-parent-route".into(),
            request_json: "{}".into(),
            session_id: parent.session_id.clone(),
            worker_generation: store.worker_generation(),
            provider: "bedrock".into(),
            base_url: Some("http://127.0.0.1:8010".into()),
            account: Some("bed-synthetic".into()),
            event_id: EventId::new("cross-parent-route-event"),
            device_id: DeviceId::new("test"),
            max_tokens: 4096,
            max_tokens_source: haider_protocol::output_budget::SessionOutputBudgetSourceV1::Derived,
            clamped: None,
            active_output_budget: None,
        })
        .expect("parent choices");
    let mut old = old_body();
    old["provider"] = json!("anthropic");
    old["model"] = json!("claude-sonnet-5");
    let mut child = create_command("cross-child", &old);
    child.provider = "anthropic".into();
    child.model = "claude-sonnet-5".into();
    store
        .create_session(&child)
        .expect("old cross-provider child");
    let mut new = old;
    new["inheritance_parent_session_id"] = json!(parent.session_id);
    for key in [
        "inherited_account_alias",
        "inherited_provider_base_url",
        "inherited_provider_rebind_id",
    ] {
        new[key] = Value::Null;
    }
    drop(store);
    let store = Store::open(root.path()).expect("upgrade");
    assert!(
        replay(&store, &child.command_id, &new)
            .expect("incompatible choices remain clear")
            .is_some()
    );
    for (key, value) in [
        ("inherited_account_alias", "bed-synthetic"),
        ("inherited_provider_base_url", "http://127.0.0.1:8010"),
        ("inherited_provider_rebind_id", "cross-parent-route"),
    ] {
        let mut changed = new.clone();
        changed[key] = json!(value);
        assert!(
            replay(&store, &child.command_id, &changed).is_err(),
            "incompatible {key} cannot become inherited"
        );
    }
}

#[test]
fn upgrade_without_parent_witness_matches_frozen_child_pin_and_rejects_its_clear() {
    let root = tempfile::tempdir().expect("private store");
    let store = Store::open(root.path()).expect("store");
    let old = old_body();
    let child = create_command("recorded-pin-child", &old);
    store
        .create_session_with_configuration(
            &child,
            SessionInteractionModeV1::Interactive,
            Some("recorded-pin".into()),
        )
        .expect("old pinned child");
    let mut new = old;
    new["inherited_account_alias"] = json!("recorded-pin");
    new["inherited_provider_base_url"] = Value::Null;
    new["inherited_provider_rebind_id"] = Value::Null;
    drop(store);
    let store = Store::open(root.path()).expect("upgrade");
    assert!(
        replay(&store, &child.command_id, &new)
            .expect("recorded choices replay")
            .is_some()
    );
    new["inherited_account_alias"] = Value::Null;
    assert!(
        replay(&store, &child.command_id, &new).is_err(),
        "null cannot clear the frozen pin"
    );
}

#[test]
fn upgrade_inheritance_uses_commit_order_across_clock_skew_and_rejects_late_parent() {
    let root = tempfile::tempdir().expect("private store");
    let store = Store::open(root.path()).expect("store");
    let parent = create_command("clock-parent", &json!({"parent":true}));
    store.create_session(&parent).expect("parent");
    let child = create_command("clock-child", &old_body());
    store.create_session(&child).expect("child");
    let raw = rusqlite::Connection::open(store.database_path()).expect("fixture journal");
    let response: String = raw
        .query_row(
            "SELECT response_json FROM command_receipts WHERE command_id = ?1",
            [&parent.command_id],
            |row| row.get(0),
        )
        .expect("frozen parent response");
    let mut response: Value = serde_json::from_str(&response).expect("response JSON");
    // A wall-clock correction can put an earlier commit's timestamp in the
    // future. Its durable journal order still precedes the child creation.
    let child_created_at_ms = store
        .session_metadata(&child.session_id)
        .expect("child metadata")
        .expect("typed child")
        .created_at_ms;
    response["metadata"]["created_at_ms"] = json!(child_created_at_ms + 60_000);
    raw.execute(
        "UPDATE command_receipts SET response_json = ?2 WHERE command_id = ?1",
        rusqlite::params![parent.command_id, response.to_string()],
    )
    .expect("clock-skew fixture");
    let mut new = old_body();
    new["inheritance_parent_session_id"] = json!(parent.session_id);
    for key in [
        "inherited_account_alias",
        "inherited_provider_base_url",
        "inherited_provider_rebind_id",
    ] {
        new[key] = Value::Null;
    }
    assert!(
        replay(&store, &child.command_id, &new)
            .expect("commit order proves old parent despite wall time")
            .is_some()
    );
    let late = create_command("late-parent", &json!({"parent":true}));
    store.create_session(&late).expect("later parent");
    new["inheritance_parent_session_id"] = json!(late.session_id);
    assert!(
        replay(&store, &child.command_id, &new).is_err(),
        "a parent committed after the original child cannot prove inheritance"
    );
}

#[test]
fn upgrade_equivalence_does_not_extend_malformed_delegation_receipts() {
    for intermediate in [false, true] {
        let root = tempfile::tempdir().expect("private store");
        let store = Store::open(root.path()).expect("store");
        let mut old = old_body();
        old["delegation_agent"] = Value::Null;
        let keys = [
            "inherited_account_alias",
            "inherited_provider_base_url",
            "inherited_provider_rebind_id",
        ];
        if intermediate {
            for key in keys {
                old[key] = Value::Null;
            }
        }
        let child = create_command("not-delegated", &old);
        store
            .create_session(&child)
            .expect("public Store seam records arbitrary request body");
        let mut new = old;
        for key in keys {
            new[key] = Value::Null;
        }
        new["inheritance_parent_session_id"] = json!("some-parent");
        assert!(
            replay(&store, &child.command_id, &new).is_err(),
            "only a recognized delegated schema gets the upgrade exception"
        );
    }
}

#[test]
fn upgrade_budget_source_proof_rejects_changed_request_hidden_by_the_same_cap() {
    use haider_protocol::output_budget::SessionOutputBudgetSourceV1;
    for intermediate in [false, true] {
        let root = tempfile::tempdir().expect("private store");
        let store = Store::open(root.path()).expect("store");
        let mut parent = create_command("budget-parent", &json!({"parent":true}));
        parent.max_tokens = 8192;
        parent.max_tokens_source = Some(SessionOutputBudgetSourceV1::UserSet { requested: 50_000 });
        store.create_session(&parent).expect("parent");
        let mut old = old_body();
        old["max_tokens"] = json!(8192);
        let keys = [
            "inherited_account_alias",
            "inherited_provider_base_url",
            "inherited_provider_rebind_id",
        ];
        if intermediate {
            for key in keys {
                old[key] = Value::Null;
            }
        }
        let mut child = create_command("budget-child", &old);
        child.max_tokens = 8192;
        child.max_tokens_source = Some(SessionOutputBudgetSourceV1::UserSet { requested: 50_000 });
        store.create_session(&child).expect("old capped child");
        let mut new = old;
        for key in keys {
            new[key] = Value::Null;
        }
        new["inheritance_parent_session_id"] = json!(parent.session_id);
        new["max_tokens_source"] =
            json!(SessionOutputBudgetSourceV1::UserSet { requested: 50_000 });
        assert!(
            replay(&store, &child.command_id, &new)
                .expect("same source enrichment")
                .is_some()
        );
        new["max_tokens_source"] =
            json!(SessionOutputBudgetSourceV1::UserSet { requested: 60_000 });
        assert!(
            replay(&store, &child.command_id, &new).is_err(),
            "effective cap stayed 8192, but requested source changed"
        );
        new["max_tokens_source"] = json!(SessionOutputBudgetSourceV1::Derived);
        assert!(
            replay(&store, &child.command_id, &new).is_err(),
            "derived cannot replace the recorded user request"
        );
    }
}

#[test]
fn upgrade_budget_source_enrichment_preserves_legacy_default_classification() {
    use haider_protocol::output_budget::SessionOutputBudgetSourceV1;
    let root = tempfile::tempdir().expect("private store");
    let store = Store::open(root.path()).expect("store");
    let child = create_command("derived-child", &old_body());
    store.create_session(&child).expect("old omitted source");
    let mut new = old_body();
    for key in [
        "inherited_account_alias",
        "inherited_provider_base_url",
        "inherited_provider_rebind_id",
    ] {
        new[key] = Value::Null;
    }
    new["max_tokens_source"] = json!(SessionOutputBudgetSourceV1::Derived);
    assert!(
        replay(&store, &child.command_id, &new)
            .expect("known old default is semantically derived")
            .is_some()
    );
    new["max_tokens_source"] = json!(SessionOutputBudgetSourceV1::UserSet { requested: 4096 });
    assert!(
        replay(&store, &child.command_id, &new).is_err(),
        "an explicit fixed budget changes the legacy derived source"
    );
}

/// Rebuild old picks with the old metadata rule, including creation pins that
/// have never been rebound. On provider changes, the current writer clears both.
#[test]
fn upgrade_cross_provider_pick_reconstructs_historical_creation_and_rebind_pins() {
    for (variant, picks) in [
        (
            "same-provider",
            &[("bedrock", "anthropic.claude-haiku-4-5")][..],
        ),
        ("cross-provider", &[("anthropic", "claude-sonnet-5")][..]),
        (
            "provider-a-b-a",
            &[
                ("anthropic", "claude-sonnet-5"),
                ("bedrock", "anthropic.claude-opus-5"),
            ][..],
        ),
    ] {
        for rebind in [false, true] {
            let root = tempfile::tempdir().expect("store");
            let store = Store::open(root.path()).expect("open");
            let parent = create_command("old-pick-parent", &json!({"parent":true}));
            store
                .create_session_with_configuration(
                    &parent,
                    SessionInteractionModeV1::Interactive,
                    Some("bed-b".into()),
                )
                .expect("creation pin");
            if rebind {
                store
                    .rebind_session_provider(&SessionProviderRebindCommand {
                        command_id: "old-rebind".into(),
                        request_digest: "old-rebind".into(),
                        request_json: "{}".into(),
                        session_id: parent.session_id.clone(),
                        worker_generation: store.worker_generation(),
                        provider: "bedrock".into(),
                        base_url: Some("http://127.0.0.1:8010".into()),
                        account: Some("bed-b".into()),
                        event_id: EventId::new("old-rebind-event"),
                        device_id: DeviceId::new("test"),
                        max_tokens: 4096,
                        max_tokens_source:
                            haider_protocol::output_budget::SessionOutputBudgetSourceV1::Derived,
                        clamped: None,
                        active_output_budget: None,
                    })
                    .expect("rebind pin and endpoint");
            }
            for (index, &(provider, model)) in picks.iter().enumerate() {
                legacy_pick(&store, &parent.session_id, index, provider, model);
            }
            // Both the events and metadata above use the pre-upgrade rule.
            drop(store);
            let store = Store::open(root.path()).expect("old writer reopened");
            let metadata = store
                .session_metadata(&parent.session_id)
                .expect("parent")
                .expect("typed");
            let same_provider = variant == "same-provider";
            let pin_kept = !rebind || same_provider;
            assert_eq!(
                metadata.account_alias.as_deref(),
                pin_kept.then_some("bed-b")
            );
            assert_eq!(
                metadata.provider_base_url.as_deref(),
                (rebind && same_provider).then_some("http://127.0.0.1:8010")
            );
            assert_eq!(
                metadata.provider_rebind_id.as_deref(),
                (rebind && same_provider).then_some("old-rebind")
            );
            let mut old = old_body();
            old["provider"] = json!(metadata.provider);
            old["model"] = json!(metadata.model);
            let mut child = create_command("old-pick-child", &old);
            child.provider = metadata.provider.clone();
            child.model = metadata.model.clone();
            store
                .create_session(&child)
                .expect("legacy child before establishment");
            let journal = store.journal_replay(&child.session_id).expect("journal");
            let mut new = old;
            new["max_tokens_source"] = Value::Null;
            new["inheritance_parent_session_id"] = json!(parent.session_id);
            new["inherited_account_alias"] = json!(metadata.account_alias);
            new["inherited_provider_base_url"] = json!(metadata.provider_base_url);
            new["inherited_provider_rebind_id"] = json!(metadata.provider_rebind_id);
            assert_eq!(new.as_object().expect("body").len(), 15);
            drop(store);
            let store = Store::open(root.path()).expect("upgrade restart");
            assert!(
                replay(&store, &child.command_id, &new)
                    .expect("unchanged replay")
                    .is_some()
            );
            assert_eq!(
                store
                    .journal_replay(&child.session_id)
                    .expect("unchanged journal"),
                journal
            );
            for (key, value) in [
                ("model", json!("claude-haiku-4-5")),
                ("max_tokens", json!(8192)),
                (
                    "inherited_account_alias",
                    if pin_kept {
                        Value::Null
                    } else {
                        json!("bed-b")
                    },
                ),
                (
                    "inherited_provider_base_url",
                    json!("http://127.0.0.1:9009"),
                ),
                ("inherited_provider_rebind_id", json!("changed-route")),
            ] {
                let mut changed = new.clone();
                changed[key] = value;
                assert!(
                    replay(&store, &child.command_id, &changed).is_err(),
                    "{variant}, rebind={rebind}: changed {key}"
                );
            }
        }
    }
}

/// Historical model picks had no pin-clear fields. Append that actual payload
/// and project the old writer's rule, rather than generating a candidate clear
/// event and merely restoring its metadata afterwards.
fn legacy_pick(store: &Store, session: &SessionId, index: usize, provider: &str, model: &str) {
    use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
    let mut metadata = store
        .session_metadata(session)
        .expect("metadata")
        .expect("session");
    if metadata.provider != provider {
        if metadata.provider_rebind_id.is_some() {
            metadata.account_alias = None;
        }
        metadata.provider_base_url = None;
        metadata.provider_rebind_id = None;
    }
    metadata.provider = provider.into();
    metadata.model = model.into();
    let mut events = [EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(format!("old-pick-{session}-{index}")),
        seq: 0,
        session_id: session.clone(),
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: DeviceId::new("test"),
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
        payload: json!({"type":"model_selected", "provider":provider, "model":model}).into(),
    }];
    store.append_worker(&mut events).expect("old pick event");
    rusqlite::Connection::open(store.database_path())
        .expect("historical fixture")
        .execute(
            "UPDATE sessions SET meta_json = ?2 WHERE id = ?1",
            rusqlite::params![
                session.as_str(),
                serde_json::to_string(&metadata).expect("metadata JSON")
            ],
        )
        .expect("old writer metadata");
}

fn candidate_pick(store: &Store, session: &SessionId, index: usize, provider: &str, model: &str) {
    store
        .select_session_model(&haider_store::SessionSelectModelCommand {
            command_id: format!("candidate-pick-{session}-{index}"),
            request_digest: format!("candidate-pick-{session}-{index}"),
            request_json: "{}".into(),
            session_id: session.clone(),
            worker_generation: store.worker_generation(),
            provider: provider.into(),
            model: model.into(),
            expected_pair: None,
            account_alias: None,
            output_budget: None,
            event_id: EventId::new(format!("candidate-picked-{session}-{index}")),
            device_id: DeviceId::new("test"),
        })
        .expect("candidate pick");
}

fn legacy_descendant(store: &Store, parent: &SessionId, id: &str) -> (SessionCreateCommand, Value) {
    let metadata = store
        .session_metadata(parent)
        .expect("metadata")
        .expect("parent");
    let mut old = old_body();
    old["provider"] = json!(metadata.provider);
    old["model"] = json!(metadata.model);
    let mut child = create_command(id, &old);
    child.provider = metadata.provider;
    child.model = metadata.model;
    store
        .create_session(&child)
        .expect("legacy ten-key descendant");
    assert!(
        replay(store, &child.command_id, &old)
            .expect("exact old request")
            .is_some()
    );
    let mut new = old;
    new["max_tokens_source"] = Value::Null;
    new["inheritance_parent_session_id"] = json!(parent);
    new["inherited_account_alias"] = json!(metadata.account_alias);
    new["inherited_provider_base_url"] = json!(metadata.provider_base_url);
    new["inherited_provider_rebind_id"] = json!(metadata.provider_rebind_id);
    (child, new)
}

fn assert_mixed_replay(store: &Store, child: &SessionCreateCommand, body: &Value) {
    let journal = store
        .journal_replay(&child.session_id)
        .expect("child journal");
    assert!(
        replay(store, &child.command_id, body)
            .expect("actual route replays")
            .is_some()
    );
    for (key, value) in [
        ("inherited_account_alias", json!("resurrected-pin")),
        (
            "inherited_provider_base_url",
            json!("http://127.0.0.1:9009"),
        ),
        ("inherited_provider_rebind_id", json!("invented-witness")),
        ("inheritance_parent_session_id", json!("invented-parent")),
        ("provider", json!("openai")),
        ("model", json!("changed-model")),
        ("max_tokens", json!(8192)),
    ] {
        let mut changed = body.clone();
        changed[key] = value;
        assert!(
            replay(store, &child.command_id, &changed).is_err(),
            "changed {key}"
        );
    }
    assert_eq!(
        store
            .journal_replay(&child.session_id)
            .expect("unchanged journal"),
        journal
    );
}

fn mixed_creation_pin(roundtrip: bool, delegated: bool) {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("store");
    let parent = create_command(
        "mixed-parent",
        &if delegated {
            old_body()
        } else {
            json!({"parent":true})
        },
    );
    if delegated {
        let source = create_command("mixed-root", &json!({"parent":true}));
        store
            .create_session_with_configuration(
                &source,
                SessionInteractionModeV1::Interactive,
                Some("resurrected-pin".into()),
            )
            .expect("delegation source pin");
        let route = store
            .session_metadata(&source.session_id)
            .expect("source metadata")
            .expect("source");
        store
            .create_session_with_workspace_configuration(
                &parent,
                SessionInteractionModeV1::Interactive,
                None,
                None,
                Some(route),
            )
            .expect("inherited delegated creation pin");
    } else {
        store
            .create_session_with_configuration(
                &parent,
                SessionInteractionModeV1::Interactive,
                Some("resurrected-pin".into()),
            )
            .expect("initial creation pin");
    }
    // A genuine old event keeps the creation pin; the following candidate
    // event records the clear. The descendant's old format distinguishes neither.
    legacy_pick(
        &store,
        &parent.session_id,
        0,
        "bedrock",
        "anthropic.claude-haiku-4-5",
    );
    candidate_pick(
        &store,
        &parent.session_id,
        1,
        "anthropic",
        "claude-sonnet-5",
    );
    if roundtrip {
        candidate_pick(
            &store,
            &parent.session_id,
            2,
            "bedrock",
            "anthropic.claude-opus-5",
        );
    }
    let (child, new) = legacy_descendant(&store, &parent.session_id, "mixed-child");
    assert_eq!(new["inherited_account_alias"], Value::Null);
    drop(store);
    let store = Store::open(root.path()).expect("candidate restart");
    if roundtrip {
        // Opening the store backfills this optional index. Clear it after
        // restart to exercise the fallback to each decoded event's evidence.
        let changed = rusqlite::Connection::open(store.database_path()).expect("legacy index fixture")
            .execute("UPDATE events SET payload_kind = NULL WHERE session_id = ?1 AND payload_kind = 'model_selected'",
                [parent.session_id.as_str()]).expect("unindexed parent facts");
        assert_eq!(changed, 3);
    }
    assert_mixed_replay(&store, &child, &new);
}

#[test]
fn mixed_creation_pin_clear_before_legacy_child() {
    mixed_creation_pin(false, false);
}
#[test]
fn mixed_creation_pin_clear_roundtrip_before_legacy_child() {
    mixed_creation_pin(true, false);
}
#[test]
fn mixed_delegated_parent_creation_pin_clear_before_legacy_descendant() {
    mixed_creation_pin(false, true);
}

fn mixed_fork_pin(metafork: bool) {
    use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
    use haider_protocol::ids::RunId;
    use haider_protocol::session_fork::{
        SessionMetaforkProposal, SessionMetaforkRemoval, SessionMetaforkReviewManifest,
    };
    use haider_protocol::{DeliveryMode, EventPayload};
    use haider_store::{
        SessionForkCommand, SessionForkOutcome, SessionMetaforkCommit, TurnAcceptCommand,
        TurnAcceptOutcome,
    };
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("store");
    let source = create_command("mixed-source", &json!({"source":true}));
    store
        .create_session_with_configuration(
            &source,
            SessionInteractionModeV1::Interactive,
            Some("resurrected-pin".into()),
        )
        .expect("source creation pin without rebind witness");
    let run = RunId::new("mixed-source-turn");
    let TurnAcceptOutcome::Committed { envelopes, .. } = store
        .accept_turn(&TurnAcceptCommand {
            command_id: "mixed-source-turn".into(),
            request_digest: "mixed-source-turn".into(),
            request_json: "{}".into(),
            session_id: source.session_id.clone(),
            worker_generation: store.worker_generation(),
            run_id: run.clone(),
            agent_id: None,
            branch_id: None,
            text: "fork source prompt".into(),
            attachments: Vec::new(),
            mode: DeliveryMode::Queue,
            queued_event_id: EventId::new("mixed-queued"),
            user_event_id: EventId::new("mixed-user"),
            active_event_id: EventId::new("mixed-active"),
            device_id: DeviceId::new("test"),
        })
        .expect("source turn")
    else {
        panic!("new turn commits");
    };
    let user_seq = envelopes
        .iter()
        .find(|event| {
            matches!(
                serde_json::from_value::<EventPayload>(event.payload.clone().into()),
                Ok(EventPayload::UserMessage { .. })
            )
        })
        .expect("user prompt coordinate")
        .seq;
    let (node, seq) = envelopes
        .iter()
        .find_map(|event| {
            let EventPayload::NodeCommitted(node) =
                serde_json::from_value(event.payload.clone().into()).ok()?
            else {
                return None;
            };
            Some((node.node, event.seq))
        })
        .expect("fork cutoff");
    let mut done = [EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new("mixed-done"),
        seq: 0,
        session_id: source.session_id.clone(),
        branch_id: None,
        run_id: Some(run),
        agent_id: None,
        device_id: DeviceId::new("test"),
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
        payload: serde_json::to_value(EventPayload::RunState(
            haider_protocol::state::RunState::Done,
        ))
        .expect("done payload")
        .into(),
    }];
    store.append_worker(&mut done).expect("finish source turn");
    let mut command = SessionForkCommand {
        command_id: "mixed-fork".into(),
        request_digest: "mixed-fork".into(),
        request_json: "{}".into(),
        source_session_id: source.session_id.clone(),
        session_id: SessionId::new("mixed-fork"),
        worker_generation: store.worker_generation(),
        source_branch_id: None,
        fork_node_id: node,
        fork_seq: seq,
        name: None,
        metafork: metafork.then(|| SessionMetaforkCommit {
            description: "omit reviewed source prompt".into(),
            model_proposal: SessionMetaforkProposal {
                removals: vec![SessionMetaforkRemoval {
                    from_seq: user_seq,
                    through_seq: user_seq,
                    reason: "omit source prompt".into(),
                    preview: None,
                    reviewed_events: Vec::new(),
                }],
            },
            accepted_proposal_digest: String::new(),
        }),
        audit_event_id: EventId::new("mixed-fork-audit"),
        device_id: DeviceId::new("test"),
    };
    if let Some(meta) = &mut command.metafork {
        meta.accepted_proposal_digest = SessionMetaforkReviewManifest {
            command_id: command.command_id.clone(),
            source_session_id: command.source_session_id.clone(),
            worker_generation: command.worker_generation,
            source_branch_id: None,
            fork_node_id: command.fork_node_id.clone(),
            fork_seq: command.fork_seq,
            name: None,
            description: meta.description.clone(),
            model_proposal: meta.model_proposal.clone(),
        }
        .digest()
        .expect("accepted review digest");
    }
    let SessionForkOutcome::Committed { created, .. } = store.fork_session(&command).expect("fork")
    else {
        panic!("new fork commits");
    };
    assert_eq!(
        created.metadata.account_alias.as_deref(),
        Some("resurrected-pin")
    );
    assert_eq!(created.metadata.provider_rebind_id, None);
    candidate_pick(
        &store,
        &created.session_id,
        1,
        "anthropic",
        "claude-sonnet-5",
    );
    let (child, new) = legacy_descendant(&store, &created.session_id, "mixed-fork-child");
    drop(store);
    let store = Store::open(root.path()).expect("candidate restart");
    assert_mixed_replay(&store, &child, &new);
}

#[test]
fn mixed_fork_creation_pin_clear_before_legacy_descendant() {
    mixed_fork_pin(false);
}
#[test]
fn mixed_metafork_creation_pin_clear_before_legacy_descendant() {
    mixed_fork_pin(true);
}

#[test]
fn mixed_writer_cycles_preserve_each_child_cutoff_and_later_rebind() {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("store");
    let parent = create_command("cycles-parent", &json!({"parent":true}));
    store
        .create_session_with_configuration(
            &parent,
            SessionInteractionModeV1::Interactive,
            Some("resurrected-pin".into()),
        )
        .expect("creation pin");
    let mut children = Vec::new();
    // The old-only A→B→A control must keep the initial pin at its exact cutoff.
    legacy_pick(
        &store,
        &parent.session_id,
        0,
        "anthropic",
        "claude-sonnet-5",
    );
    legacy_pick(
        &store,
        &parent.session_id,
        1,
        "bedrock",
        "anthropic.claude-opus-5",
    );
    children.push(legacy_descendant(
        &store,
        &parent.session_id,
        "cycles-old-child",
    ));
    for cycle in 0..3 {
        candidate_pick(
            &store,
            &parent.session_id,
            cycle * 4,
            "anthropic",
            "claude-sonnet-5",
        );
        candidate_pick(
            &store,
            &parent.session_id,
            cycle * 4 + 1,
            "anthropic",
            "claude-haiku-4-5",
        );
        let metadata = store
            .session_metadata(&parent.session_id)
            .expect("metadata")
            .expect("parent");
        store
            .commit_resolved_route(
                &parent.session_id,
                &metadata.provider,
                &metadata.model,
                metadata.selection_epoch.expect("epoch"),
                None,
                &DeviceId::new("test"),
            )
            .expect("route-only fact");
        legacy_pick(
            &store,
            &parent.session_id,
            cycle * 4 + 2,
            "bedrock",
            "anthropic.claude-opus-5",
        );
        children.push(legacy_descendant(
            &store,
            &parent.session_id,
            &format!("cycles-child-{cycle}"),
        ));
    }
    store
        .rebind_session_provider(&SessionProviderRebindCommand {
            command_id: "cycles-later-rebind".into(),
            request_digest: "cycles-later-rebind".into(),
            request_json: "{}".into(),
            session_id: parent.session_id.clone(),
            worker_generation: store.worker_generation(),
            provider: "bedrock".into(),
            base_url: Some("http://127.0.0.1:8010".into()),
            account: Some("later-pin".into()),
            event_id: EventId::new("cycles-rebound"),
            device_id: DeviceId::new("test"),
            max_tokens: 4096,
            max_tokens_source: haider_protocol::output_budget::SessionOutputBudgetSourceV1::Derived,
            clamped: None,
            active_output_budget: None,
        })
        .expect("later explicit route");
    children.push(legacy_descendant(
        &store,
        &parent.session_id,
        "cycles-rebound-child",
    ));
    candidate_pick(
        &store,
        &parent.session_id,
        99,
        "anthropic",
        "claude-sonnet-5",
    );
    children.push(legacy_descendant(
        &store,
        &parent.session_id,
        "cycles-cleared-child",
    ));
    drop(store);
    for _ in 0..2 {
        let store = Store::open(root.path()).expect("candidate restart");
        for (child, body) in &children {
            let journal = store.journal_replay(&child.session_id).expect("journal");
            assert!(
                replay(&store, &child.command_id, body)
                    .expect("exact historical cutoff")
                    .is_some()
            );
            let mut wrong = body.clone();
            wrong["inherited_account_alias"] = if body["inherited_account_alias"].is_null() {
                json!("resurrected-pin")
            } else {
                Value::Null
            };
            assert!(
                replay(&store, &child.command_id, &wrong).is_err(),
                "no alternate alias accepted"
            );
            assert_eq!(
                store.journal_replay(&child.session_id).expect("unchanged"),
                journal
            );
        }
    }
}

#[test]
fn pre_route_digest_source_present_inherits_the_historical_route_and_budget_intent() {
    upgrade_variant_with_budget_source(true, true, false, true);
    upgrade_variant_with_budget_source(false, false, true, true);
}

#[test]
fn current_receipt_binds_budget_source_and_every_route_field_strictly() {
    let root = tempfile::tempdir().expect("private store");
    let store = Store::open(root.path()).expect("store");
    let mut body = old_body();
    body["max_tokens_source"] = json!({"kind":"derived"});
    body["inheritance_parent_session_id"] = json!("parent");
    for key in [
        "inherited_account_alias",
        "inherited_provider_base_url",
        "inherited_provider_rebind_id",
    ] {
        body[key] = Value::Null;
    }
    let command = create_command("current-budget-route", &body);
    store.create_session(&command).expect("create");
    assert!(
        replay(&store, &command.command_id, &body)
            .expect("exact control")
            .is_some()
    );
    for key in [
        "max_tokens_source",
        "inheritance_parent_session_id",
        "inherited_account_alias",
        "inherited_provider_base_url",
        "inherited_provider_rebind_id",
    ] {
        let mut changed = body.clone();
        changed[key] = if key == "max_tokens_source" {
            json!({"kind":"user_set", "requested":4096})
        } else {
            json!("changed")
        };
        assert!(
            replay(&store, &command.command_id, &changed).is_err(),
            "current {key} stays strict"
        );
    }
}

#[test]
fn unknown_receipt_schema_cannot_use_source_only_upgrade_equivalence() {
    let root = tempfile::tempdir().expect("private store");
    let store = Store::open(root.path()).expect("store");
    let mut old = old_body();
    old["future_semantics"] = json!(true);
    let command = create_command("unknown-schema", &old);
    store.create_session(&command).expect("create");
    assert!(
        replay(&store, &command.command_id, &old)
            .expect("exact control")
            .is_some()
    );
    let mut new = old;
    new["max_tokens_source"] = json!({"kind":"derived"});
    assert!(
        replay(&store, &command.command_id, &new).is_err(),
        "only exact historical schemas allow source addition"
    );
}

#[test]
fn frozen_child_route_precedes_next_turn_parent_route_on_legacy_replay() {
    for (pin, endpoint) in [(true, false), (false, true), (true, true)] {
        let root = tempfile::tempdir().expect("profile");
        let store = Store::open(root.path()).expect("store");
        let parent = create_command("frozen-parent", &json!({"parent":true}));
        store.create_session(&parent).expect("parent");
        let route = SessionProviderRebindCommand {
            command_id: "frozen-route".into(),
            request_digest: "frozen-route".into(),
            request_json: "{}".into(),
            session_id: parent.session_id.clone(),
            worker_generation: store.worker_generation(),
            provider: "bedrock".into(),
            base_url: endpoint.then(|| "http://127.0.0.1:8020".into()),
            account: pin.then(|| "frozen-pin".into()),
            max_tokens: 4096,
            max_tokens_source: haider_protocol::output_budget::SessionOutputBudgetSourceV1::Derived,
            clamped: None,
            active_output_budget: None,
            event_id: EventId::new("frozen-route"),
            device_id: DeviceId::new("test"),
        };
        store
            .rebind_session_provider(&route)
            .expect("original route");
        let admitted = store
            .session_metadata(&parent.session_id)
            .expect("parent")
            .expect("typed");
        store
            .select_session_model(&haider_store::SessionSelectModelCommand {
                command_id: "next-parent".into(),
                request_digest: "next-parent".into(),
                request_json: "{}".into(),
                session_id: parent.session_id.clone(),
                worker_generation: store.worker_generation(),
                provider: "anthropic".into(),
                model: "claude-sonnet-5".into(),
                expected_pair: None,
                account_alias: None,
                output_budget: None,
                event_id: EventId::new("next-parent"),
                device_id: DeviceId::new("test"),
            })
            .expect("next-turn selection");
        let old = old_body();
        let child = create_command("frozen-child", &old);
        store
            .create_session_with_workspace_configuration(
                &child,
                SessionInteractionModeV1::Interactive,
                None,
                None,
                Some(admitted),
            )
            .expect("child admitted from the running route");
        let before = store
            .session_metadata(&child.session_id)
            .expect("child")
            .expect("typed");
        assert_eq!(before.account_alias, route.account);
        assert_eq!(before.provider_base_url, route.base_url);
        let journal = store.journal_replay(&child.session_id).expect("journal");
        let mut new = old.clone();
        new["max_tokens_source"] = json!({"kind":"derived"});
        new["inheritance_parent_session_id"] = json!(parent.session_id);
        new["inherited_account_alias"] = json!(route.account);
        new["inherited_provider_base_url"] = json!(route.base_url);
        new["inherited_provider_rebind_id"] = json!(route.command_id);
        drop(store);
        let store = Store::open(root.path()).expect("restart");
        assert!(
            replay(&store, &child.command_id, &old)
                .expect("original control")
                .is_some()
        );
        assert!(
            replay(&store, &child.command_id, &new)
                .expect("frozen route replay")
                .is_some()
        );
        assert_eq!(
            store
                .session_metadata(&child.session_id)
                .expect("child")
                .expect("typed"),
            before
        );
        assert_eq!(
            store.journal_replay(&child.session_id).expect("journal"),
            journal
        );
        for key in [
            "inherited_account_alias",
            "inherited_provider_base_url",
            "inherited_provider_rebind_id",
            "inheritance_parent_session_id",
            "max_tokens",
            "max_tokens_source",
        ] {
            let mut changed = new.clone();
            changed[key] = match key {
                "max_tokens" => json!(8192),
                "max_tokens_source" => json!({"kind":"user_set", "requested":4096}),
                _ => json!("changed"),
            };
            assert!(
                replay(&store, &child.command_id, &changed).is_err(),
                "frozen {key} stays strict"
            );
        }
        let mut discarded = new.clone();
        for key in [
            "inherited_account_alias",
            "inherited_provider_base_url",
            "inherited_provider_rebind_id",
        ] {
            discarded[key] = Value::Null;
        }
        assert!(
            replay(&store, &child.command_id, &discarded).is_err(),
            "next-turn route cannot discard admission"
        );
    }
}

#[test]
fn legacy_unrecorded_child_route_uses_running_parent_instead_of_next_turn() {
    use haider_protocol::{
        EventPayload,
        envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION},
        ids::{AgentId, ItemId, LeaseId, RunId},
        item::{ItemEvent, TurnItem},
        state::RunState,
    };
    fn append(store: &Store, parent: &SessionId, run: &RunId, label: &str, payload: EventPayload) {
        let mut events = [EventEnvelope {
            schema_version: SCHEMA_VERSION,
            event_id: EventId::new(label),
            seq: 0,
            session_id: parent.clone(),
            branch_id: None,
            run_id: Some(run.clone()),
            agent_id: None,
            device_id: DeviceId::new("test"),
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
            payload: serde_json::to_value(payload).expect("payload").into(),
        }];
        store.append_worker(&mut events).expect("parent run event");
    }
    fn accept(store: &Store, parent: &SessionId, run: &RunId) {
        store
            .accept_turn(&haider_store::TurnAcceptCommand {
                command_id: format!("turn-{run}"),
                request_digest: format!("turn-{run}"),
                request_json: "{}".into(),
                session_id: parent.clone(),
                worker_generation: store.worker_generation(),
                run_id: run.clone(),
                agent_id: None,
                branch_id: None,
                text: "parent turn".into(),
                attachments: Vec::new(),
                mode: haider_protocol::DeliveryMode::Queue,
                queued_event_id: EventId::new(format!("queued-{run}")),
                user_event_id: EventId::new(format!("user-{run}")),
                active_event_id: EventId::new(format!("active-{run}")),
                device_id: DeviceId::new("test"),
            })
            .expect("accept parent run");
    }
    for case in [
        "pinned",
        "legacy-rebound",
        "same-provider-auto",
        "cross-provider-auto",
        "other-run-auto",
        "queued-only",
        "completed",
        "linked-original",
        "linked-queued",
        "ambiguous",
        "future-rebound",
    ] {
        for (pin, endpoint) in [(true, false), (false, true), (true, true)] {
            for source_present in [false, true] {
                let root = tempfile::tempdir().expect("profile");
                let store = Store::open(root.path()).expect("store");
                let parent = create_command("running-parent", &json!({"parent": true}));
                store.create_session(&parent).expect("parent");
                let mut binding = SessionProviderRebindCommand {
                    command_id: "running-route".into(),
                    request_digest: "running-route".into(),
                    request_json: "{}".into(),
                    session_id: parent.session_id.clone(),
                    worker_generation: store.worker_generation(),
                    provider: "bedrock".into(),
                    base_url: endpoint.then(|| "http://127.0.0.1:8020".into()),
                    account: pin.then(|| "running-pin".into()),
                    max_tokens: 4096,
                    max_tokens_source:
                        haider_protocol::output_budget::SessionOutputBudgetSourceV1::UserSet {
                            requested: 4096,
                        },
                    clamped: None,
                    active_output_budget: None,
                    event_id: EventId::new("running-route"),
                    device_id: DeviceId::new("test"),
                };
                store
                    .rebind_session_provider(&binding)
                    .expect("original route");
                let admitted = store
                    .session_metadata(&parent.session_id)
                    .expect("parent")
                    .expect("typed");
                let run = RunId::new("original-parent-run");
                accept(&store, &parent.session_id, &run);
                if !matches!(case, "legacy-rebound" | "linked-queued") {
                    let pair = haider_protocol::session::RunOutputPairPinnedV1 {
                        provider: admitted.provider.clone(),
                        model: admitted.model.clone(),
                        max_tokens: admitted.max_tokens,
                        source:
                            haider_protocol::output_budget::SessionOutputBudgetSourceV1::UserSet {
                                requested: 4096,
                            },
                        provider_route: haider_protocol::session::RunOutputProviderRouteV1::capture(
                            &admitted,
                        ),
                    };
                    append(
                        &store,
                        &parent.session_id,
                        &run,
                        "parent-pin",
                        pair.to_event(&run).expect("pin"),
                    );
                }
                if !matches!(case, "queued-only" | "linked-queued") {
                    append(
                        &store,
                        &parent.session_id,
                        &run,
                        "parent-thinking",
                        EventPayload::RunState(RunState::Thinking),
                    );
                }
                if case == "legacy-rebound" {
                    binding.command_id = "after-start-route".into();
                    binding.request_digest = "after-start-route".into();
                    binding.event_id = EventId::new("after-start-route");
                    store
                        .rebind_session_provider(&binding)
                        .expect("legacy active rebind");
                }
                if case.ends_with("-auto") {
                    let scope = if case == "other-run-auto" {
                        let other = RunId::new("other-queued-run");
                        accept(&store, &parent.session_id, &other);
                        other
                    } else {
                        run.clone()
                    };
                    append(
                        &store,
                        &parent.session_id,
                        &scope,
                        "automatic-switch",
                        EventPayload::Item(ItemEvent::Completed {
                            item_id: ItemId::new("automatic-switch"),
                            item: TurnItem::Extension {
                                kind: "provider_pair_switch_v1".into(),
                                data: json!({"to_provider": if case == "same-provider-auto" { "bedrock" } else { "anthropic" }, "to_model": "automatic-model"}),
                            },
                        }),
                    );
                }
                if matches!(case, "completed" | "linked-original") {
                    append(
                        &store,
                        &parent.session_id,
                        &run,
                        "parent-done",
                        EventPayload::RunState(RunState::Done),
                    );
                }
                store
                    .select_session_model(&haider_store::SessionSelectModelCommand {
                        command_id: "next-parent".into(),
                        request_digest: "next-parent".into(),
                        request_json: "{}".into(),
                        session_id: parent.session_id.clone(),
                        worker_generation: store.worker_generation(),
                        provider: "anthropic".into(),
                        model: "claude-sonnet-5".into(),
                        expected_pair: None,
                        account_alias: None,
                        output_budget: None,
                        event_id: EventId::new("next-parent"),
                        device_id: DeviceId::new("test"),
                    })
                    .expect("next-turn selection");
                if case == "linked-queued" {
                    binding.command_id = "queued-current-route".into();
                    binding.request_digest = "queued-current-route".into();
                    binding.event_id = EventId::new("queued-current-route");
                    binding.provider = "anthropic".into();
                    binding.account = pin.then(|| "queued-current-pin".into());
                    binding.base_url = endpoint.then(|| "http://127.0.0.1:8022".into());
                    store
                        .rebind_session_provider(&binding)
                        .expect("queued parent uses the current historical session route");
                }
                if matches!(case, "linked-original" | "ambiguous") {
                    let next = RunId::new("next-parent-run");
                    accept(&store, &parent.session_id, &next);
                    append(
                        &store,
                        &parent.session_id,
                        &next,
                        "next-thinking",
                        EventPayload::RunState(RunState::Thinking),
                    );
                }
                let mut old = old_body();
                if source_present {
                    old["max_tokens_source"] = json!({"kind": "user_set", "requested": 4096});
                }
                if matches!(case, "cross-provider-auto" | "linked-queued") {
                    old["provider"] = json!("anthropic");
                    old["model"] = json!("claude-sonnet-5");
                }
                let mut child = create_command("unrecorded-child", &old);
                if matches!(case, "cross-provider-auto" | "linked-queued") {
                    child.provider = "anthropic".into();
                    child.model = "claude-sonnet-5".into();
                }
                child.max_tokens_source = Some(
                    haider_protocol::output_budget::SessionOutputBudgetSourceV1::UserSet {
                        requested: 4096,
                    },
                );
                store
                    .create_session_with_interaction_mode(
                        &child,
                        SessionInteractionModeV1::Interactive,
                    )
                    .expect("original legacy child");
                let before = store
                    .session_metadata(&child.session_id)
                    .expect("child")
                    .expect("typed");
                assert_eq!(before.account_alias, None);
                assert_eq!(before.provider_base_url, None);
                if matches!(case, "linked-original" | "linked-queued") {
                    use haider_protocol::agent::{AgentManifest, AgentRole, Grant, Placement};
                    let agent = AgentId::new("agent-synthetic");
                    store
                        .create_delegation(&haider_store::DelegationRecord {
                            agent_id: agent.clone(),
                            child_session_id: child.session_id.clone(),
                            child_run_id: RunId::new("child-run"),
                            parent_session_id: parent.session_id.clone(),
                            parent_run_id: run.clone(),
                            parent_branch_id: None,
                            call_id: "child-call".into(),
                            tool_item_id: ItemId::new("child-call"),
                            parent_agent_id: None,
                            root_session_id: parent.session_id.clone(),
                            depth: 1,
                            task: "test route inheritance".into(),
                            prompt: "test route inheritance".into(),
                            manifest: AgentManifest {
                                agent,
                                role: AgentRole::Subagent,
                                task: "test route inheritance".into(),
                                callsign: None,
                                model_profile: "anthropic.claude-opus-5".into(),
                                grant: Grant {
                                    tools: Vec::new(),
                                    effect_ceiling: Vec::new(),
                                },
                                budget_tokens: Some(4096),
                                placement: Placement::Local,
                                lease: LeaseId::new("child-lease"),
                                fencing_epoch: 1,
                                attempt: 0,
                                parent: None,
                                coordinates: None,
                                cli_scope: None,
                            },
                            state: haider_store::DelegationState::Spawned,
                            report: None,
                        })
                        .expect("original durable delegation witness");
                }
                if case == "future-rebound" {
                    let mut future = binding.clone();
                    future.command_id = "future-route".into();
                    future.request_digest = "future-route".into();
                    future.event_id = EventId::new("future-route");
                    future.account = Some("future-pin".into());
                    future.base_url = Some("http://127.0.0.1:8021".into());
                    store
                        .rebind_session_provider(&future)
                        .expect("later route cannot change the original cutoff");
                }
                let journal = store.journal_replay(&child.session_id).expect("journal");
                let inherits = !matches!(case, "cross-provider-auto" | "queued-only" | "completed");
                let mut new = old.clone();
                new["max_tokens_source"] = json!({"kind": "user_set", "requested": 4096});
                new["inheritance_parent_session_id"] = json!(parent.session_id);
                new["inherited_account_alias"] = if inherits {
                    json!(binding.account)
                } else {
                    Value::Null
                };
                new["inherited_provider_base_url"] = if inherits {
                    json!(binding.base_url)
                } else {
                    Value::Null
                };
                new["inherited_provider_rebind_id"] = if inherits {
                    json!(binding.command_id)
                } else {
                    Value::Null
                };
                drop(store);
                let store = Store::open(root.path()).expect("restart");
                assert!(
                    replay(&store, &child.command_id, &old)
                        .expect("original control")
                        .is_some()
                );
                let (_, admission) = store
                    .child_create_admission(
                        &child.command_id,
                        &child.session_id,
                        &parent.session_id,
                    )
                    .expect("original admission proof")
                    .expect("receipt");
                if case != "ambiguous" {
                    assert_eq!(
                        serde_json::to_value(admission.account_alias).expect("account_alias JSON"),
                        new["inherited_account_alias"],
                        "{case}: admitted alias"
                    );
                    assert_eq!(
                        serde_json::to_value(admission.provider_base_url)
                            .expect("provider_base_url JSON"),
                        new["inherited_provider_base_url"],
                        "{case}: admitted endpoint"
                    );
                    assert_eq!(
                        serde_json::to_value(admission.provider_rebind_id)
                            .expect("provider_rebind_id JSON"),
                        new["inherited_provider_rebind_id"],
                        "{case}: admitted rebind"
                    );
                } else {
                    assert_eq!(admission, before, "ambiguous history supplies no route");
                }
                if case == "ambiguous" {
                    assert!(
                        replay(&store, &child.command_id, &new).is_err(),
                        "no unique run witness"
                    );
                } else {
                    assert!(replay(&store, &child.command_id, &new).unwrap_or_else(|error| panic!("{case} pin={pin} endpoint={endpoint} source={source_present}: {error:?}")).is_some());
                    for field in [
                        "inherited_account_alias",
                        "inherited_provider_base_url",
                        "inherited_provider_rebind_id",
                    ] {
                        let mut changed = new.clone();
                        changed[field] = json!("changed-choice");
                        assert!(
                            replay(&store, &child.command_id, &changed).is_err(),
                            "{case}: {field}"
                        );
                    }
                    let mut changed = new;
                    changed["max_tokens_source"] = json!({"kind": "user_set", "requested": 8192});
                    assert!(
                        replay(&store, &child.command_id, &changed).is_err(),
                        "budget intent stays strict"
                    );
                }
                assert_eq!(
                    store
                        .session_metadata(&child.session_id)
                        .expect("child")
                        .expect("typed"),
                    before
                );
                assert_eq!(
                    store.journal_replay(&child.session_id).expect("journal"),
                    journal
                );
            }
        }
    }
}
