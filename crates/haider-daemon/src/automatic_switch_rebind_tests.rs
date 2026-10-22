#![allow(clippy::expect_used)]
use super::*;

#[path = "automatic_switch_rebind_order_tests.rs"]
mod ordering;

#[path = "automatic_switch_rebind_recovery_tests.rs"]
mod recovery;

#[derive(Clone, Copy, Debug)]
enum BoundaryMode {
    Native,
    Headless,
    Legacy,
}

async fn start_ordering_run(
    hub: &SessionHub,
    sid: &SessionId,
    generation: u64,
    mode: BoundaryMode,
) {
    let run = RunId::new("rebind-test-run");
    if matches!(mode, BoundaryMode::Headless) {
        let mut spec = rebound_recovery_headless_spec();
        spec.max_output_tokens = 4096;
        hub.accept_internal_turn(haider_core::TurnAcceptCommand {
            command_id: format!("accept-{sid}"),
            request_digest: format!("accept-{sid}"),
            request_json: serde_json::json!({"headless":spec}).to_string(),
            session_id: sid.clone(),
            worker_generation: generation,
            run_id: run.clone(),
            agent_id: None,
            branch_id: None,
            text: "ordering run".into(),
            attachments: Vec::new(),
            mode: haider_protocol::DeliveryMode::Queue,
            queued_event_id: EventId::new(format!("queued-{sid}")),
            user_event_id: EventId::new(format!("user-{sid}")),
            active_event_id: EventId::new(format!("active-{sid}")),
            device_id: DeviceId::new("ordering"),
        })
        .await
        .expect("headless admission");
        let lease = hub.acquire_worker_lease(sid.clone()).await.expect("lease");
        append_payloads(
            &lease,
            &DeviceId::new("ordering"),
            &run,
            None,
            &EventIdGenerator::new("headless-start"),
            vec![EventPayload::RunState(RunState::Thinking)],
        )
        .await
        .expect("headless start");
    } else {
        start_legacy_rebind_test_run(hub, sid, generation).await;
    }
    if matches!(mode, BoundaryMode::Native) {
        let metadata = hub
            .session_metadata(sid)
            .await
            .expect("metadata")
            .expect("typed");
        let lease = hub.acquire_worker_lease(sid.clone()).await.expect("lease");
        let pin = haider_protocol::session::RunOutputPairPinnedV1 {
            provider: metadata.provider.clone(),
            model: metadata.model.clone(),
            max_tokens: metadata.max_tokens,
            source: haider_protocol::output_budget::SessionOutputBudgetSourceV1::Derived,
            provider_route: haider_protocol::session::RunOutputProviderRouteV1::capture(&metadata),
            route_snapshot: Some(haider_protocol::session::RunOutputRouteSnapshotV1::capture(
                &metadata,
            )),
        };
        append_payloads(
            &lease,
            &DeviceId::new("ordering"),
            &run,
            None,
            &EventIdGenerator::new("ordering-pin"),
            vec![pin.to_event(&run).expect("pin")],
        )
        .await
        .expect("native pin");
    }
}

async fn ordering_rebind(
    hub: &SessionHub,
    sid: &SessionId,
    generation: u64,
    ordinal: u64,
    provider: &str,
    selectors: &str,
) {
    hub.rebind_session_provider(haider_store::SessionProviderRebindCommand {
        command_id: format!("ordering-{sid}-{ordinal}"),
        request_digest: format!("ordering-{sid}-{ordinal}"),
        request_json: format!("{{\"ordinal\":{ordinal}}}"),
        session_id: sid.clone(),
        worker_generation: generation,
        provider: provider.into(),
        base_url: (selectors != "account" && selectors != "reset")
            .then(|| "http://127.0.0.1:31701/v1".into()),
        account: (selectors != "endpoint" && selectors != "reset")
            .then(|| "automatic-account".into()),
        max_tokens: 4096,
        max_tokens_source: haider_protocol::output_budget::SessionOutputBudgetSourceV1::Derived,
        clamped: None,
        active_output_budget: None,
        event_id: EventId::new(format!("ordering-rebind-{ordinal}")),
        device_id: DeviceId::new("ordering"),
    })
    .await
    .expect("explicit rebind");
}

