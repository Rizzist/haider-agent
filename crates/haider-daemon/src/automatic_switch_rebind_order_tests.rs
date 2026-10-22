#![allow(clippy::expect_used)]
use super::*;
// Independent review: the production automatic-selection commit and Core's
// later provider_pair_switch_v1 append have an await boundary between them.
#[tokio::test]
async fn astra31_automatic_rebind_order_consistency() {
    let mut failures = Vec::new();
    for target_provider in ["fake", "fake-next"] {
        for late_marker in [false, true] {
            let root = tempfile::tempdir().expect("profile");
            let (store, hub) = crate::session_hub::open_retention_test_hub(root.path())
                .await
                .expect("hub");
            let sid = SessionId::new(format!("a31-order-{target_provider}-{late_marker}"));
            let run = RunId::new("rebind-test-run");
            create_rebind_test_session(&hub, &sid).await;
            let original = hub
                .session_metadata(&sid)
                .await
                .expect("metadata")
                .expect("typed");
            start_legacy_rebind_test_run(&hub, &sid, store.worker_generation()).await;
            let lease = hub.acquire_worker_lease(sid.clone()).await.expect("lease");
            let pin = haider_protocol::session::RunOutputPairPinnedV1 {
                provider: original.provider.clone(),
                model: original.model.clone(),
                max_tokens: original.max_tokens,
                source: haider_protocol::output_budget::SessionOutputBudgetSourceV1::Derived,
                provider_route: haider_protocol::session::RunOutputProviderRouteV1::capture(
                    &original,
                ),
                route_snapshot: Some(haider_protocol::session::RunOutputRouteSnapshotV1::capture(
                    &original,
                )),
            };
            append_payloads(
                &lease,
                &DeviceId::new("a31"),
                &run,
                None,
                &EventIdGenerator::new("a31-pin"),
                vec![pin.to_event(&run).expect("pin")],
            )
            .await
            .expect("append pin");
            let turn_selection = Arc::new(tokio::sync::Mutex::new(original.clone()));
            let committer = DaemonProviderPairSwitchCommitter {
                store: lease.clone(),
                branch_id: None,
                device_id: DeviceId::new("a31"),
                event_ids: Arc::new(EventIdGenerator::new("a31-auto-select")),
                turn_selection: Arc::clone(&turn_selection),
            };
            let automatic_seq = haider_core::ProviderPairSwitchCommitter::commit(
                &committer,
                &haider_core::ProviderPairSwitch {
                    run_id: run.clone(),
                    switch_ordinal: 0,
                    source_rebind_seq: 0,
                    target_account: "automatic-account".into(),
                    from_provider: "fake".into(),
                    from_model: "initial-model".into(),
                    to_provider: target_provider.into(),
                    to_model: "switched-model".into(),
                    cause: haider_core::ProviderPairSwitchCause::FallbackChain,
                },
            )
            .await
            .expect("production automatic selection");
            let marker = || {
                EventPayload::Item(ItemEvent::Completed {
                    item_id: haider_protocol::ids::ItemId::new("a31-switch"),
                    item: TurnItem::Extension {
                        kind: "provider_pair_switch_v1".into(),
                        data: serde_json::json!({"from_provider":"fake", "from_model":"initial-model", "to_provider":target_provider, "to_model":"switched-model", "resolved_alias":"automatic-account", "why":"fallback_chain"}),
                    },
                })
            };
            if !late_marker {
                append_payloads(
                    &lease,
                    &DeviceId::new("a31"),
                    &run,
                    None,
                    &EventIdGenerator::new("a31-switch"),
                    vec![marker()],
                )
                .await
                .expect("Core switch fact before rebind");
            }
            commit_test_rebind(&hub, &sid, store.worker_generation(), 1).await;
            let expected = hub
                .session_metadata(&sid)
                .await
                .expect("metadata")
                .expect("rebound");
            if late_marker {
                append_payloads(
                    &lease,
                    &DeviceId::new("a31"),
                    &run,
                    None,
                    &EventIdGenerator::new("a31-switch"),
                    vec![marker()],
                )
                .await
                .expect("Core switch fact after rebind");
            }
            let events = store.read(&sid, 0, 4096).await.expect("journal");
            let order: Vec<_> = events
                .iter()
                .filter(|e| {
                    e.payload
                        .get("type")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|t| t == "model_selected" || t == "session_provider_rebound")
                        || e.payload
                            .get("item")
                            .and_then(|v| v.get("kind"))
                            .and_then(serde_json::Value::as_str)
                            == Some("provider_pair_switch_v1")
                })
                .map(|e| (e.seq, e.payload.clone()))
                .collect();
            println!(
                "A31 ORDER target={target_provider} late={late_marker} automatic_seq={automatic_seq} facts={order:?}"
            );
            drop(committer);
            drop(lease);
            hub.shutdown().await.expect("shutdown");
            store.close().await.expect("close");
            let (store, hub) = crate::session_hub::open_retention_test_hub(root.path())
                .await
                .expect("reopen");
            let pair = hub
                .run_output_pair(&sid, &run)
                .await
                .expect("pair")
                .expect("run");
            let mut restored = hub
                .session_metadata(&sid)
                .await
                .expect("metadata")
                .expect("session");
            pair.restore_route(&mut restored).expect("known route");
            restored.provider.clone_from(&pair.provider);
            restored.model.clone_from(&pair.model);
            restored.max_tokens = pair.max_tokens;
            let expected_route =
                haider_protocol::session::RunOutputRouteSnapshotV1::capture(&expected);
            let actual_route =
                haider_protocol::session::RunOutputRouteSnapshotV1::capture(&restored);
            println!(
                "A31 ORDER RESULT target={target_provider} late={late_marker} expected_provider={} actual_provider={} expected={expected_route:?} actual={actual_route:?}",
                expected.provider, restored.provider
            );
            if restored.provider != expected.provider || actual_route != expected_route {
                failures.push(format!("{target_provider}/late={late_marker}: expected provider {} route {expected_route:?}; got {} {actual_route:?}",expected.provider,restored.provider));
            }
            hub.shutdown().await.expect("shutdown");
            store.close().await.expect("close");
        }
    }
    assert!(
        failures.is_empty(),
        "explicit rebind after automatic selection must survive the delayed switch fact: {failures:#?}"
    );
}

