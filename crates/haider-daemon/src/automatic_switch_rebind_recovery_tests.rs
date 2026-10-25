#![allow(clippy::expect_used)]
use super::automatic_switch_rebind_tests::{
    BoundaryMode, StaleAutomaticFallback, ordering_rebind, start_ordering_run,
};
use super::tests::{
    RecordingRebindFactory, commit_test_rebind, create_rebind_test_session,
    rebound_recovery_headless_spec,
};
use super::*;

struct RecoveredFallbackFactory {
    inner: Arc<RecordingRebindFactory>,
    resolver: Arc<StaleAutomaticFallback>,
}
#[async_trait]
impl ProviderFactory for RecoveredFallbackFactory {
    async fn resolve_for_turn(
        &self,
        metadata: &SessionMetadataV1,
    ) -> Result<ResolvedTurnProvider, HaiderError> {
        let mut resolved = self.inner.resolve_for_turn(metadata).await?;
        resolved.account_alias = Some(
            metadata
                .resolved_route_alias
                .clone()
                .unwrap_or_else(|| "first-automatic".into()),
        );
        resolved.attempt_resolver = Some(self.resolver.clone());
        Ok(resolved)
    }
}

/// Rebind generations survive released selectors and complete migration pins.
/// Exercise the real recovered worker and Core fallback, rather than passing a
/// generation manually to the committer after reopening.
#[tokio::test]
async fn recovered_selector_free_route_can_switch_again_in_every_run_mode() {
    use haider_provider::{FakeProvider, FakeStep};
    for mode in [
        BoundaryMode::Native,
        BoundaryMode::Headless,
        BoundaryMode::Legacy,
    ] {
        let root = tempfile::tempdir().expect("profile");
        let (store, hub) = crate::session_hub::open_retention_test_hub(root.path())
            .await
            .expect("hub");
        let sid = SessionId::new(format!("recovered-future-{mode:?}"));
        let run = RunId::new("rebind-test-run");
        create_rebind_test_session(&hub, &sid).await;
        start_ordering_run(&hub, &sid, store.worker_generation(), mode).await;
        let request_json = if matches!(mode, BoundaryMode::Headless) {
            let mut spec = rebound_recovery_headless_spec();
            spec.max_output_tokens = 4096;
            serde_json::json!({"headless":spec}).to_string()
        } else {
            "{}".into()
        };
        let mut accepted = hub
            .accept_internal_turn(haider_core::TurnAcceptCommand {
                command_id: format!("accept-{sid}"),
                request_digest: format!("accept-{sid}"),
                request_json,
                session_id: sid.clone(),
                worker_generation: store.worker_generation(),
                run_id: run.clone(),
                agent_id: None,
                branch_id: None,
                text: if matches!(mode, BoundaryMode::Headless) {
                    "ordering run"
                } else {
                    "legacy rebind turn"
                }
                .into(),
                attachments: Vec::new(),
                mode: haider_protocol::DeliveryMode::Queue,
                queued_event_id: EventId::new(format!("queued-{sid}")),
                user_event_id: EventId::new(format!("user-{sid}")),
                active_event_id: EventId::new(format!("active-{sid}")),
                device_id: DeviceId::new("rebind-test-device"),
            })
            .await
            .expect("accepted receipt");
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
            device_id: DeviceId::new("recovered-future"),
            event_ids: Arc::new(EventIdGenerator::new("recovered-first")),
            turn_selection: Arc::new(tokio::sync::Mutex::new(original.clone())),
        };
        let selection_seq = haider_core::ProviderPairSwitchCommitter::commit(
            &committer,
            &haider_core::ProviderPairSwitch {
                run_id: run.clone(),
                switch_ordinal: 0,
                source_rebind_seq: original.selection_epoch.unwrap_or(0),
                target_account: "first-automatic".into(),
                from_provider: "fake".into(),
                from_model: "initial-model".into(),
                to_provider: "fake-next".into(),
                to_model: "first-model".into(),
                cause: haider_core::ProviderPairSwitchCause::FallbackChain,
            },
        )
        .await
        .expect("first automatic switch");
        append_payloads(&lease, &DeviceId::new("recovered-future"), &run, None, &EventIdGenerator::new("recovered-first-fact"), vec![EventPayload::Item(ItemEvent::Completed {
            item_id: haider_protocol::ids::ItemId::new("first-switch"), item: TurnItem::Extension { kind: "provider_pair_switch_v1".into(), data: serde_json::json!({"selection_seq":selection_seq,"to_provider":"fake-next","to_model":"first-model","resolved_alias":"first-automatic"}) },
        })]).await.expect("first fact");
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
        let mut restored = original.clone();
        pair.restore_route(&mut restored).expect("route");
        assert!(restored.provider_rebind_id.is_none());
        assert!(restored.account_alias.is_none());
        assert!(restored.provider_base_url.is_none());
        assert_eq!(
            pair.latest_rebind_seq,
            original.selection_epoch.unwrap_or(0)
        );
        let first = Arc::new(FakeProvider::new(vec![FakeStep::Error {
            kind: haider_provider::ProviderErrorKind::Authentication,
            message: "synthetic first route unavailable".into(),
            retry_after_ms: None,
        }]));
        let healthy = Arc::new(FakeProvider::new(vec![FakeStep::Finish {
            reason: haider_protocol::provider::FinishReason::EndTurn,
        }]));
        let inner = Arc::new(RecordingRebindFactory {
            provider: Some(first.clone()),
            ..Default::default()
        });
        let factory = Arc::new(RecoveredFallbackFactory {
            inner: inner.clone(),
            resolver: Arc::new(StaleAutomaticFallback {
                target: haider_core::ProviderPairSwitchTarget {
                    provider: healthy.clone(),
                    account: haider_protocol::ids::CredentialAlias::new("second-automatic"),
                    account_incarnation: Some(2),
                    provider_name: "fake".into(),
                    model: "after-recovery-model".into(),
                    context_window: Some(128_000),
                    cached_input_is_subset: true,
                    provider_request_state: Default::default(),
                    auth_scope: "none".into(),
                    attempt_resolver: None,
                    cause: haider_core::ProviderPairSwitchCause::FallbackChain,
                },
            }),
        });
        let manager = WorkerManager::start(
            hub.clone(),
            WorkerDependencies {
                provider_factory: factory,
                tool_factory: Arc::new(BrokerToolFactory),
                delegation: None,
                web_search: None,
                diagnostics: None,
            },
            false,
        );
        hub.install_worker_manager(manager.handle())
            .expect("worker");
        accepted.worker_generation = store.worker_generation();
        manager
            .handle()
            .recover_workflow_continuation(accepted, 1, 1)
            .await
            .expect("recover");
        let terminal = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                if let Some(state) = store
                    .read(&sid, 0, 4096)
                    .await
                    .expect("journal")
                    .iter()
                    .filter_map(|event| {
                        if event.run_id.as_ref() == Some(&run)
                            && let Ok(EventPayload::RunState(state)) = event.payload.decode_event()
                            && state.is_terminal()
                        {
                            Some(state)
                        } else {
                            None
                        }
                    })
                    .next_back()
                {
                    break state;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("recovered fallback settles");
        assert_eq!(terminal, RunState::Done, "mode={mode:?}");
        assert_eq!(first.requests().len(), 1, "mode={mode:?}");
        assert_eq!(healthy.requests().len(), 1, "mode={mode:?}");
        assert_eq!(healthy.requests()[0].model, "after-recovery-model");
        assert_eq!(inner.calls.lock().expect("calls").len(), 1);
        manager.shutdown().await.expect("stop worker");
        hub.shutdown().await.expect("shutdown");
        store.close().await.expect("close");
    }
}

