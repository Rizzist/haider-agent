#![allow(clippy::expect_used)]

use super::*;
use haider_core::ProviderRebindResolver as _;

#[derive(Default)]
struct RecordingRebindFactory {
    calls: std::sync::Mutex<Vec<SessionMetadataV1>>,
    active_no_auth: std::sync::atomic::AtomicBool,
    provider: Option<Arc<haider_provider::FakeProvider>>,
}

#[async_trait]
impl ProviderFactory for RecordingRebindFactory {
    async fn resolve_for_turn(
        &self,
        metadata: &SessionMetadataV1,
    ) -> Result<ResolvedTurnProvider, HaiderError> {
        self.calls
            .lock()
            .expect("record metadata")
            .push(metadata.clone());
        Ok(ResolvedTurnProvider {
            provider: self
                .provider
                .clone()
                .unwrap_or_else(|| Arc::new(haider_provider::FakeProvider::new(Vec::new()))),
            provider_name: metadata.provider.clone(),
            model: metadata.model.clone(),
            context_window: Some(128_000),
            account_alias: metadata.account_alias.clone(),
            active_no_auth: self.active_no_auth.load(Ordering::Acquire),
            initial_rotation: None,
            rotation_budget_consumed: false,
            attempt_resolver: None,
            compaction_promotion: None,
        })
    }
}

async fn create_rebind_test_session(hub: &SessionHub, session_id: &SessionId) {
    hub.create_internal_session(haider_core::SessionCreateCommand {
        command_id: format!("create-{session_id}"),
        request_digest: format!("create-digest-{session_id}"),
        request_json: "{}".into(),
        session_id: session_id.clone(),
        cwd: "/tmp".into(),
        provider: "fake".into(),
        model: "initial-model".into(),
        max_tokens: 4096,
        permission_overrides: None,
        effort: Some("low".into()),
        fast: false,
        cache_policy: Default::default(),
        system_prompt_version: "test-system".into(),
        event_id: EventId::new(format!("created-{session_id}")),
        device_id: DeviceId::new("rebind-test-device"),
    })
    .await
    .expect("create session");
    let run_id = RunId::new(format!("active-{session_id}"));
    hub.bind_lockdown_turn(
        session_id,
        &run_id,
        "fake",
        crate::auto_hermetic::ProviderLockdownPolicy::Full,
    )
    .expect("bind active Full policy");
    hub.activate_lockdown_turn(session_id, &run_id)
        .expect("activate frozen policy");
}