#[tokio::test]
async fn astra31_automatic_rebind_live_consistency() {
    let mut failures = Vec::new();
    for target_provider in ["fake", "fake-next"] {
        for late_marker in [false, true] {
            let root = tempfile::tempdir().expect("profile");
            let (store, hub) = crate::session_hub::open_retention_test_hub(root.path())
                .await
                .expect("hub");
            let sid = SessionId::new(format!("a31-live-{target_provider}-{late_marker}"));
            let run = RunId::new("rebind-test-run");
            create_rebind_test_session(&hub, &sid).await;
            let original = hub
                .session_metadata(&sid)
                .await
                .expect("metadata")
                .expect("typed");
            start_legacy_rebind_test_run(&hub, &sid, store.worker_generation()).await;
            let lease = hub.acquire_worker_lease(sid.clone()).await.expect("lease");
            let pin = haider_protocol::session::RunOutputPairPinnedV1 {
                provider: original.provider.clone(),
                model: original.model.clone(),
                max_tokens: original.max_tokens,
                source: haider_protocol::output_budget::SessionOutputBudgetSourceV1::Derived,
                provider_route: haider_protocol::session::RunOutputProviderRouteV1::capture(
                    &original,
                ),
                route_snapshot: Some(haider_protocol::session::RunOutputRouteSnapshotV1::capture(
                    &original,
                )),
            };
            append_payloads(
                &lease,
                &DeviceId::new("a31"),
                &run,
                None,
                &EventIdGenerator::new("a31-pin"),
                vec![pin.to_event(&run).expect("pin")],
            )
            .await
            .expect("append pin");
            let turn_selection = Arc::new(tokio::sync::Mutex::new(original.clone()));
            let committer = DaemonProviderPairSwitchCommitter {
                store: lease.clone(),
                branch_id: None,
                device_id: DeviceId::new("a31"),
                event_ids: Arc::new(EventIdGenerator::new("a31-auto-select")),
                turn_selection: Arc::clone(&turn_selection),
            };
            let automatic_seq = haider_core::ProviderPairSwitchCommitter::commit(
                &committer,
                &haider_core::ProviderPairSwitch {
                    run_id: run.clone(),
                    switch_ordinal: 0,
                    source_rebind_seq: 0,
                    target_account: "automatic-account".into(),
                    from_provider: "fake".into(),
                    from_model: "initial-model".into(),
                    to_provider: target_provider.into(),
                    to_model: "switched-model".into(),
                    cause: haider_core::ProviderPairSwitchCause::FallbackChain,
                },
            )
            .await
            .expect("production automatic selection");
            let marker = || {
                EventPayload::Item(ItemEvent::Completed {
                    item_id: haider_protocol::ids::ItemId::new("a31-switch"),
                    item: TurnItem::Extension {
                        kind: "provider_pair_switch_v1".into(),
                        data: serde_json::json!({"from_provider":"fake", "from_model":"initial-model", "to_provider":target_provider, "to_model":"switched-model", "resolved_alias":"automatic-account", "why":"fallback_chain"}),
                    },
                })
            };
            if !late_marker {
                append_payloads(
                    &lease,
                    &DeviceId::new("a31"),
                    &run,
                    None,
                    &EventIdGenerator::new("a31-switch"),
                    vec![marker()],
                )
                .await
                .expect("Core switch fact before rebind");
            }
            commit_test_rebind(&hub, &sid, store.worker_generation(), 1).await;
            let expected = hub
                .session_metadata(&sid)
                .await
                .expect("metadata")
                .expect("rebound");
            if late_marker {
                append_payloads(
                    &lease,
                    &DeviceId::new("a31"),
                    &run,
                    None,
                    &EventIdGenerator::new("a31-switch"),
                    vec![marker()],
                )
                .await
                .expect("Core switch fact after rebind");
            }
            let events = store.read(&sid, 0, 4096).await.expect("journal");
            let order: Vec<_> = events
                .iter()
                .filter(|e| {
                    e.payload
                        .get("type")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|t| t == "model_selected" || t == "session_provider_rebound")
                        || e.payload
                            .get("item")
                            .and_then(|v| v.get("kind"))
                            .and_then(serde_json::Value::as_str)
                            == Some("provider_pair_switch_v1")
                })
                .map(|e| (e.seq, e.payload.clone()))
                .collect();
            println!(
                "A31 ORDER target={target_provider} late={late_marker} automatic_seq={automatic_seq} facts={order:?}"
            );
            let previous = turn_selection.lock().await.clone();
            let factory = Arc::new(RecordingRebindFactory::default());
            let resolver = DaemonProviderRebindResolver::new(
                lease.clone(),
                run.clone(),
                factory.clone(),
                previous,
                WebCapabilityDegrade::default(),
            );
            let refreshed = resolver
                .refresh("switched-model", r#"{"effort":null,"fast":false}"#)
                .await;
            let calls = factory.calls.lock().expect("recording factory").clone();
            println!(
                "A31 LIVE target={target_provider} late={late_marker} refresh_some={} calls={calls:?}",
                refreshed.as_ref().is_ok_and(Option::is_some)
            );
            if !refreshed.as_ref().is_ok_and(Option::is_some) {
                failures.push(format!("{target_provider}/late={late_marker}: pending explicit rebind was not returned: result_present={}",refreshed.is_ok()));
            }
            if let Some(actual) = calls.last() {
                let expected_route =
                    haider_protocol::session::RunOutputRouteSnapshotV1::capture(&expected);
                let actual_route =
                    haider_protocol::session::RunOutputRouteSnapshotV1::capture(actual);
                if actual.provider != expected.provider || actual_route != expected_route {
                    failures.push(format!("{target_provider}/late={late_marker}: provider factory expected {} {expected_route:?}; got {} {actual_route:?}",expected.provider,actual.provider));
                }
            } else {
                failures.push(format!("{target_provider}/late={late_marker}: explicit rebind never reached the factory"));
            }
            drop(resolver);
            drop(committer);
            drop(lease);
            hub.shutdown().await.expect("shutdown");
            store.close().await.expect("close");
        }
    }
    assert!(
        failures.is_empty(),
        "explicit rebind after automatic selection must survive the delayed switch fact: {failures:#?}"
    );
}

