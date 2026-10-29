#![allow(clippy::expect_used)]

use super::*;
use crate::worker::provider_rebind::tests::{
    RecordingRebindFactory, change_rotation_test_session, commit_test_rebind,
    create_rebind_test_session, rebound_recovery_headless_spec, select_rotation_test_model,
    start_legacy_rebind_test_run,
};

#[path = "run_route_legacy_fixtures.rs"]
mod legacy_fixtures;
use legacy_fixtures::*;

// Astra 29 independent probe: a pre-pin journal's route was chosen before
// the legacy run started, then a selection for the next turn releases it.
async fn recovery_matrix(bindings: &[&str], variants: &[&str]) {
    let mut failures = Vec::new();
    for &binding in bindings {
        for durable in [false, true] {
            for &variant in variants {
                let root = tempfile::tempdir().expect("private profile");
                let (store, hub) = crate::session_hub::open_retention_test_hub(root.path())
                    .await
                    .expect("hub");
                let mut session =
                    SessionId::new(format!("o29-legacy-{binding}-{durable}-{variant}"));
                let run = RunId::new("rebind-test-run");
                if binding.starts_with("child") {
                    create_child_route_parent(&hub, &session, root.path()).await;
                } else {
                    create_rebind_test_session(&hub, &session).await;
                }
                if binding.starts_with("opaque_") {
                    setup_opaque_selection(&hub, &session, store.worker_generation(), binding)
                        .await;
                } else if matches!(
                    binding,
                    "rebind"
                        | "resolved"
                        | "create"
                        | "fork"
                        | "metafork"
                        | "automatic_switch"
                        | "headless_automatic_switch"
                        | "child"
                        | "child_delayed"
                        | "resolved_create"
                        | "resolved_fork"
                        | "resolved_metafork"
                        | "endpoint_only"
                        | "checkpoint_rebind"
                        | "checkpoint_resolved"
                        | "legacy_observed_create"
                ) {
                    commit_test_rebind(&hub, &session, store.worker_generation(), 1).await;
                } else if !matches!(
                    binding,
                    "unselected"
                        | "headless_unselected"
                        | "seen_unselected"
                        | "same_provider_automatic_switch"
                ) && !binding.starts_with("unselected_")
                    && !binding.starts_with("headless_unselected_")
                {
                    select_rotation_test_model(
                        &hub,
                        &session,
                        store.worker_generation(),
                        "initial-account",
                        "fake",
                        "initial-model",
                        Some("selected-account"),
                        None,
                    )
                    .await;
                }
                if binding == "resolved"
                    || binding.starts_with("resolved_")
                    || binding == "seen_unselected"
                    || binding == "checkpoint_resolved"
                    || binding == "legacy_observed_create"
                {
                    let lease = hub
                        .acquire_worker_lease(session.clone())
                        .await
                        .expect("lease");
                    let metadata = lease
                        .session_metadata()
                        .await
                        .expect("metadata")
                        .expect("typed");
                    lease
                        .commit_resolved_route(
                            &metadata,
                            if binding == "seen_unselected" {
                                None
                            } else {
                                Some("selected-account")
                            },
                        )
                        .await
                        .expect("historical resolution");
                }
                if let Some(kind) = ["create", "fork", "metafork", "endpoint_only"]
                    .into_iter()
                    .find(|kind| binding == *kind || binding.ends_with(&format!("_{kind}")))
                {
                    session = inherited_session(&store, &hub, &session, kind, &mut failures).await;
                }
                let mut original = hub
                    .session_metadata(&session)
                    .await
                    .expect("metadata")
                    .expect("typed");
                if binding == "legacy_observed_create" {
                    append_legacy_same_epoch_observation(&store, &hub, &session, &original).await;
                }
                if binding.starts_with("headless_") {
                    accept_headless_boundary(&hub, &session, store.worker_generation(), &original)
                        .await;
                } else {
                    start_legacy_rebind_test_run(&hub, &session, store.worker_generation()).await;
                }
                let mut accepted = hub
                    .accept_internal_turn(haider_core::TurnAcceptCommand {
                        command_id: format!("accept-{session}"),
                        request_digest: format!("accept-{session}"),
                        request_json: boundary_request(binding, &original),
                        session_id: session.clone(),
                        worker_generation: store.worker_generation(),
                        run_id: run.clone(),
                        agent_id: None,
                        branch_id: None,
                        text: "legacy rebind turn".into(),
                        attachments: Vec::new(),
                        mode: haider_protocol::DeliveryMode::Queue,
                        queued_event_id: EventId::new(format!("queued-{session}")),
                        user_event_id: EventId::new(format!("user-{session}")),
                        active_event_id: EventId::new(format!("active-{session}")),
                        device_id: DeviceId::new("rebind-test-device"),
                    })
                    .await
                    .expect("read accepted receipt");

                if binding.starts_with("headless_") {
                    let journal = store
                        .read(&session, 0, 4096)
                        .await
                        .expect("admission journal");
                    if !journal.iter().any(|event| event.payload.decode_event().is_ok_and(|payload| {
                    matches!(payload, EventPayload::Item(ItemEvent::Completed { item: TurnItem::Extension { kind, data }, .. })
                        if kind == haider_protocol::session::RunOutputPairPinnedV1::KIND && data.get("route_snapshot").is_some_and(serde_json::Value::is_object))
                })) { failures.push(format!("{binding}/{variant}: admission omitted complete snapshot")); }
                }
                if durable {
                    let lease = hub
                        .acquire_worker_lease(session.clone())
                        .await
                        .expect("lease");
                    let value = serde_json::json!({
                        "provider": original.provider, "model": original.model,
                        "max_tokens": original.max_tokens,
                        "source": haider_protocol::output_budget::SessionOutputBudgetSourceV1::Derived,
                        "provider_route": haider_protocol::session::RunOutputProviderRouteV1::capture(&original),
                        "route_snapshot": route_json(&original),
                    });
                    let pin: haider_protocol::session::RunOutputPairPinnedV1 =
                        serde_json::from_value(value).expect("additive snapshot");
                    append_payloads(
                        &lease,
                        &DeviceId::new("o29"),
                        &run,
                        None,
                        &EventIdGenerator::new("o29-pin"),
                        vec![pin.to_event(&run).expect("pin")],
                    )
                    .await
                    .expect("append pin");
                }
                if binding.ends_with("automatic_switch") || binding == "child_delayed" {
                    let switch_provider = if binding == "same_provider_automatic_switch" {
                        "fake"
                    } else {
                        "fake-next"
                    };
                    let lease = hub
                        .acquire_worker_lease(session.clone())
                        .await
                        .expect("lease");
                    let turn_selection = Arc::new(tokio::sync::Mutex::new(original.clone()));
                    let committer = DaemonProviderPairSwitchCommitter {
                        store: lease.clone(),
                        branch_id: None,
                        device_id: DeviceId::new("o29"),
                        event_ids: Arc::new(EventIdGenerator::new("automatic-selection")),
                        turn_selection: Arc::clone(&turn_selection),
                    };
                    haider_core::ProviderPairSwitchCommitter::commit(
                        &committer,
                        &haider_core::ProviderPairSwitch {
                            run_id: run.clone(),
                            switch_ordinal: 0,
                            source_rebind_seq: hub
                                .run_output_pair(&session, &run)
                                .await
                                .expect("source boundary")
                                .expect("run")
                                .latest_rebind_seq,
                            target_account: "switch-account".into(),
                            from_provider: original.provider.clone(),
                            from_model: original.model.clone(),
                            to_provider: switch_provider.into(),
                            to_model: "switched-model".into(),
                            cause: haider_core::ProviderPairSwitchCause::FallbackChain,
                        },
                    )
                    .await
                    .expect("production automatic selection");
                    if binding == "child_delayed" {
                        commit_test_rebind(&hub, &session, store.worker_generation(), 2).await;
                        original = hub
                            .session_metadata(&session)
                            .await
                            .expect("rebound parent")
                            .expect("typed");
                    }
                    append_payloads(&lease, &DeviceId::new("o29"), &run, None,
                    &EventIdGenerator::new("automatic-switch"), vec![EventPayload::Item(ItemEvent::Completed {
                        item_id: haider_protocol::ids::ItemId::new("automatic-switch"),
                        item: TurnItem::Extension { kind: "provider_pair_switch_v1".into(), data: serde_json::json!({
                            "from_provider": original.provider, "from_model": original.model,
                            "to_provider": switch_provider, "to_model": "switched-model", "resolved_alias": "switch-account"
                        }) }
                    })]).await.expect("automatic switch");
                    if binding != "child_delayed" {
                        if original.provider != switch_provider {
                            original.provider_base_url = None;
                            original.provider_rebind_id = None;
                        }
                        original.provider = switch_provider.into();
                        original.model = "switched-model".into();
                        original.account_alias = None;
                        original.resolved_route_alias = None;
                        original.resolved_route_seen = false;
                        let live = turn_selection.lock().await;
                        if route_json(&live) != route_json(&original) {
                            failures.push(format!("{binding}/{durable}/{variant}: live automatic switch changed the run's route provenance"));
                        }
                        original.resolved_route_alias = Some("switch-account".into());
                        original.resolved_route_seen = true;
                    }
                }
                match variant {
                    "cross_provider"
                    | "account_pin"
                    | "same_provider_model"
                    | "budget_reselect"
                    | "model_roundtrip" => {
                        change_rotation_test_session(
                            &hub,
                            &session,
                            store.worker_generation(),
                            &original.model,
                            variant,
                        )
                        .await
                    }
                    "account_roundtrip" => {
                        select_rotation_test_model(
                            &hub,
                            &session,
                            store.worker_generation(),
                            "account-away",
                            "fake",
                            &original.model,
                            Some("pin-b"),
                            None,
                        )
                        .await;
                        select_rotation_test_model(
                            &hub,
                            &session,
                            store.worker_generation(),
                            "account-back",
                            "fake",
                            &original.model,
                            Some("selected-account"),
                            None,
                        )
                        .await;
                    }
                    "provider_roundtrip" => {
                        select_rotation_test_model(
                            &hub,
                            &session,
                            store.worker_generation(),
                            "provider-away",
                            "fake-next",
                            &original.model,
                            None,
                            None,
                        )
                        .await;
                        select_rotation_test_model(
                            &hub,
                            &session,
                            store.worker_generation(),
                            "provider-back",
                            "fake",
                            &original.model,
                            None,
                            None,
                        )
                        .await;
                    }
                    "later_q_selection" => {
                        select_rotation_test_model(
                            &hub,
                            &session,
                            store.worker_generation(),
                            "q-next",
                            "fake-next",
                            "later-q-model",
                            None,
                            Some(3333),
                        )
                        .await
                    }
                    "rebind_after_start" => {
                        commit_test_rebind(&hub, &session, store.worker_generation(), 2).await
                    }
                    _ => {}
                }
                if binding.starts_with("opaque_post_start_rebind") {
                    commit_test_rebind(&hub, &session, store.worker_generation(), 2).await;
                }
                if binding.starts_with("checkpoint_") {
                    if binding == "checkpoint_resolved" {
                        let lease = hub
                            .acquire_worker_lease(session.clone())
                            .await
                            .expect("lease");
                        let later = lease
                            .session_metadata()
                            .await
                            .expect("metadata")
                            .expect("typed");
                        lease
                            .commit_resolved_route(&later, Some("later-served-account"))
                            .await
                            .expect("later turn observation");
                    }
                    append_old_recovery_checkpoint(
                        &hub,
                        &session,
                        &run,
                        &original,
                        binding == "checkpoint_resolved",
                    )
                    .await;
                }
                let expected = if variant == "rebind_after_start"
                    || binding.starts_with("opaque_post_start_rebind")
                {
                    hub.session_metadata(&session)
                        .await
                        .expect("metadata")
                        .expect("typed")
                } else {
                    original.clone()
                };
                let legacy_events = if binding.starts_with("opaque_") {
                    store
                        .read(&session, 0, 4096)
                        .await
                        .expect("fixture history")
                } else {
                    Vec::new()
                };
                hub.shutdown().await.expect("shutdown");
                store.close().await.expect("close");
                if binding.starts_with("opaque_") {
                    strip_legacy_witnesses(root.path(), &legacy_events, binding);
                }
                let (store, hub) = crate::session_hub::open_retention_test_hub(root.path())
                    .await
                    .expect("reopen");
                let mut restored = hub
                    .session_metadata(&session)
                    .await
                    .expect("metadata")
                    .expect("typed");
                let pair = hub
                    .run_output_pair(&session, &run)
                    .await
                    .expect("pair")
                    .expect("recorded pair");
                #[allow(clippy::let_unit_value)]
                // The same probe runs against the pre-fix void-returning API.
                let _ = pair.restore_route(&mut restored);
                restored.provider = pair.provider;
                restored.model = pair.model;
                let expected_route = (
                    &expected.provider,
                    &expected.provider_base_url,
                    &expected.account_alias,
                    &expected.provider_rebind_id,
                );
                let recovered_route = (
                    &restored.provider,
                    &restored.provider_base_url,
                    &restored.account_alias,
                    &restored.provider_rebind_id,
                );
                println!(
                    "O29 legacy-route binding={binding} durable={durable} variant={variant} expected={expected_route:?} recovered={recovered_route:?}"
                );
                if expected_route != recovered_route {
                    failures.push(format!("binding={binding} durable={durable} variant={variant}: {recovered_route:?} != {expected_route:?}"));
                }
                let expected_epoch = (expected.selection_epoch, expected.route_reset_epoch);
                let recovered_epoch = (restored.selection_epoch, restored.route_reset_epoch);
                println!(
                    "O29 epoch binding={binding} durable={durable} variant={variant} expected={expected_epoch:?} recovered={recovered_epoch:?}"
                );
                if (
                    &restored.resolved_route_alias,
                    restored.resolved_route_seen,
                    restored.route_reset_epoch,
                ) != (
                    &expected.resolved_route_alias,
                    expected.resolved_route_seen,
                    expected.route_reset_epoch,
                ) {
                    failures.push(format!(
                        "{binding}/{variant}: route observation/reset differs"
                    ));
                }
                if expected_epoch.0 != recovered_epoch.0 {
                    failures.push(format!("binding={binding} durable={durable} variant={variant}: restored route has a next-turn selection epoch"));
                }
                // Exercise the production recovery/start_turn path and inspect the
                // actual metadata delivered to the provider factory, not only the snapshot.
                accepted.worker_generation = store.worker_generation();
                let mut steps = Vec::new();
                if binding.starts_with("child") {
                    steps.extend([
                    haider_provider::FakeStep::EmitToolCall { call_id: "spawn-route".into(), name: "spawn_subagent".into(),
                        args: serde_json::json!({"task": "route witness", "prompt": "report route"}) },
                    haider_provider::FakeStep::Finish { reason: haider_protocol::provider::FinishReason::ToolUse },
                    haider_provider::FakeStep::EmitText { text: "child report".into() },
                    haider_provider::FakeStep::Finish { reason: haider_protocol::provider::FinishReason::EndTurn },
                    haider_provider::FakeStep::ExpectToolResult { call_id: "spawn-route".into() },
                ]);
                }
                steps.extend([
                    haider_provider::FakeStep::EmitText {
                        text: "o29 recovered".into(),
                    },
                    haider_provider::FakeStep::Finish {
                        reason: haider_protocol::provider::FinishReason::EndTurn,
                    },
                ]);
                let provider = Arc::new(haider_provider::FakeProvider::new(steps));
                let factory = Arc::new(RecordingRebindFactory {
                    provider: Some(provider.clone()),
                    ..Default::default()
                });
                let manager = WorkerManager::start(
                    hub.clone(),
                    WorkerDependencies {
                        provider_factory: Arc::new(BoundaryRecordingFactory {
                            inner: factory.clone(),
                            default_alias: binding
                                .ends_with("automatic_switch")
                                .then(|| "switch-account".to_owned()),
                        }),
                        tool_factory: if binding.starts_with("child") {
                            crate::worker::DaemonDependencies::default()
                                .with_tool_exposure(Some(vec!["spawn_subagent".into()]))
                                .tool_factory
                        } else {
                            Arc::new(BrokerToolFactory)
                        },
                        delegation: (binding.starts_with("child"))
                            .then(|| crate::delegation::DelegationHandle::new(hub.clone())),
                        web_search: None,
                        diagnostics: None,
                    },
                    false,
                );
                hub.install_worker_manager(manager.handle())
                    .expect("recovery worker");
                manager
                    .handle()
                    .recover_workflow_continuation(accepted, 1, 1)
                    .await
                    .expect("resume run");
                tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    let ended = store.read(&session, 0, 4096).await.expect("journal").iter().any(|event| {
                        event.run_id.as_ref() == Some(&run) && matches!(event.payload.decode_event(), Ok(EventPayload::RunState(state)) if state.is_terminal())
                    });
                    if ended { break; }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }).await.expect("recovery settles");
                if !durable && !binding.starts_with("headless_") {
                    let journal = store.read(&session, 0, 4096).await.expect("pickup journal");
                    if !journal.iter().any(|event| event.payload.decode_event().is_ok_and(|payload| {
                    matches!(payload, EventPayload::Item(ItemEvent::Completed { item: TurnItem::Extension { kind, data }, .. })
                        if kind == haider_protocol::session::RunOutputPairPinnedV1::KIND && data.get("route_snapshot").is_some_and(serde_json::Value::is_object))
                })) { failures.push(format!("{binding}/{variant}: pickup omitted complete snapshot")); }
                }
                let terminal = store
                    .read(&session, 0, 4096)
                    .await
                    .expect("terminal journal")
                    .into_iter()
                    .filter_map(|event| {
                        if event.run_id.as_ref() == Some(&run)
                            && let Ok(EventPayload::RunState(state)) = event.payload.decode_event()
                            && state.is_terminal()
                        {
                            return Some(state);
                        }
                        None
                    })
                    .next_back();
                println!(
                    "O29 terminal binding={binding} durable={durable} variant={variant} state={terminal:?}"
                );
                assert_eq!(
                    terminal,
                    Some(RunState::Done),
                    "fake recovery completes normally"
                );
                let calls = factory.calls.lock().expect("calls").clone();
                println!(
                    "O29 dispatch binding={binding} durable={durable} variant={variant} calls={:?} requests={}",
                    calls
                        .iter()
                        .map(|m| (
                            &m.provider,
                            &m.model,
                            &m.provider_base_url,
                            &m.account_alias,
                            &m.provider_rebind_id
                        ))
                        .collect::<Vec<_>>(),
                    provider.requests().len()
                );
                let expected_dispatches = if binding.starts_with("child") { 3 } else { 1 };
                if calls.len() != (if binding.starts_with("child") { 2 } else { 1 })
                    || provider.requests().len() != expected_dispatches
                {
                    if binding.starts_with("child") {
                        for event in store
                            .read(&session, 0, 4096)
                            .await
                            .expect("diagnostic journal")
                        {
                            let payload =
                                serde_json::to_string(&event.payload).expect("diagnostic payload");
                            if payload.contains("spawn-route") {
                                println!(
                                    "O29 child-tool {}",
                                    payload.chars().take(4096).collect::<String>()
                                );
                            }
                        }
                    }
                    failures.push(format!("binding={binding} durable={durable} variant={variant}: recovery did not dispatch exactly once"));
                } else {
                    if binding.starts_with("child") {
                        let child = &calls[1];
                        if (
                            &child.provider,
                            &child.provider_base_url,
                            &child.account_alias,
                            &child.provider_rebind_id,
                        ) != expected_route
                        {
                            failures.push(format!("{binding}/{durable}/{variant}: child inherits the run boundary route"));
                        }
                        if child.selection_epoch != expected_epoch.0 {
                            failures.push(format!("{binding}/{durable}/{variant}: child inherits the run boundary epoch"));
                        }
                        if child.route_reset_epoch != expected_epoch.1
                            || !child.resolved_route_seen
                            || child.resolved_route_alias != original.account_alias
                        {
                            failures.push(format!("{binding}/{durable}/{variant}: child inherits the run's own account observation and reset epoch"));
                        }
                        if child.model != original.model || child.max_tokens != original.max_tokens
                        {
                            failures.push(format!("{binding}/{durable}/{variant}: child inherits the run model and budget"));
                        }
                    }
                    let m = &calls[0];
                    let requests = provider.requests();
                    let footprint_epochs = store
                        .read(&session, 0, 4096)
                        .await
                        .expect("request footprints")
                        .into_iter()
                        .filter(|event| event.run_id.as_ref() == Some(&run))
                        .filter_map(|event| match event.payload.decode_event() {
                            Ok(EventPayload::Item(ItemEvent::Completed {
                                item: TurnItem::Extension { kind, data },
                                ..
                            })) if kind == "context_footprint_v1" => data
                                .get("selection_epoch")
                                .and_then(serde_json::Value::as_u64),
                            _ => None,
                        })
                        .collect::<std::collections::BTreeSet<_>>();
                    println!(
                        "O29 footprint-epoch binding={binding} durable={durable} variant={variant} expected={} actual={footprint_epochs:?}",
                        expected_epoch.0.unwrap_or(0)
                    );
                    if footprint_epochs
                        != std::collections::BTreeSet::from([expected_epoch.0.unwrap_or(0)])
                    {
                        failures.push(format!("{binding}/{durable}/{variant}: actual request footprint has the next-turn selection epoch"));
                    }
                    println!(
                        "O29 request binding={binding} durable={durable} variant={variant} model={} max_tokens={} factory_max_tokens={}",
                        requests[0].model, requests[0].max_tokens, m.max_tokens
                    );
                    assert_eq!(
                        requests[0].model, original.model,
                        "the original active model remains pinned"
                    );
                    assert_eq!(
                        requests[0].max_tokens, original.max_tokens,
                        "the original active budget remains pinned"
                    );

                    println!(
                        "O29 dispatch-epoch binding={binding} durable={durable} variant={variant} actual={:?}",
                        (m.selection_epoch, m.route_reset_epoch)
                    );
                    if m.selection_epoch != expected_epoch.0 {
                        failures.push(format!("binding={binding} durable={durable} variant={variant}: provider factory received the next-turn selection epoch"));
                    }

                    if (
                        &m.provider,
                        &m.provider_base_url,
                        &m.account_alias,
                        &m.provider_rebind_id,
                    ) != expected_route
                    {
                        failures.push(format!("binding={binding} durable={durable} variant={variant}: provider factory received the wrong route"));
                    }
                }
                manager.shutdown().await.expect("stop worker");
                hub.shutdown().await.expect("shutdown");
                store.close().await.expect("close");
            }
        }
    }
    assert!(
        failures.is_empty(),
        "legacy route reconstruction failures: {failures:#?}"
    );
}