async fn switch_rebind_variants(stale_target: bool) {
    let mut failures = Vec::new();
    for mode in [
        BoundaryMode::Native,
        BoundaryMode::Headless,
        BoundaryMode::Legacy,
    ] {
        for cause in [
            haider_core::ProviderPairSwitchCause::FallbackChain,
            haider_core::ProviderPairSwitchCause::CompactionGuard,
        ] {
            for target in ["fake", "fake-next"] {
                for rebound in ["fake", "fake-next", "third"] {
                    for selectors in ["both", "account", "endpoint", "reset", "multiple"] {
                        let root = tempfile::tempdir().expect("profile");
                        let (store, hub) = crate::session_hub::open_retention_test_hub(root.path())
                            .await
                            .expect("hub");
                        let sid = SessionId::new("ordering-variants");
                        let run = RunId::new("rebind-test-run");
                        create_rebind_test_session(&hub, &sid).await;
                        let original = hub
                            .session_metadata(&sid)
                            .await
                            .expect("metadata")
                            .expect("typed");
                        start_ordering_run(&hub, &sid, store.worker_generation(), mode).await;
                        let lease = hub.acquire_worker_lease(sid.clone()).await.expect("lease");
                        let committer = DaemonProviderPairSwitchCommitter {
                            store: lease.clone(),
                            branch_id: None,
                            device_id: DeviceId::new("ordering"),
                            event_ids: Arc::new(EventIdGenerator::new("ordering-commit")),
                            turn_selection: Arc::new(tokio::sync::Mutex::new(original.clone())),
                        };
                        let switch = haider_core::ProviderPairSwitch {
                            run_id: run.clone(),
                            switch_ordinal: 0,
                            source_rebind_seq: 0,
                            target_account: "automatic-account".into(),
                            from_provider: "fake".into(),
                            from_model: "initial-model".into(),
                            to_provider: target.into(),
                            to_model: "switched-model".into(),
                            cause,
                        };
                        let prepared = if stale_target {
                            None
                        } else {
                            Some(
                                haider_core::ProviderPairSwitchCommitter::commit(
                                    &committer, &switch,
                                )
                                .await
                                .expect("automatic commit"),
                            )
                        };
                        if selectors == "multiple" {
                            ordering_rebind(
                                &hub,
                                &sid,
                                store.worker_generation(),
                                1,
                                "third",
                                "account",
                            )
                            .await;
                        }
                        ordering_rebind(
                            &hub,
                            &sid,
                            store.worker_generation(),
                            2,
                            rebound,
                            selectors,
                        )
                        .await;
                        let expected = hub
                            .session_metadata(&sid)
                            .await
                            .expect("metadata")
                            .expect("typed");
                        let accepted = if stale_target {
                            let result = haider_core::ProviderPairSwitchCommitter::commit(
                                &committer, &switch,
                            )
                            .await;
                            if let Err(error) = &result
                                && error.code != ErrorCode::RevisionConflict
                            {
                                failures.push(format!("{mode:?}/{cause:?}/{target}/{rebound}/{selectors}: stale-target decline must preserve RevisionConflict, got {:?}", error.code));
                            }
                            result.ok()
                        } else {
                            prepared
                        };
                        if let Some(selection_seq) = accepted {
                            append_payloads(&lease, &DeviceId::new("ordering"), &run, None, &EventIdGenerator::new("ordering-fact"),
                    vec![EventPayload::Item(ItemEvent::Completed { item_id: haider_protocol::ids::ItemId::new("ordering-fact"),
                        item: TurnItem::Extension { kind: "provider_pair_switch_v1".into(), data: serde_json::json!({
                            "selection_seq":selection_seq, "from_provider":"fake", "from_model":"initial-model",
                            "to_provider":target, "to_model":"switched-model", "resolved_alias":"automatic-account", "why":cause.as_str()
                        }) } })]).await.expect("switch fact");
                        }
                        drop(committer);
                        drop(lease);
                        hub.shutdown().await.expect("shutdown");
                        store.close().await.expect("close");
                        let (store, hub) = crate::session_hub::open_retention_test_hub(root.path())
                            .await
                            .expect("reopen");
                        hub.bind_lockdown_turn(
                            &sid,
                            &run,
                            "fake",
                            crate::auto_hermetic::ProviderLockdownPolicy::Full,
                        )
                        .expect("restore accepted Full binding");
                        hub.activate_lockdown_turn(&sid, &run)
                            .expect("activate recovered binding");
                        let pair = hub
                            .run_output_pair(&sid, &run)
                            .await
                            .expect("pair")
                            .expect("run");
                        let mut restored = original.clone();
                        pair.restore_route(&mut restored).expect("route");
                        let expected_route =
                            haider_protocol::session::RunOutputRouteSnapshotV1::capture(&expected);
                        let actual_route =
                            haider_protocol::session::RunOutputRouteSnapshotV1::capture(&restored);
                        let label = format!(
                            "{mode:?}/{cause:?}/{target}/{rebound}/{selectors}/stale={stale_target}"
                        );
                        if pair.provider != expected.provider
                            || actual_route != expected_route
                            || (stale_target && accepted.is_some())
                        {
                            failures.push(format!("{label}: provider={} expected={} actual={actual_route:?} expected_route={expected_route:?}", pair.provider, expected.provider));
                        }
                        let lease = hub.acquire_worker_lease(sid.clone()).await.expect("lease");
                        let factory = Arc::new(RecordingRebindFactory::default());
                        let resolver = DaemonProviderRebindResolver::new(
                            lease.clone(),
                            run.clone(),
                            factory.clone(),
                            original.clone(),
                            WebCapabilityDegrade::default(),
                        );
                        let refreshed = resolver
                            .refresh("initial-model", r#"{"effort":null,"fast":false}"#)
                            .await
                            .expect("live resolver");
                        let calls = factory.calls.lock().expect("calls").clone();
                        if refreshed.is_none()
                            || calls.last().is_none_or(|metadata| {
                                metadata.provider != expected.provider
                                    || haider_protocol::session::RunOutputRouteSnapshotV1::capture(
                                        metadata,
                                    ) != expected_route
                            })
                        {
                            failures.push(format!(
                                "{label}: newer rebind was not sent to recording factory: {calls:?}"
                            ));
                        }
                        println!(
                            "ORDERING VARIANT {label} restored_provider={} refresh={}",
                            pair.provider,
                            refreshed.is_some()
                        );
                        drop(resolver);
                        drop(lease);
                        hub.shutdown().await.expect("shutdown");
                        store.close().await.expect("close");
                    }
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "automatic completion must preserve the newer explicit route: {failures:#?}"
    );
}

#[tokio::test]
async fn automatic_switch_delayed_fact_preserves_every_rebind_boundary() {
    switch_rebind_variants(false).await;
}
#[tokio::test]
async fn automatic_switch_pre_resolved_target_preserves_every_rebind_boundary() {
    switch_rebind_variants(true).await;
}

struct RebindBeforeAutomaticInstall {
    inner: DaemonProviderPairSwitchCommitter,
    hub: SessionHub,
    sid: SessionId,
    generation: u64,
}
impl std::fmt::Debug for RebindBeforeAutomaticInstall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RebindBeforeAutomaticInstall")
            .finish_non_exhaustive()
    }
}
#[async_trait]
impl haider_core::ProviderPairSwitchCommitter for RebindBeforeAutomaticInstall {
    async fn commit(&self, switch: &haider_core::ProviderPairSwitch) -> Result<u64, HaiderError> {
        commit_test_rebind(&self.hub, &self.sid, self.generation, 1).await;
        haider_core::ProviderPairSwitchCommitter::commit(&self.inner, switch).await
    }
}

#[derive(Debug)]
struct StaleAutomaticFallback {
    target: haider_core::ProviderPairSwitchTarget,
}
#[async_trait]
impl haider_core::ProviderAttemptResolver for StaleAutomaticFallback {
    async fn resolve(
        &self,
        _: &haider_protocol::ids::CredentialAlias,
        _: &haider_provider::ProviderError,
    ) -> Result<haider_core::ProviderAttemptDecision, HaiderError> {
        Ok(haider_core::ProviderAttemptDecision::Switch(
            self.target.clone(),
        ))
    }
}

#[tokio::test]
async fn core_continues_on_healthy_rebind_when_pre_resolved_fallback_is_declined() {
    use haider_provider::{FakeProvider, FakeStep, Message};
    let root = tempfile::tempdir().expect("profile");
    let (store, hub) = crate::session_hub::open_retention_test_hub(root.path())
        .await
        .expect("hub");
    let sid = SessionId::new("core-stale-fallback");
    let run = RunId::new("rebind-test-run");
    create_rebind_test_session(&hub, &sid).await;
    let original = hub
        .session_metadata(&sid)
        .await
        .expect("metadata")
        .expect("typed");
    start_ordering_run(&hub, &sid, store.worker_generation(), BoundaryMode::Native).await;
    let lease = hub.acquire_worker_lease(sid.clone()).await.expect("lease");
    let original_provider = Arc::new(FakeProvider::new(vec![FakeStep::Error {
        kind: haider_provider::ProviderErrorKind::Authentication,
        message: "synthetic account unavailable".into(),
        retry_after_ms: None,
    }]));
    let stale_provider = Arc::new(FakeProvider::new(vec![FakeStep::Finish {
        reason: haider_protocol::provider::FinishReason::EndTurn,
    }]));
    let healthy_provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitText {
            text: "newer rebind answer".into(),
        },
        FakeStep::Finish {
            reason: haider_protocol::provider::FinishReason::EndTurn,
        },
    ]));
    let factory = Arc::new(RecordingRebindFactory {
        provider: Some(healthy_provider.clone()),
        ..Default::default()
    });
    let mut config = haider_core::HarnessConfig::for_session(
        sid.clone(),
        DeviceId::new("core-ordering"),
        0,
        store.worker_generation(),
    );
    config.model = original.model.clone();
    config.max_tokens = original.max_tokens;
    config.reserved_output_tokens = original.max_tokens;
    config.usage_scope.provider = original.provider.clone();
    config.usage_scope.model = original.model.clone();
    config.usage_account = Some(haider_protocol::ids::CredentialAlias::new(
        "initial-account",
    ));
    config.selection_epoch = original.selection_epoch;
    config.provider_pair_switch_committer = Some(Arc::new(RebindBeforeAutomaticInstall {
        inner: DaemonProviderPairSwitchCommitter {
            store: lease.clone(),
            branch_id: None,
            device_id: DeviceId::new("core-ordering"),
            event_ids: Arc::new(EventIdGenerator::new("core-switch")),
            turn_selection: Arc::new(tokio::sync::Mutex::new(original.clone())),
        },
        hub: hub.clone(),
        sid: sid.clone(),
        generation: store.worker_generation(),
    }));
    config.provider_attempt_resolver = Some(Arc::new(StaleAutomaticFallback {
        target: haider_core::ProviderPairSwitchTarget {
            provider: stale_provider.clone(),
            account: haider_protocol::ids::CredentialAlias::new("automatic-account"),
            account_incarnation: Some(1),
            provider_name: "fake-next".into(),
            model: "switched-model".into(),
            context_window: Some(128_000),
            cached_input_is_subset: true,
            provider_request_state: Default::default(),
            auth_scope: "none".into(),
            attempt_resolver: None,
            cause: haider_core::ProviderPairSwitchCause::FallbackChain,
        },
    }));
    config.provider_rebind_resolver = Some(Arc::new(DaemonProviderRebindResolver::new(
        lease.clone(),
        run.clone(),
        factory.clone(),
        original,
        WebCapabilityDegrade::default(),
    )));
    let handle = haider_core::HarnessActor::spawn(
        config,
        original_provider.clone(),
        Arc::new(lease.clone()),
    );
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        handle
            .submit_committed_turn(haider_core::SubmitCommittedTurn {
                run_id: run.clone(),
                messages: vec![Message::user_text("continue on the latest explicit route")],
            })
            .await
            .expect("submit")
            .wait()
            .await
            .expect("outcome")
    })
    .await
    .expect("continuation timeout");
    assert_eq!(
        outcome.state,
        RunState::Done,
        "healthy rebind must continue the same Core run: {:?}",
        outcome.error
    );
    assert_eq!(original_provider.requests().len(), 1);
    assert!(stale_provider.requests().is_empty());
    assert_eq!(healthy_provider.requests().len(), 1);
    let calls = factory.calls.lock().expect("calls").clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].provider_rebind_id.as_deref(),
        Some("rebind-core-stale-fallback-1")
    );
    assert_eq!(calls[0].account_alias.as_deref(), Some("selected-account"));
    assert_eq!(
        calls[0].provider_base_url.as_deref(),
        Some("http://127.0.0.1:31701/v1")
    );
    assert_eq!(healthy_provider.requests()[0].model, "initial-model");
    let final_metadata = hub
        .session_metadata(&sid)
        .await
        .expect("metadata")
        .expect("typed");
    assert_eq!(final_metadata.provider, calls[0].provider);
    assert_eq!(final_metadata.account_alias, calls[0].account_alias);
    assert_eq!(final_metadata.provider_base_url, calls[0].provider_base_url);
    assert_eq!(
        final_metadata.provider_rebind_id,
        calls[0].provider_rebind_id
    );
    handle.stop().await.expect("Core actor stops");
    drop(handle);
    drop(lease);
    hub.shutdown().await.expect("shutdown");
    store.close().await.expect("close");
}