// Independent review: the production automatic-selection commit and Core's
// later provider_pair_switch_v1 append have an await boundary between them.
#[tokio::test]
async fn astra31_automatic_rebind_target_consistency() {
    let mut failures = Vec::new();
    for target_provider in ["fake", "fake-next"] {
        let late_marker = true;
        let root = tempfile::tempdir().expect("profile");
        let (store, hub) = crate::session_hub::open_retention_test_hub(root.path())
            .await
            .expect("hub");
        let sid = SessionId::new(format!("a31-target-{target_provider}-{late_marker}"));
        let run = RunId::new("rebind-test-run");
        create_rebind_test_session(&hub, &sid).await;
        let original = hub
            .session_metadata(&sid)
            .await
            .expect("metadata")
            .expect("typed");
        start_legacy_rebind_test_run(&hub, &sid, store.worker_generation()).await;
        let lease = hub.acquire_worker_lease(sid.clone()).await.expect("lease");
        let pin = haider_protocol::session::RunOutputPairPinnedV1 {
            provider: original.provider.clone(),
            model: original.model.clone(),
            max_tokens: original.max_tokens,
            source: haider_protocol::output_budget::SessionOutputBudgetSourceV1::Derived,
            provider_route: haider_protocol::session::RunOutputProviderRouteV1::capture(&original),
            route_snapshot: Some(haider_protocol::session::RunOutputRouteSnapshotV1::capture(
                &original,
            )),
        };
        append_payloads(
            &lease,
            &DeviceId::new("a31"),
            &run,
            None,
            &EventIdGenerator::new("a31-pin"),
            vec![pin.to_event(&run).expect("pin")],
        )
        .await
        .expect("append pin");
        let turn_selection = Arc::new(tokio::sync::Mutex::new(original.clone()));
        let committer = DaemonProviderPairSwitchCommitter {
            store: lease.clone(),
            branch_id: None,
            device_id: DeviceId::new("a31"),
            event_ids: Arc::new(EventIdGenerator::new("a31-auto-select")),
            turn_selection: Arc::clone(&turn_selection),
        };
        commit_test_rebind(&hub, &sid, store.worker_generation(), 1).await;
        let expected = hub
            .session_metadata(&sid)
            .await
            .expect("metadata")
            .expect("rebound");
        let automatic_result = haider_core::ProviderPairSwitchCommitter::commit(
            &committer,
            &haider_core::ProviderPairSwitch {
                run_id: run.clone(),
                switch_ordinal: 0,
                source_rebind_seq: 0,
                target_account: "automatic-account".into(),
                from_provider: "fake".into(),
                from_model: "initial-model".into(),
                to_provider: target_provider.into(),
                to_model: "switched-model".into(),
                cause: haider_core::ProviderPairSwitchCause::FallbackChain,
            },
        )
        .await;
        let automatic_seq = match automatic_result {
            Ok(seq) => seq,
            Err(error) => {
                println!("A31 TARGET declined stale target {target_provider}: {error:?}");
                assert_eq!(
                    hub.session_metadata(&sid)
                        .await
                        .expect("metadata")
                        .expect("session"),
                    expected
                );
                drop(committer);
                drop(lease);
                hub.shutdown().await.expect("shutdown");
                store.close().await.expect("close");
                continue;
            }
        };
        let marker = || {
            EventPayload::Item(ItemEvent::Completed {
                item_id: haider_protocol::ids::ItemId::new("a31-switch"),
                item: TurnItem::Extension {
                    kind: "provider_pair_switch_v1".into(),
                    data: serde_json::json!({"from_provider":"fake", "from_model":"initial-model", "to_provider":target_provider, "to_model":"switched-model", "resolved_alias":"automatic-account", "why":"fallback_chain"}),
                },
            })
        };
        if !late_marker {
            append_payloads(
                &lease,
                &DeviceId::new("a31"),
                &run,
                None,
                &EventIdGenerator::new("a31-switch"),
                vec![marker()],
            )
            .await
            .expect("Core switch fact before rebind");
        }
        if late_marker {
            append_payloads(
                &lease,
                &DeviceId::new("a31"),
                &run,
                None,
                &EventIdGenerator::new("a31-switch"),
                vec![marker()],
            )
            .await
            .expect("Core switch fact after rebind");
        }
        let events = store.read(&sid, 0, 4096).await.expect("journal");
        let order: Vec<_> = events
            .iter()
            .filter(|e| {
                e.payload
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|t| t == "model_selected" || t == "session_provider_rebound")
                    || e.payload
                        .get("item")
                        .and_then(|v| v.get("kind"))
                        .and_then(serde_json::Value::as_str)
                        == Some("provider_pair_switch_v1")
            })
            .map(|e| (e.seq, e.payload.clone()))
            .collect();
        println!(
            "A31 TARGET target={target_provider} late={late_marker} automatic_seq={automatic_seq} facts={order:?}"
        );
        drop(committer);
        drop(lease);
        hub.shutdown().await.expect("shutdown");
        store.close().await.expect("close");
        let (store, hub) = crate::session_hub::open_retention_test_hub(root.path())
            .await
            .expect("reopen");
        let pair = hub
            .run_output_pair(&sid, &run)
            .await
            .expect("pair")
            .expect("run");
        let mut restored = hub
            .session_metadata(&sid)
            .await
            .expect("metadata")
            .expect("session");
        pair.restore_route(&mut restored).expect("known route");
        restored.provider.clone_from(&pair.provider);
        restored.model.clone_from(&pair.model);
        restored.max_tokens = pair.max_tokens;
        let expected_route = haider_protocol::session::RunOutputRouteSnapshotV1::capture(&expected);
        let actual_route = haider_protocol::session::RunOutputRouteSnapshotV1::capture(&restored);
        println!(
            "A31 TARGET RESULT target={target_provider} late={late_marker} expected_provider={} actual_provider={} expected={expected_route:?} actual={actual_route:?}",
            expected.provider, restored.provider
        );
        if restored.provider != expected.provider || actual_route != expected_route {
            failures.push(format!("{target_provider}/late={late_marker}: expected provider {} route {expected_route:?}; got {} {actual_route:?}",expected.provider,restored.provider));
        }
        hub.shutdown().await.expect("shutdown");
        store.close().await.expect("close");
    }
    assert!(
        failures.is_empty(),
        "explicit rebind committed after automatic target resolution must survive target installation: {failures:#?}"
    );
}