const TRANSITIONS: &[&str] = &[
    "control",
    "cross_provider",
    "account_pin",
    "rebind_after_start",
    "same_provider_model",
    "budget_reselect",
    "model_roundtrip",
    "provider_roundtrip",
    "account_roundtrip",
];

#[tokio::test]
async fn o29_legacy_prebound_route_is_recovered_at_the_run_boundary() {
    recovery_matrix(&["rebind", "account_selection"], TRANSITIONS).await;
}

#[tokio::test]
async fn recovery_preserves_absent_selector_and_historical_resolution() {
    recovery_matrix(
        &["unselected", "resolved", "seen_unselected"],
        &[
            "control",
            "cross_provider",
            "account_pin",
            "provider_roundtrip",
            "same_provider_model",
            "budget_reselect",
        ],
    )
    .await;
}

#[tokio::test]
async fn recovery_preserves_create_fork_and_metafork_inheritance() {
    recovery_matrix(
        &[
            "create",
            "fork",
            "metafork",
            "resolved_create",
            "resolved_fork",
            "resolved_metafork",
            "endpoint_only",
            "account_create",
            "account_fork",
            "account_metafork",
            "unselected_create",
            "unselected_fork",
            "unselected_metafork",
            "legacy_observed_create",
        ],
        &[
            "cross_provider",
            "account_pin",
            "provider_roundtrip",
            "same_provider_model",
        ],
    )
    .await;
}