#[tokio::test]
async fn automatic_switch_after_selector_reset_retains_generation_without_disabling_future_switches()
 {
    let root = tempfile::tempdir().expect("profile");
    let (store, hub) = crate::session_hub::open_retention_test_hub(root.path())
        .await
        .expect("hub");
    let sid = SessionId::new("automatic-after-reset");
    let run = RunId::new("rebind-test-run");
    create_rebind_test_session(&hub, &sid).await;
    start_ordering_run(&hub, &sid, store.worker_generation(), BoundaryMode::Native).await;
    ordering_rebind(&hub, &sid, store.worker_generation(), 1, "fake", "reset").await;
    let original = hub
        .session_metadata(&sid)
        .await
        .expect("metadata")
        .expect("typed");
    let lease = hub.acquire_worker_lease(sid.clone()).await.expect("lease");
    let committer = DaemonProviderPairSwitchCommitter {
        store: lease.clone(),
        branch_id: None,
        device_id: DeviceId::new("reset"),
        event_ids: Arc::new(EventIdGenerator::new("reset-auto")),
        turn_selection: Arc::new(tokio::sync::Mutex::new(original.clone())),
    };
    let mut from = "fake";
    let mut model = "initial-model";
    for (ordinal, target, to_model) in [(0, "fake-next", "switch-one"), (1, "third", "switch-two")]
    {
        let seq = haider_core::ProviderPairSwitchCommitter::commit(
            &committer,
            &haider_core::ProviderPairSwitch {
                run_id: run.clone(),
                switch_ordinal: ordinal,
                source_rebind_seq: original.selection_epoch.unwrap_or(0),
                target_account: "automatic-account".into(),
                from_provider: from.into(),
                from_model: model.into(),
                to_provider: target.into(),
                to_model: to_model.into(),
                cause: haider_core::ProviderPairSwitchCause::FallbackChain,
            },
        )
        .await
        .expect("legitimate automatic switch");
        append_payloads(&lease,&DeviceId::new("reset"),&run,None,&EventIdGenerator::new(format!("reset-{ordinal}")),vec![EventPayload::Item(ItemEvent::Completed{item_id:haider_protocol::ids::ItemId::new(format!("reset-{ordinal}")),item:TurnItem::Extension{kind:"provider_pair_switch_v1".into(),data:serde_json::json!({"selection_seq":seq,"to_provider":target,"to_model":to_model,"resolved_alias":"automatic-account"})}})]).await.expect("switch fact");
        let pair = hub
            .run_output_pair(&sid, &run)
            .await
            .expect("pair")
            .expect("run");
        assert_eq!(pair.provider, target);
        assert_eq!(pair.model, to_model);
        from = target;
        model = to_model;
    }
    drop(committer);
    drop(lease);
    hub.shutdown().await.expect("shutdown");
    store.close().await.expect("close");
}