#[tokio::test]
async fn core_continues_when_rebind_commits_between_route_read_and_store_install() {
    use haider_provider::{FakeProvider, FakeStep, Message};
    let root = tempfile::tempdir().expect("profile");
    let (store, hub) = crate::session_hub::open_retention_test_hub(root.path())
        .await
        .expect("hub");
    let sid = SessionId::new("core-atomic-fallback");
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
    let probe = hub.install_pair_switch_commit_probe(&sid).await;
    config.provider_pair_switch_committer = Some(Arc::new(DaemonProviderPairSwitchCommitter {
        store: lease.clone(),
        branch_id: None,
        device_id: DeviceId::new("core-ordering"),
        event_ids: Arc::new(EventIdGenerator::new("core-switch")),
        turn_selection: Arc::new(tokio::sync::Mutex::new(original.clone())),
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
    let turn = handle
        .submit_committed_turn(haider_core::SubmitCommittedTurn {
            run_id: run.clone(),
            messages: vec![Message::user_text("continue on the latest explicit route")],
        })
        .await
        .expect("submit");
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        probe.pair_switch_reached.notified(),
    )
    .await
    .expect("target reached the Store window");
    commit_test_rebind(&hub, &sid, store.worker_generation(), 1).await;
    probe.pair_switch_release.notify_one();
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(30), turn.wait())
        .await
        .expect("continuation timeout")
        .expect("outcome");
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
        Some("rebind-core-atomic-fallback-1")
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