#[tokio::test]
async fn recovery_preserves_headless_admission_account_and_absent_selector() {
    recovery_matrix(
        &["headless_account", "headless_unselected"],
        &[
            "control",
            "cross_provider",
            "account_pin",
            "provider_roundtrip",
            "same_provider_model",
        ],
    )
    .await;
}

#[tokio::test]
async fn recovery_preserves_automatic_switch_epoch_after_later_q_selection() {
    recovery_matrix(
        &[
            "automatic_switch",
            "headless_automatic_switch",
            "same_provider_automatic_switch",
            "unselected_automatic_switch",
            "headless_unselected_automatic_switch",
        ],
        &["control", "later_q_selection"],
    )
    .await;
}

#[tokio::test]
async fn recovered_parent_child_inherits_boundary_route() {
    recovery_matrix(
        &["child"],
        &["cross_provider", "account_pin", "provider_roundtrip"],
    )
    .await;
}

#[tokio::test]
async fn unrecoverable_legacy_route_history_returns_a_typed_error() {
    for route_only in [false, true] {
        let root = tempfile::tempdir().expect("private profile");
        let (store, hub) = crate::session_hub::open_retention_test_hub(root.path())
            .await
            .expect("hub");
        let session = SessionId::new(format!("missing-route-witness-{route_only}"));
        create_rebind_test_session(&hub, &session).await;
        let lease = hub
            .acquire_worker_lease(session.clone())
            .await
            .expect("lease");
        let legacy = haider_protocol::session::ModelSelected {
            selection_epoch: Some(2),
            provider: "fake".into(),
            model: "initial-model".into(),
            route_only,
            cleared_account_pin: None,
            cleared_provider_endpoint: None,
            output_budget_clamp: None,
        }
        .to_payload_value()
        .expect("legacy fact");
        let mut fact = [supervisor_raw_envelope(
            &lease,
            &DeviceId::new("legacy"),
            None,
            None,
            EventId::new("legacy-without-route-witness"),
            legacy,
        )];
        haider_core::StoreHandle::append(&store, &mut fact)
            .await
            .expect("legacy journal");
        drop(lease);
        start_legacy_rebind_test_run(&hub, &session, store.worker_generation()).await;
        hub.shutdown().await.expect("shutdown");
        store.close().await.expect("close");
        let (store, hub) = crate::session_hub::open_retention_test_hub(root.path())
            .await
            .expect("reopen");
        let result = hub
            .run_output_pair(&session, &RunId::new("rebind-test-run"))
            .await;
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("unrecoverable route must fail closed"),
        };
        assert_eq!(error.code, ErrorCode::StoreCorrupt);
        assert_eq!(
            error
                .presentation
                .expect("typed recovery error")
                .subcode
                .as_str(),
            "run-route-history-unavailable"
        );
        hub.shutdown().await.expect("shutdown");
        store.close().await.expect("close");
    }
}