/// The actor's transaction must close the gap after the Hub route read.
#[tokio::test]
async fn automatic_target_transaction_fences_rebind_after_route_read() {
    let root = tempfile::tempdir().expect("profile");
    let (store, hub) = crate::session_hub::open_retention_test_hub(root.path())
        .await
        .expect("hub");
    let sid = SessionId::new("store-rebind-generation");
    let run = RunId::new("rebind-test-run");
    create_rebind_test_session(&hub, &sid).await;
    start_ordering_run(&hub, &sid, store.worker_generation(), BoundaryMode::Native).await;
    let original = hub
        .session_metadata(&sid)
        .await
        .expect("metadata")
        .expect("typed");
    let pair = hub
        .run_output_pair(&sid, &run)
        .await
        .expect("pair")
        .expect("run");
    ordering_rebind(&hub, &sid, store.worker_generation(), 1, "fake", "endpoint").await;
    let expected = hub
        .session_metadata(&sid)
        .await
        .expect("metadata")
        .expect("typed");
    let result = hub
        .select_session_model(haider_store::SessionSelectModelCommand {
            command_id: "store-stale-target".into(),
            request_digest: "store-stale-target".into(),
            request_json: "{}".into(),
            session_id: sid.clone(),
            worker_generation: store.worker_generation(),
            provider: "fake".into(),
            model: "switched-model".into(),
            expected_pair: Some(("fake".into(), "initial-model".into())),
            automatic_run: Some(haider_store::AutomaticRunSelection {
                run_id: run,
                branch_id: None,
                rebind_seq: pair.latest_rebind_seq,
                resolved_alias: "automatic-account".into(),
                selection_epoch: original.selection_epoch,
            }),
            account_alias: None,
            output_budget: None,
            event_id: EventId::new("store-stale-target"),
            device_id: DeviceId::new("ordering"),
        })
        .await;
    let error =
        result.expect_err("a rebind between the Hub read and Store commit must decline the target");
    assert!(
        matches!(
            error,
            crate::session_hub::SessionHubError::Store(HaiderError {
                code: ErrorCode::RevisionConflict,
                ..
            })
        ),
        "decline must preserve its typed generation conflict"
    );
    assert_eq!(
        hub.session_metadata(&sid)
            .await
            .expect("metadata")
            .expect("typed"),
        expected
    );
    hub.shutdown().await.expect("shutdown");
    store.close().await.expect("close");
}

