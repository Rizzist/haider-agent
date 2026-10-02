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
    };
    store
        .rebind_session_provider(&rebind)
        .expect("parent route");
    let mut old = old_body();
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