#[tokio::test]
async fn headless_admission_refuses_a_provider_without_a_matching_route() {
    let root = tempfile::tempdir().expect("profile");
    let (store, hub) = crate::session_hub::open_retention_test_hub(root.path())
        .await
        .expect("hub");
    let session = SessionId::new("stale-headless-route");
    create_rebind_test_session(&hub, &session).await;
    let before = store.read(&session, 0, 4096).await.expect("journal").len();
    let mut spec = rebound_recovery_headless_spec();
    spec.provider = "fake-next".into();
    let result = hub
        .accept_internal_turn(haider_core::TurnAcceptCommand {
            command_id: "stale-route-admission".into(),
            request_digest: "stale-route-admission".into(),
            request_json: serde_json::json!({"headless": spec}).to_string(),
            session_id: session.clone(),
            worker_generation: store.worker_generation(),
            run_id: RunId::new("stale-route-run"),
            agent_id: None,
            branch_id: None,
            text: "stale route".into(),
            attachments: Vec::new(),
            mode: haider_protocol::DeliveryMode::Queue,
            queued_event_id: EventId::new("stale-route-queued"),
            user_event_id: EventId::new("stale-route-user"),
            active_event_id: EventId::new("stale-route-active"),
            device_id: DeviceId::new("stale-route-device"),
        })
        .await;
    assert_eq!(
        result
            .expect_err("mismatched headless provider must be refused")
            .code,
        ErrorCode::InvalidArgument
    );
    assert_eq!(
        store.read(&session, 0, 4096).await.expect("journal").len(),
        before,
        "refusal appends no partial run facts"
    );
    // A later selection configures the next run; steering this already
    // admitted headless run must not require that mutable route to match.
    let original = hub
        .session_metadata(&session)
        .await
        .expect("metadata")
        .expect("session");
    accept_headless_boundary(&hub, &session, store.worker_generation(), &original).await;
    let run = RunId::new("rebind-test-run");
    let lease = hub
        .acquire_worker_lease(session.clone())
        .await
        .expect("lease");
    append_payloads(
        &lease,
        &DeviceId::new("headless-steer-device"),
        &run,
        None,
        &EventIdGenerator::new("headless-steer-start"),
        vec![EventPayload::RunState(RunState::Thinking)],
    )
    .await
    .expect("start admitted headless run");
    drop(lease);
    select_rotation_test_model(
        &hub,
        &session,
        store.worker_generation(),
        "headless-steer-next-turn",
        "fake-next",
        "later-model",
        None,
        None,
    )
    .await;
    hub.accept_internal_turn(haider_core::TurnAcceptCommand {
        command_id: "headless-same-run-steer".into(),
        request_digest: "headless-same-run-steer".into(),
        request_json: boundary_request("headless_account", &original),
        session_id: session.clone(),
        worker_generation: store.worker_generation(),
        run_id: run.clone(),
        agent_id: None,
        branch_id: None,
        text: "steer the admitted headless run".into(),
        attachments: Vec::new(),
        mode: haider_protocol::DeliveryMode::Steer,
        queued_event_id: EventId::new("headless-steer-queued"),
        user_event_id: EventId::new("headless-steer-user"),
        active_event_id: EventId::new("headless-steer-active"),
        device_id: DeviceId::new("headless-steer-device"),
    })
    .await
    .expect("same-run headless steer keeps its admission route");
    hub.shutdown().await.expect("shutdown");
    store.close().await.expect("close");
    let (store, hub) = crate::session_hub::open_retention_test_hub(root.path())
        .await
        .expect("reopen");
    let mut restored = hub
        .session_metadata(&session)
        .await
        .expect("metadata")
        .expect("session");
    let pair = hub
        .run_output_pair(&session, &run)
        .await
        .expect("owned pair")
        .expect("admitted");
    #[allow(clippy::let_unit_value)]
    let _ = pair.restore_route(&mut restored);
    assert_eq!(
        (pair.provider, pair.model, pair.max_tokens),
        (
            original.provider.clone(),
            original.model.clone(),
            original.max_tokens
        )
    );
    assert_eq!(
        route_json(&restored),
        route_json(&original),
        "steer preserves the admission snapshot across reopen"
    );
    println!("HEADLESS STEER preserved admitted route across a next-turn selection and reopen");

    hub.shutdown().await.expect("shutdown");
    store.close().await.expect("close");
}