#[tokio::test]
async fn ordinary_next_turn_selection_neither_cancels_automatic_switch_nor_donates_its_epoch() {
    next_turn_selection_control(false).await;
}

#[tokio::test]
async fn ordinary_next_turn_model_survives_selector_reset_before_automatic_switch() {
    next_turn_selection_control(true).await;
}

async fn next_turn_selection_control(reset_route: bool) {
    let root = tempfile::tempdir().expect("profile");
    let (store, hub) = crate::session_hub::open_retention_test_hub(root.path())
        .await
        .expect("hub");
    let sid = SessionId::new("automatic-next-turn-control");
    let run = RunId::new("rebind-test-run");
    create_rebind_test_session(&hub, &sid).await;
    start_ordering_run(&hub, &sid, store.worker_generation(), BoundaryMode::Native).await;
    let original = hub
        .session_metadata(&sid)
        .await
        .expect("metadata")
        .expect("typed");
    select_rotation_test_model(
        &hub,
        &sid,
        store.worker_generation(),
        "later-choice",
        "third",
        "later-model",
        None,
        None,
    )
    .await;
    if reset_route {
        ordering_rebind(&hub, &sid, store.worker_generation(), 1, "fake", "reset").await;
    }
    let next = hub
        .session_metadata(&sid)
        .await
        .expect("metadata")
        .expect("typed");
    let source_rebind_seq = if reset_route {
        next.selection_epoch.unwrap_or(0)
    } else {
        0
    };
    let expected_epoch = if reset_route {
        next.selection_epoch
    } else {
        original.selection_epoch
    };
    let lease = hub.acquire_worker_lease(sid.clone()).await.expect("lease");
    let committer = DaemonProviderPairSwitchCommitter {
        store: lease.clone(),
        branch_id: None,
        device_id: DeviceId::new("ordering"),
        event_ids: Arc::new(EventIdGenerator::new("next-auto")),
        turn_selection: Arc::new(tokio::sync::Mutex::new(original.clone())),
    };
    let seq = haider_core::ProviderPairSwitchCommitter::commit(
        &committer,
        &haider_core::ProviderPairSwitch {
            run_id: run.clone(),
            switch_ordinal: 0,
            source_rebind_seq,
            target_account: "automatic-account".into(),
            from_provider: "fake".into(),
            from_model: "initial-model".into(),
            to_provider: "fake-next".into(),
            to_model: "switched-model".into(),
            cause: haider_core::ProviderPairSwitchCause::FallbackChain,
        },
    )
    .await
    .expect("ordinary next-turn settings cannot cancel this run");
    append_payloads(&lease,&DeviceId::new("ordering"),&run,None,&EventIdGenerator::new("next-fact"),vec![EventPayload::Item(ItemEvent::Completed{item_id:haider_protocol::ids::ItemId::new("next-fact"),item:TurnItem::Extension{kind:"provider_pair_switch_v1".into(),data:serde_json::json!({"selection_seq":seq,"to_provider":"fake-next","to_model":"switched-model","resolved_alias":"automatic-account"})}})]).await.expect("switch fact");
    assert_eq!(
        hub.session_metadata(&sid)
            .await
            .expect("metadata")
            .expect("typed"),
        next
    );
    let pair = hub
        .run_output_pair(&sid, &run)
        .await
        .expect("pair")
        .expect("run");
    let mut restored = next.clone();
    pair.restore_route(&mut restored).expect("route");
    assert_eq!(pair.provider, "fake-next");
    assert_eq!(pair.model, "switched-model");
    assert_eq!(restored.selection_epoch, expected_epoch);
    drop(committer);
    drop(lease);
    hub.shutdown().await.expect("shutdown");
    store.close().await.expect("close");
}