async fn commit_test_rebind(
    hub: &SessionHub,
    session_id: &SessionId,
    generation: u64,
    ordinal: u64,
) {
    hub.rebind_session_provider(haider_store::SessionProviderRebindCommand {
        command_id: format!("rebind-{session_id}-{ordinal}"),
        request_digest: format!("rebind-digest-{session_id}-{ordinal}"),
        request_json: format!(r#"{{"ordinal":{ordinal}}}"#),
        session_id: session_id.clone(),
        worker_generation: generation,
        provider: "fake".into(),
        base_url: Some("http://127.0.0.1:31701/v1".into()),
        account: Some("selected-account".into()),
        event_id: EventId::new(format!("rebound-{session_id}-{ordinal}")),
        device_id: DeviceId::new("rebind-test-device"),
    })
    .await
    .expect("commit rebind");
}

/// MUTATION CHECK: reuse resolver-construction metadata.model/effort/fast.
/// The next adapter would regress an automatic promotion or install a pending
/// ordinary model selection instead of the currently executing request lane.
#[tokio::test]
async fn provider_rebind_after_automatic_model_change_builds_current_live_request_configuration() {
    let root = tempfile::tempdir().expect("store root");
    let (store, hub) = crate::session_hub::open_retention_test_hub(root.path())
        .await
        .expect("hub");
    let session_id = SessionId::new("rebind-live-model");
    create_rebind_test_session(&hub, &session_id).await;
    let lease = hub
        .acquire_worker_lease(session_id.clone())
        .await
        .expect("lease");
    let initial = lease
        .session_metadata()
        .await
        .expect("metadata")
        .expect("typed metadata");
    let factory = Arc::new(RecordingRebindFactory::default());
    let resolver = DaemonProviderRebindResolver::new(
        lease.clone(),
        factory.clone(),
        initial,
        WebCapabilityDegrade::default(),
    );
    // An automatic promotion committed the active pair. A later explicit
    // selection is durable but remains pending until the next logical turn.
    for (ordinal, model, expected_pair) in [
        (
            1,
            "promoted-live-model",
            Some(("fake".to_owned(), "initial-model".to_owned())),
        ),
        (2, "pending-next-turn-model", None),
    ] {
        hub.select_session_model(haider_store::SessionSelectModelCommand {
            command_id: format!("select-{ordinal}"),
            request_digest: format!("select-digest-{ordinal}"),
            request_json: format!(r#"{{"model":"{model}"}}"#),
            session_id: session_id.clone(),
            worker_generation: store.worker_generation(),
            provider: "fake".into(),
            model: model.into(),
            expected_pair,
            event_id: EventId::new(format!("selected-{ordinal}")),
            device_id: DeviceId::new("rebind-test-device"),
        })
        .await
        .expect("select model");
    }
    commit_test_rebind(&hub, &session_id, store.worker_generation(), 1).await;
    let target = resolver
        .refresh("promoted-live-model", r#"{"effort":"high","fast":true}"#)
        .await
        .expect("refresh")
        .expect("rebound target");
    assert_eq!(
        target.account.as_ref().map(|alias| alias.as_str()),
        Some("selected-account")
    );
    let calls = factory.calls.lock().expect("calls").clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].model, "promoted-live-model");
    assert_eq!(calls[0].effort.as_deref(), Some("high"));
    assert!(calls[0].fast);
    assert_eq!(
        calls[0].provider_base_url.as_deref(),
        Some("http://127.0.0.1:31701/v1")
    );
    assert_eq!(
        lease
            .session_metadata()
            .await
            .expect("metadata")
            .expect("typed")
            .model,
        "pending-next-turn-model",
        "request-boundary pickup must not rewrite pending selection"
    );
    assert!(
        resolver
            .refresh("promoted-live-model", r#"{"effort":"high","fast":true}"#)
            .await
            .expect("unchanged revision")
            .is_none()
    );
    drop(resolver);
    drop(lease);
    hub.shutdown().await.expect("shutdown");
    store.close().await.expect("close");
}

#[tokio::test]
async fn provider_rebind_new_identity_rebuilds_same_route_using_current_model() {
    let root = tempfile::tempdir().expect("store root");
    let (store, hub) = crate::session_hub::open_retention_test_hub(root.path())
        .await
        .expect("hub");
    let session_id = SessionId::new("rebind-same-route");
    create_rebind_test_session(&hub, &session_id).await;
    let lease = hub
        .acquire_worker_lease(session_id.clone())
        .await
        .expect("lease");
    let initial = lease
        .session_metadata()
        .await
        .expect("metadata")
        .expect("typed");
    let factory = Arc::new(RecordingRebindFactory::default());
    let resolver = DaemonProviderRebindResolver::new(
        lease.clone(),
        factory.clone(),
        initial,
        WebCapabilityDegrade::default(),
    );
    commit_test_rebind(&hub, &session_id, store.worker_generation(), 1).await;
    let first = resolver
        .refresh("first-live-model", r#"{"effort":"low","fast":false}"#)
        .await
        .expect("first refresh")
        .expect("first target");
    commit_test_rebind(&hub, &session_id, store.worker_generation(), 2).await;
    let second = resolver
        .refresh("later-live-model", r#"{"effort":null,"fast":true}"#)
        .await
        .expect("second refresh")
        .expect("new command must rebuild even at same URL");
    assert_ne!(first.route_epoch, second.route_epoch);
    let calls = factory.calls.lock().expect("calls").clone();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].model, "first-live-model");
    assert_eq!(calls[1].model, "later-live-model");
    assert_eq!(calls[1].effort, None);
    assert!(calls[1].fast);
    drop(resolver);
    drop(lease);
    hub.shutdown().await.expect("shutdown");
    store.close().await.expect("close");
}

/// Account removal can turn a previously credentialed custom adapter into
/// a keyless one after the RPC accepted the route. The physical-request
/// boundary must reject the now-different policy before core installs it.
#[tokio::test]
async fn provider_rebind_rejects_keyless_adapter_resolved_after_full_policy_receipt() {
    let root = tempfile::tempdir().expect("store root");
    let (store, hub) = crate::session_hub::open_retention_test_hub(root.path())
        .await
        .expect("hub");
    let session_id = SessionId::new("rebind-account-disappeared");
    create_rebind_test_session(&hub, &session_id).await;
    let lease = hub
        .acquire_worker_lease(session_id.clone())
        .await
        .expect("lease");
    let initial = lease
        .session_metadata()
        .await
        .expect("metadata")
        .expect("typed");
    let factory = Arc::new(RecordingRebindFactory::default());
    let resolver = DaemonProviderRebindResolver::new(
        lease.clone(),
        factory.clone(),
        initial,
        WebCapabilityDegrade::default(),
    );
    commit_test_rebind(&hub, &session_id, store.worker_generation(), 1).await;
    factory.active_no_auth.store(true, Ordering::Release);
    let error = resolver
        .refresh("live-model", r#"{"effort":null,"fast":false}"#)
        .await
        .err()
        .expect("keyless resolution must not inherit Full tools");
    assert_eq!(error.code, ErrorCode::Busy);
    assert!(error.retryable);
    assert_eq!(
        hub.bound_session_lockdown(&session_id).expect("binding"),
        Some((
            "fake".into(),
            crate::auto_hermetic::ProviderLockdownPolicy::Full
        ))
    );
    // A rejected resolution must not consume the durable revision. Restoring
    // the credential before retry makes the same command eligible again.
    factory.active_no_auth.store(false, Ordering::Release);
    assert!(
        resolver
            .refresh("live-model", r#"{"effort":null,"fast":false}"#)
            .await
            .expect("restored credential")
            .is_some()
    );
    drop(resolver);
    drop(lease);
    hub.shutdown().await.expect("shutdown");
    store.close().await.expect("close");
}

fn rebound_recovery_headless_spec() -> HeadlessRunSpecV1 {
    serde_json::from_value(serde_json::json!({
        "provider": "fake",
        "model": "initial-model",
        "max_output_tokens": 513,
        "effort": "high",
        "fast": true,
        "permission_overrides": {},
        "budget": {}
    }))
    .expect("headless spec")
}

/// MUTATION CHECK: restore the old headless provider, use mutable metadata's
/// pending model, or call the generic lockdown binder with the rebound provider.
/// Any of those changes prevents the resumed turn from sending this request.
#[tokio::test]
async fn provider_rebind_cross_provider_active_recovery_preserves_route_run_model_and_authority() {
    for (headless, automatic_switch) in [(false, false), (true, false), (true, true)] {
        let root = tempfile::tempdir().expect("store root");
        let (store, hub) = crate::session_hub::open_retention_test_hub(root.path())
            .await
            .expect("hub");
        let session_id = SessionId::new(format!("rebind-recover-{headless}-{automatic_switch}"));
        let run_id = RunId::new(format!("active-{session_id}"));
        let device_id = DeviceId::new("recovery-device");
        create_rebind_test_session(&hub, &session_id).await;
        let accepted = hub
            .accept_internal_turn(haider_core::TurnAcceptCommand {
                command_id: "accept-recovery".into(),
                request_digest: "accept-recovery-digest".into(),
                request_json: "{}".into(),
                session_id: session_id.clone(),
                worker_generation: store.worker_generation(),
                run_id: run_id.clone(),
                agent_id: None,
                branch_id: None,
                text: "resume the accepted run".into(),
                attachments: Vec::new(),
                mode: haider_protocol::DeliveryMode::Queue,
                queued_event_id: EventId::new("recovery-queued"),
                user_event_id: EventId::new("recovery-user"),
                active_event_id: EventId::new("recovery-active"),
                device_id: device_id.clone(),
            })
            .await
            .expect("accept durable run");
        let lease = hub
            .acquire_worker_lease(session_id.clone())
            .await
            .expect("lease");
        let event_ids = EventIdGenerator::new(format!("recovery-{session_id}"));
        if headless {
            let mut envelopes =
                [
                    supervisor_raw_envelope(
                        &lease,
                        &device_id,
                        None,
                        Some(run_id.clone()),
                        event_ids.next(),
                        HeadlessRunEventPayload::HeadlessRunConfigured(
                            rebound_recovery_headless_spec(),
                        )
                        .to_payload_value()
                        .expect("headless payload"),
                    ),
                ];
            // Headless configuration is an admission-owned durable fact,
            // outside the worker envelope's writable payload family.
            haider_core::StoreHandle::append(&store, &mut envelopes)
                .await
                .expect("headless config");
        }
        append_payloads(
            &lease,
            &device_id,
            &run_id,
            None,
            &event_ids,
            vec![EventPayload::RunState(RunState::Thinking)],
        )
        .await
        .expect("run was active before recovery");
        if automatic_switch {
            append_payloads(
                &lease,
                &device_id,
                &run_id,
                None,
                &event_ids,
                vec![EventPayload::Item(ItemEvent::Completed {
                    item_id: haider_protocol::ids::ItemId::new("live-model-switch"),
                    item: TurnItem::Extension {
                        kind: "provider_pair_switch_v1".into(),
                        data: serde_json::json!({
                            "from_provider": "fake", "from_model": "initial-model",
                            "to_provider": "fake", "to_model": "promoted-live-model",
                            "why": "fallback_chain"
                        }),
                    },
                })],
            )
            .await
            .expect("automatic run model fact");
            let unrelated_run = RunId::new("unrelated-run");
            let mut historical = [
                EventPayload::RunState(RunState::Queued),
                EventPayload::Item(ItemEvent::Completed {
                    item_id: haider_protocol::ids::ItemId::new("unrelated-model-switch"),
                    item: TurnItem::Extension {
                        kind: "provider_pair_switch_v1".into(),
                        data: serde_json::json!({"to_provider": "fake", "to_model": "unrelated-model"}),
                    },
                }),
                EventPayload::RunState(RunState::Done),
            ].into_iter().map(|payload| supervisor_envelope(
                &lease, &device_id, None, Some(unrelated_run.clone()), event_ids.next(), payload,
            ).expect("historical envelope")).collect::<Vec<_>>();
            haider_core::StoreHandle::append(&store, &mut historical)
                .await
                .expect("unrelated completed run history");
        }
        if headless {
            hub.select_session_model(haider_store::SessionSelectModelCommand {
                command_id: "pending-selection".into(),
                request_digest: "pending-selection-digest".into(),
                request_json: "{}".into(),
                session_id: session_id.clone(),
                worker_generation: store.worker_generation(),
                provider: "fake".into(),
                model: "pending-next-turn-model".into(),
                expected_pair: None,
                event_id: EventId::new("pending-model-selected"),
                device_id: device_id.clone(),
            })
            .await
            .expect("pending ordinary model selection");
        }
        hub.rebind_session_provider(haider_store::SessionProviderRebindCommand {
            command_id: "rebind-recovery".into(),
            request_digest: "rebind-recovery-digest".into(),
            request_json: "{}".into(),
            session_id: session_id.clone(),
            worker_generation: store.worker_generation(),
            provider: "fake-b".into(),
            base_url: Some("http://127.0.0.1:31702/v1".into()),
            account: Some("fake-b-account".into()),
            event_id: EventId::new("recovery-rebound"),
            device_id: device_id.clone(),
        })
        .await
        .expect("durable cross-provider route");
        drop(lease);
        let provider = Arc::new(haider_provider::FakeProvider::new(vec![
            haider_provider::FakeStep::EmitText {
                text: "recovered answer".into(),
            },
            haider_provider::FakeStep::Finish {
                reason: haider_protocol::provider::FinishReason::EndTurn,
            },
        ]));
        let factory = Arc::new(RecordingRebindFactory {
            provider: Some(provider.clone()),
            ..RecordingRebindFactory::default()
        });
        let manager = WorkerManager::start(
            hub.clone(),
            WorkerDependencies {
                provider_factory: factory.clone(),
                tool_factory: Arc::new(BrokerToolFactory),
                delegation: None,
                web_search: None,
                diagnostics: None,
            },
            false,
        );
        hub.install_worker_manager(manager.handle())
            .expect("install recovered worker");
        manager
            .handle()
            // Resume this same active run at a workflow-continuation handoff.
            .recover_workflow_continuation(accepted, 1, 1)
            .await
            .expect("recover active run");
        // One fake response has no delay; 10 s is the existing runtime-test
        // scheduling budget, and the loop performs no network or backoff wait.
        let settled = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(state) = store
                    .read(&session_id, 0, 1024)
                    .await
                    .expect("journal")
                    .iter()
                    .filter_map(|event| {
                        if event.run_id.as_ref() != Some(&run_id) {
                            return None;
                        }
                        match event.payload.decode_event() {
                            Ok(EventPayload::RunState(state)) if state.is_terminal() => Some(state),
                            _ => None,
                        }
                    })
                    .last()
                {
                    break state;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert_eq!(
            settled.expect("rebound recovery reaches a terminal state"),
            RunState::Done,
            "recovery headless={headless} automatic_switch={automatic_switch}"
        );
        let calls = factory.calls.lock().expect("factory calls").clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].provider, "fake-b");
        assert_eq!(
            calls[0].provider_base_url.as_deref(),
            Some("http://127.0.0.1:31702/v1")
        );
        assert_eq!(calls[0].account_alias.as_deref(), Some("fake-b-account"));
        assert_eq!(
            calls[0].provider_rebind_id.as_deref(),
            Some("rebind-recovery")
        );
        let expected_model = if automatic_switch {
            "promoted-live-model"
        } else {
            "initial-model"
        };
        assert_eq!(calls[0].model, expected_model);
        if headless {
            assert_eq!(calls[0].effort.as_deref(), Some("high"));
            assert!(calls[0].fast);
            assert_eq!(calls[0].max_tokens, 513);
        }
        let requests = provider.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].model, expected_model);
        assert_eq!(
            hub.bound_lockdown_run(&session_id, &run_id)
                .expect("frozen authority"),
            Some((
                "fake".into(),
                crate::auto_hermetic::ProviderLockdownPolicy::Full
            ))
        );
        manager.shutdown().await.expect("manager shutdown");
        hub.shutdown().await.expect("hub shutdown");
        store.close().await.expect("store close");
    }
}

#[tokio::test]
async fn provider_rebind_recovery_rejects_changed_frozen_permissions_and_lockdown_provider() {
    use crate::auto_hermetic::ProviderLockdownPolicy::{AutoHermetic, Configured, Full};
    let root = tempfile::tempdir().expect("store root");
    let (store, hub) = crate::session_hub::open_retention_test_hub(root.path())
        .await
        .expect("hub");
    let session_id = SessionId::new("recovery-policy");
    for (ordinal, frozen, proposed, target) in [
        (0, Full, Configured, "fake-b"),
        (1, Full, AutoHermetic, "fake"),
        (2, Configured, Full, "fake"),
        (3, Configured, Configured, "fake-b"),
    ] {
        let run_id = RunId::new(format!("frozen-{ordinal}"));
        hub.bind_lockdown_turn(&session_id, &run_id, "fake", frozen)
            .expect("freeze source");
        let error =
            rebound_turn_lockdown_snapshot(&hub, &session_id, &run_id, true, target, proposed)
                .err()
                .expect("frozen mismatch must reject recovery");
        assert_eq!(error.code, ErrorCode::Busy);
        assert!(error.retryable);
        assert_eq!(
            hub.bound_lockdown_run(&session_id, &run_id)
                .expect("original binding"),
            Some(("fake".into(), frozen))
        );
    }
    let run_id = RunId::new("ordinary-full");
    hub.bind_lockdown_turn(&session_id, &run_id, "fake", Full)
        .expect("freeze Full");
    assert!(
        rebound_turn_lockdown_snapshot(&hub, &session_id, &run_id, false, "fake-b", Full).is_err(),
        "ordinary recovery cannot invent an unjournaled route change"
    );
    hub.shutdown().await.expect("shutdown");
    store.close().await.expect("close");
}