#[tokio::test]
async fn unrecoverable_headless_provider_history_returns_a_typed_error() {
    let root = tempfile::tempdir().expect("profile");
    let (store, hub) = crate::session_hub::open_retention_test_hub(root.path())
        .await
        .expect("hub");
    let session = SessionId::new("legacy-stale-headless-route");
    let run = RunId::new("rebind-test-run");
    create_rebind_test_session(&hub, &session).await;
    start_legacy_rebind_test_run(&hub, &session, store.worker_generation()).await;
    let mut spec = rebound_recovery_headless_spec();
    spec.provider = "fake-next".into();
    let lease = hub
        .acquire_worker_lease(session.clone())
        .await
        .expect("lease");
    let mut fact = [supervisor_raw_envelope(
        &lease,
        &DeviceId::new("legacy"),
        None,
        Some(run.clone()),
        EventId::new("legacy-stale-headless-spec"),
        HeadlessRunEventPayload::HeadlessRunConfigured(spec)
            .to_payload_value()
            .expect("spec"),
    )];
    haider_core::StoreHandle::append(&store, &mut fact)
        .await
        .expect("old-writer fact");
    drop(lease);
    hub.shutdown().await.expect("shutdown");
    store.close().await.expect("close");
    let (store, hub) = crate::session_hub::open_retention_test_hub(root.path())
        .await
        .expect("reopen");
    let error = match hub.run_output_pair(&session, &run).await {
        Err(error) => error,
        Ok(_) => panic!("an unrecorded provider route cannot be reconstructed"),
    };
    assert_eq!(error.code, ErrorCode::StoreCorrupt);
    assert_eq!(
        error
            .presentation
            .expect("typed recovery error")
            .subcode
            .as_str(),
        "run-route-history-unavailable"
    );
    hub.shutdown().await.expect("shutdown");
    store.close().await.expect("close");
}