#[tokio::test]
async fn legacy_automatic_receipt_owns_switch_after_reset_and_rejects_later_ordinary_lookalike() {
    for newer_rebind in [false, true] {
        let root = tempfile::tempdir().expect("profile");
        let (store, hub) = crate::session_hub::open_retention_test_hub(root.path())
            .await
            .expect("hub");
        let sid = SessionId::new("legacy-automatic-owner");
        let run = RunId::new("rebind-test-run");
        create_rebind_test_session(&hub, &sid).await;
        start_ordering_run(&hub, &sid, store.worker_generation(), BoundaryMode::Native).await;
        ordering_rebind(&hub, &sid, store.worker_generation(), 1, "fake", "reset").await;
        let original = hub
            .session_metadata(&sid)
            .await
            .expect("metadata")
            .expect("typed");
        let lease = hub.acquire_worker_lease(sid.clone()).await.expect("lease");
        let committer = DaemonProviderPairSwitchCommitter {
            store: lease.clone(),
            branch_id: None,
            device_id: DeviceId::new("legacy-owner"),
            event_ids: Arc::new(EventIdGenerator::new("legacy-auto")),
            turn_selection: Arc::new(tokio::sync::Mutex::new(original.clone())),
        };
        let selected_seq = haider_core::ProviderPairSwitchCommitter::commit(
            &committer,
            &haider_core::ProviderPairSwitch {
                run_id: run.clone(),
                switch_ordinal: 0,
                source_rebind_seq: original.selection_epoch.unwrap_or(0),
                target_account: "automatic-account".into(),
                from_provider: "fake".into(),
                from_model: "initial-model".into(),
                to_provider: "fake-next".into(),
                to_model: "switched-model".into(),
                cause: haider_core::ProviderPairSwitchCause::FallbackChain,
            },
        )
        .await
        .expect("automatic switch");
        let expected = if newer_rebind {
            ordering_rebind(&hub, &sid, store.worker_generation(), 2, "fake", "both").await;
            let rebound = hub
                .session_metadata(&sid)
                .await
                .expect("metadata")
                .expect("typed");
            // Equal target coordinates are not an automatic command receipt.
            select_rotation_test_model(
                &hub,
                &sid,
                store.worker_generation(),
                "ordinary-lookalike",
                "fake-next",
                "switched-model",
                None,
                None,
            )
            .await;
            rebound
        } else {
            let mut switched = original.clone();
            let mut route = haider_protocol::session::RunOutputRouteSnapshotV1::capture(&switched);
            route.clear_selectors();
            route.resolved_alias = Some("automatic-account".into());
            route.resolved_seen = true;
            route.restore(&mut switched);
            switched.provider = "fake-next".into();
            switched.model = "switched-model".into();
            switched
        };
        append_payloads(&lease,&DeviceId::new("legacy-owner"),&run,None,&EventIdGenerator::new("legacy-fact"),vec![EventPayload::Item(ItemEvent::Completed{item_id:haider_protocol::ids::ItemId::new("legacy-fact"),item:TurnItem::Extension{kind:"provider_pair_switch_v1".into(),data:serde_json::json!({"from_provider":"fake","from_model":"initial-model","to_provider":"fake-next","to_model":"switched-model","resolved_alias":"automatic-account"})}})]).await.expect("legacy switch fact");
        let legacy_selection = store
            .read(&sid, 0, 4096)
            .await
            .expect("journal")
            .into_iter()
            .find(|event| event.seq == selected_seq)
            .expect("selection envelope");
        drop(committer);
        drop(lease);
        hub.shutdown().await.expect("shutdown");
        store.close().await.expect("close");
        // Reproduce the previous writer's unscoped selection envelope exactly;
        // its existing receipt supplies the ownership proof after reopening.
        let selected_seq = i64::try_from(selected_seq).expect("SQLite sequence");
        let connection = rusqlite::Connection::open(root.path().join("store.sqlite"))
            .expect("private fixture database");
        let names = connection
            .prepare(
                "SELECT name FROM sqlite_master WHERE type = 'trigger' AND tbl_name = 'events'",
            )
            .expect("fixture triggers")
            .query_map([], |row| row.get::<_, String>(0))
            .expect("trigger names")
            .collect::<Result<Vec<_>, _>>()
            .expect("triggers");
        for name in names {
            connection
                .execute_batch(&format!("DROP TRIGGER \"{}\"", name.replace('"', "\"\"")))
                .expect("owned fixture trigger");
        }
        let mut value = serde_json::to_value(&legacy_selection).expect("selection envelope");
        value["run_id"] = serde_json::Value::Null;
        value["payload"]
            .as_object_mut()
            .expect("payload")
            .remove("automatic_rebind_seq");
        value["payload"]
            .as_object_mut()
            .expect("payload")
            .remove("automatic_only");
        value["payload"]
            .as_object_mut()
            .expect("payload")
            .remove("automatic_resolved_alias");
        connection
            .execute(
                "UPDATE events SET envelope_json=?1 WHERE session_id=?2 AND seq=?3",
                rusqlite::params![
                    serde_json::to_string(&value).expect("legacy envelope"),
                    sid.as_str(),
                    selected_seq
                ],
            )
            .expect("legacy selection");
        drop(connection);
        let (store, hub) = crate::session_hub::open_retention_test_hub(root.path())
            .await
            .expect("reopen");
        hub.bind_lockdown_turn(
            &sid,
            &run,
            "fake",
            crate::auto_hermetic::ProviderLockdownPolicy::Full,
        )
        .expect("restore accepted Full binding");
        hub.activate_lockdown_turn(&sid, &run)
            .expect("activate recovered binding");
        let pair = hub
            .run_output_pair(&sid, &run)
            .await
            .expect("pair")
            .expect("run");
        let mut restored = original.clone();
        pair.restore_route(&mut restored).expect("route");
        assert_eq!(
            pair.provider, expected.provider,
            "newer_rebind={newer_rebind}"
        );
        assert_eq!(
            haider_protocol::session::RunOutputRouteSnapshotV1::capture(&restored),
            haider_protocol::session::RunOutputRouteSnapshotV1::capture(&expected),
            "newer_rebind={newer_rebind}"
        );
        if newer_rebind {
            let lease = hub.acquire_worker_lease(sid.clone()).await.expect("lease");
            let factory = Arc::new(RecordingRebindFactory::default());
            let resolver = DaemonProviderRebindResolver::new(
                lease.clone(),
                run,
                factory.clone(),
                original,
                WebCapabilityDegrade::default(),
            );
            assert!(
                resolver
                    .refresh("switched-model", r#"{"effort":null,"fast":false}"#)
                    .await
                    .expect("live boundary")
                    .is_some()
            );
            let calls = factory.calls.lock().expect("calls").clone();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].provider, expected.provider);
            assert_eq!(
                haider_protocol::session::RunOutputRouteSnapshotV1::capture(&calls[0]),
                haider_protocol::session::RunOutputRouteSnapshotV1::capture(&expected)
            );
            drop(resolver);
            drop(lease);
        }
        hub.shutdown().await.expect("shutdown");
        store.close().await.expect("close");
    }
}