#[tokio::test]
async fn legacy_recovery_checkpoints_keep_the_original_boundary() {
    recovery_matrix(
        &["checkpoint_rebind", "checkpoint_account"],
        &[
            "cross_provider",
            "account_pin",
            "model_roundtrip",
            "provider_roundtrip",
        ],
    )
    .await;
}

#[tokio::test]
async fn legacy_recovery_does_not_mint_an_epoch_from_a_later_seen_account() {
    // The durable mode also controls a complete boundary against an obsolete
    // duplicate checkpoint; the legacy mode models the old recovery writer.
    recovery_matrix(
        &["checkpoint_resolved"],
        &["control", "budget_reselect", "model_roundtrip"],
    )
    .await;
}

#[tokio::test]
async fn later_historical_witnesses_and_owned_rebinds_resolve_legacy_uncertainty() {
    recovery_matrix(
        &[
            "opaque_later_witness",
            "opaque_legacy_rebind",
            "opaque_post_start_rebind",
            "opaque_later_witness_empty",
            "opaque_legacy_rebind_empty",
            "opaque_post_start_rebind_empty",
        ],
        &["cross_provider", "account_pin", "provider_roundtrip"],
    )
    .await;
}

#[tokio::test]
async fn opaque_legacy_account_arguments_fail_closed_even_with_a_model_key() {
    for binding in ["opaque_missing_empty", "opaque_missing_model"] {
        let root = tempfile::tempdir().expect("profile");
        let (store, hub) = crate::session_hub::open_retention_test_hub(root.path())
            .await
            .expect("hub");
        let session = SessionId::new(binding);
        create_rebind_test_session(&hub, &session).await;
        setup_opaque_selection(&hub, &session, store.worker_generation(), binding).await;
        start_legacy_rebind_test_run(&hub, &session, store.worker_generation()).await;
        select_rotation_test_model(
            &hub,
            &session,
            store.worker_generation(),
            "later-account",
            "fake",
            "initial-model",
            Some("later-account"),
            None,
        )
        .await;
        let events = store.read(&session, 0, 4096).await.expect("history");
        hub.shutdown().await.expect("shutdown");
        store.close().await.expect("close");
        strip_legacy_witnesses(root.path(), &events, binding);
        let (store, hub) = crate::session_hub::open_retention_test_hub(root.path())
            .await
            .expect("reopen");
        let error = hub
            .run_output_pair(&session, &RunId::new("rebind-test-run"))
            .await
            .err();
        hub.shutdown().await.expect("shutdown");
        store.close().await.expect("close");
        let error = error.expect("unrecorded account setter must fail closed");
        assert_eq!(error.code, ErrorCode::StoreCorrupt);
        assert_eq!(
            error
                .presentation
                .expect("typed recovery error")
                .subcode
                .as_str(),
            "run-route-history-unavailable"
        );
    }
}

#[tokio::test]
async fn legacy_automatic_switch_without_account_observation_fails_closed() {
    let root = tempfile::tempdir().expect("private profile");
    let (store, hub) = crate::session_hub::open_retention_test_hub(root.path())
        .await
        .expect("hub");
    let session = SessionId::new("legacy-switch-missing-observation");
    let run = RunId::new("rebind-test-run");
    create_rebind_test_session(&hub, &session).await;
    start_legacy_rebind_test_run(&hub, &session, store.worker_generation()).await;
    let lease = hub
        .acquire_worker_lease(session.clone())
        .await
        .expect("lease");
    append_payloads(
        &lease,
        &DeviceId::new("legacy-switch"),
        &run,
        None,
        &EventIdGenerator::new("legacy-switch"),
        vec![EventPayload::Item(ItemEvent::Completed {
            item_id: haider_protocol::ids::ItemId::new("legacy-switch"),
            item: TurnItem::Extension {
                kind: "provider_pair_switch_v1".into(),
                data: serde_json::json!({
                    "from_provider": "fake", "from_model": "initial-model",
                    "to_provider": "fake-next", "to_model": "switched-model"
                }),
            },
        })],
    )
    .await
    .expect("old switch has no resolved alias witness");
    drop(lease);
    select_rotation_test_model(
        &hub,
        &session,
        store.worker_generation(),
        "later-switch-account",
        "fake-next",
        "switched-model",
        Some("later-account"),
        None,
    )
    .await;
    hub.shutdown().await.expect("shutdown");
    store.close().await.expect("close");
    let (store, hub) = crate::session_hub::open_retention_test_hub(root.path())
        .await
        .expect("reopen");
    let result = hub.run_output_pair(&session, &run).await;
    println!(
        "LEGACY SWITCH accepted unknown account observation: {}",
        result.is_ok()
    );
    let error = result.err();
    hub.shutdown().await.expect("shutdown");
    store.close().await.expect("close");
    let error = error.expect("legacy automatic switch requires historical account observation");
    assert_eq!(error.code, ErrorCode::StoreCorrupt);
    assert_eq!(
        error
            .presentation
            .expect("typed recovery error")
            .subcode
            .as_str(),
        "run-route-history-unavailable"
    );
}

#[tokio::test]
async fn recovered_parent_child_inherits_newer_rebind_after_delayed_automatic_fact() {
    recovery_matrix(
        &["child_delayed"],
        &["cross_provider", "account_pin", "provider_roundtrip"],
    )
    .await;
}
