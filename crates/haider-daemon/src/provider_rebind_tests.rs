#![allow(clippy::expect_used)]

use super::*;
use haider_core::ProviderRebindResolver as _;

#[derive(Default)]
pub(super) struct RecordingRebindFactory {
    pub(super) calls: std::sync::Mutex<Vec<SessionMetadataV1>>,
    pub(super) active_no_auth: std::sync::atomic::AtomicBool,
    pub(super) provider: Option<Arc<haider_provider::FakeProvider>>,
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
            account_incarnation: Some(1),
            active_no_auth: self.active_no_auth.load(Ordering::Acquire),
            initial_rotation: None,
            rotation_budget_consumed: false,
            attempt_resolver: None,
            compaction_promotion: None,
        })
    }
}

pub(super) async fn create_rebind_test_session(hub: &SessionHub, session_id: &SessionId) {
    hub.create_internal_session(haider_core::SessionCreateCommand {
        command_id: format!("create-{session_id}"),
        request_digest: format!("create-digest-{session_id}"),
        request_json: "{}".into(),
        session_id: session_id.clone(),
        cwd: "/tmp".into(),
        provider: "fake".into(),
        model: "initial-model".into(),
        max_tokens: 4096,
        max_tokens_source: None,
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

// These resolver fixtures model legacy running turns without a durable pin.
// Their route resolution still belongs to an accepted run and its branch.
pub(super) async fn start_legacy_rebind_test_run(
    hub: &SessionHub,
    session_id: &SessionId,
    generation: u64,
) {
    let run = RunId::new("rebind-test-run");
    let device = DeviceId::new("rebind-test-device");
    hub.accept_internal_turn(haider_core::TurnAcceptCommand {
        command_id: format!("accept-{session_id}"),
        request_digest: format!("accept-{session_id}"),
        request_json: "{}".into(),
        session_id: session_id.clone(),
        worker_generation: generation,
        run_id: run.clone(),
        agent_id: None,
        branch_id: None,
        text: "legacy rebind turn".into(),
        attachments: Vec::new(),
        mode: haider_protocol::DeliveryMode::Queue,
        queued_event_id: EventId::new(format!("queued-{session_id}")),
        user_event_id: EventId::new(format!("user-{session_id}")),
        active_event_id: EventId::new(format!("active-{session_id}")),
        device_id: device.clone(),
    })
    .await
    .expect("accept legacy run");
    let lease = hub
        .acquire_worker_lease(session_id.clone())
        .await
        .expect("lease");
    append_payloads(
        &lease,
        &device,
        &run,
        None,
        &EventIdGenerator::new("legacy-start"),
        vec![EventPayload::RunState(RunState::Thinking)],
    )
    .await
    .expect("start legacy run");
}

pub(super) async fn commit_test_rebind(
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
        max_tokens: 4096,
        max_tokens_source: haider_protocol::output_budget::SessionOutputBudgetSourceV1::Derived,
        clamped: None,
        active_output_budget: None,
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
    start_legacy_rebind_test_run(&hub, &session_id, store.worker_generation()).await;
    hub.install_creatable_providers(std::collections::BTreeSet::from(["fake".to_owned()]))
        .expect("creatable provider");
    let profile = serde_json::from_value::<haider_rpc::ProviderSummaryWire>(serde_json::json!({
        "provider":"fake", "api_family":"openai_responses", "availability":"available",
        "enabled":true, "trust":"full", "inventory":{"state":"static"},
        "models":["initial-model","promoted-live-model","pending-next-turn-model"],
        "model_details":[
            {"name":"initial-model","max_output_tokens":8192,"max_output_tokens_source":"declared","context_window":1048576},
            {"name":"promoted-live-model","max_output_tokens":8192,"max_output_tokens_source":"declared","context_window":1048576},
            {"name":"pending-next-turn-model","max_output_tokens":30000,"max_output_tokens_source":"declared","context_window":1048576}
        ]
    })).expect("profile");
    hub.install_accounts(crate::accounts::AccountsFacade {
        login: None,
        oauth: None,
        snapshot: Arc::new(std::sync::Mutex::new(Vec::new())),
        management: crate::accounts::ManagementSnapshot::new(1, Vec::new(), vec![profile]),
        vault_supported: false,
        discovery_disabled: true,
        device_discovery: crate::accounts::DeviceDiscoverySnapshot::new(true),
        vault: None,
        sources: Arc::new(std::sync::Mutex::new(Vec::new())),
    })
    .expect("accounts");
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
        RunId::new("rebind-test-run"),
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
            automatic_run: None,
            account_alias: None,
            output_budget: None,
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
        target.max_tokens, 8192,
        "legacy rebind facts still re-derive for the active promoted model"
    );
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
    start_legacy_rebind_test_run(&hub, &session_id, store.worker_generation()).await;
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
        RunId::new("rebind-test-run"),
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
    start_legacy_rebind_test_run(&hub, &session_id, store.worker_generation()).await;
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
        RunId::new("rebind-test-run"),
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

pub(super) fn rebound_recovery_headless_spec() -> HeadlessRunSpecV1 {
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

#[tokio::test]
async fn headless_recovery_keeps_accepted_budget_across_prebound_and_queued_rebinds() {
    for rebind_before_admission in [true, false] {
        let root = tempfile::tempdir().expect("store root");
        let (store, hub) = crate::session_hub::open_retention_test_hub(root.path())
            .await
            .expect("hub");
        let session_id = SessionId::new("prebound-headless-budget");
        create_rebind_test_session(&hub, &session_id).await;
        if rebind_before_admission {
            commit_test_rebind(&hub, &session_id, store.worker_generation(), 1).await;
        }
        let run_id = RunId::new("prebound-headless-run");
        hub.accept_internal_turn(haider_core::TurnAcceptCommand {
            command_id: "prebound-accept".into(),
            request_digest: "prebound-accept-digest".into(),
            request_json: serde_json::json!({"headless": rebound_recovery_headless_spec()})
                .to_string(),
            session_id: session_id.clone(),
            worker_generation: store.worker_generation(),
            run_id: run_id.clone(),
            agent_id: None,
            branch_id: None,
            text: "headless pinned request".into(),
            attachments: Vec::new(),
            mode: haider_protocol::DeliveryMode::Queue,
            queued_event_id: EventId::new("prebound-queued"),
            user_event_id: EventId::new("prebound-user"),
            active_event_id: EventId::new("prebound-active"),
            device_id: DeviceId::new("prebound-device"),
        })
        .await
        .expect("accepted run");
        if !rebind_before_admission {
            commit_test_rebind(&hub, &session_id, store.worker_generation(), 1).await;
        }
        hub.shutdown().await.expect("shutdown before restore");
        store.close().await.expect("close before restore");
        let (store, hub) = crate::session_hub::open_retention_test_hub(root.path())
            .await
            .expect("reopen");
        let lease = hub
            .acquire_worker_lease(session_id.clone())
            .await
            .expect("lease");
        let mut metadata = lease
            .session_metadata()
            .await
            .expect("read")
            .expect("typed");
        assert_eq!(metadata.max_tokens, 4096);
        let spec = rebound_recovery_headless_spec();
        super::pin_headless_turn_metadata(&mut metadata, &spec, &lease, &run_id)
            .await
            .expect("pin earlier rebind");
        assert_eq!(metadata.max_tokens, 513);
        drop(lease);
        hub.shutdown().await.expect("hub shutdown");
        store.close().await.expect("store close");
    }
}

/// MUTATION CHECK: restore the old headless provider, use mutable metadata's
/// pending model, or call the generic lockdown binder with the rebound provider.
/// Any of those changes prevents the resumed turn from sending this request.
#[tokio::test]
async fn provider_rebind_cross_provider_active_recovery_preserves_route_run_model_and_authority() {
    for (headless, automatic_switch, native_pickup) in [
        (false, false, false),
        (true, false, false),
        (true, true, false),
        (false, false, true),
        (false, true, true),
    ] {
        let root = tempfile::tempdir().expect("store root");
        let (store, hub) = crate::session_hub::open_retention_test_hub(root.path())
            .await
            .expect("hub");
        let session_id = SessionId::new(format!(
            "rebind-recover-{headless}-{automatic_switch}-{native_pickup}"
        ));
        let run_id = RunId::new(format!("active-{session_id}"));
        let device_id = DeviceId::new("recovery-device");
        create_rebind_test_session(&hub, &session_id).await;
        let select_native =
            |id: &str, model: &str, budget| haider_store::SessionSelectModelCommand {
                command_id: id.into(),
                request_digest: format!("{id}-digest"),
                request_json: "{}".into(),
                session_id: session_id.clone(),
                worker_generation: store.worker_generation(),
                provider: "fake".into(),
                model: model.into(),
                expected_pair: None,
                automatic_run: None,
                output_budget: Some(haider_protocol::output_budget::SessionOutputBudgetV1 {
                    max_tokens: budget,
                    source: haider_protocol::output_budget::SessionOutputBudgetSourceV1::UserSet {
                        requested: budget,
                    },
                    clamped: None,
                }),
                event_id: EventId::new(format!("{id}-event")),
                device_id: device_id.clone(),
                account_alias: None,
            };
        if native_pickup {
            hub.select_session_model(select_native("native-admission", "initial-model", 20000))
                .await
                .expect("admission budget");
        }
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
        if native_pickup {
            let model = if automatic_switch {
                "promoted-live-model"
            } else {
                "initial-model"
            };
            hub.select_session_model(select_native("native-pickup", model, 6000))
                .await
                .expect("selection before pickup");
            append_payloads(
                &lease,
                &device_id,
                &run_id,
                None,
                &event_ids,
                vec![
                    haider_protocol::session::RunOutputPairPinnedV1 {
                        route_snapshot: None,
                        provider: "fake".into(),
                        model: model.into(),
                        max_tokens: 6000,
                        provider_route: None,
                        source:
                            haider_protocol::output_budget::SessionOutputBudgetSourceV1::UserSet {
                                requested: 6000,
                            },
                    }
                    .to_event(&run_id)
                    .expect("durable pickup pin"),
                ],
            )
            .await
            .expect("pin before crash");
            hub.select_session_model(select_native(
                "native-next-turn",
                "pending-next-turn-model",
                20000,
            ))
            .await
            .expect("next-turn selection");
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
        if automatic_switch && !native_pickup {
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
                output_budget: None,
                expected_pair: None,
                automatic_run: None,
                event_id: EventId::new("pending-model-selected"),
                device_id: device_id.clone(),
                account_alias: None,
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
            max_tokens: 8192,
            max_tokens_source:
                haider_protocol::output_budget::SessionOutputBudgetSourceV1::UserSet {
                    requested: 20_000,
                },
            clamped: Some(haider_protocol::output_budget::OutputBudgetClampV1 {
                requested: 20_000,
                max_output_tokens: 8192,
            }),
            active_output_budget: native_pickup.then(|| {
                haider_protocol::session::ActiveRunOutputBudgetV1 {
                    run_id: run_id.clone(),
                    model: if automatic_switch {
                        "promoted-live-model"
                    } else {
                        "initial-model"
                    }
                    .into(),
                    budget: haider_protocol::output_budget::SessionOutputBudgetV1 {
                        max_tokens: 6000,
                        source:
                            haider_protocol::output_budget::SessionOutputBudgetSourceV1::UserSet {
                                requested: 6000,
                            },
                        clamped: None,
                    },
                }
            }),
            event_id: EventId::new("recovery-rebound"),
            device_id: device_id.clone(),
        })
        .await
        .expect("durable cross-provider route");
        if !headless {
            hub.select_session_model(select_native(
                "native-after-rebind",
                "pending-next-turn-model",
                7000,
            ))
            .await
            .expect("ordinary selection after rebind is for the next turn");
        }
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
                    .next_back()
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
            assert_eq!(calls[0].max_tokens, 8192);
        }
        let requests = provider.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].model, expected_model);
        assert_eq!(
            requests[0].max_tokens,
            if native_pickup { 6000 } else { 8192 }
        );
        // Recovery writes a migration pin after the old rebind. Its route
        // must remain available when that earlier fact is outside the pin's
        // reduction boundary and the session now selects another pair.
        let recovered_pair = hub
            .run_output_pair(&session_id, &run_id)
            .await
            .expect("recovered run record")
            .expect("migration pin");
        assert!(recovered_pair.durably_pinned);
        let mut next_turn = hub
            .session_metadata(&session_id)
            .await
            .expect("next-turn metadata")
            .expect("typed session");
        recovered_pair
            .restore_route(&mut next_turn)
            .expect("complete run route");
        assert_eq!(
            next_turn.provider_rebind_id.as_deref(),
            Some("rebind-recovery")
        );
        assert_eq!(
            next_turn.provider_base_url.as_deref(),
            Some("http://127.0.0.1:31702/v1")
        );
        assert_eq!(next_turn.account_alias.as_deref(), Some("fake-b-account"));
        if native_pickup {
            assert_eq!(
                calls[0].max_tokens_source,
                Some(
                    haider_protocol::output_budget::SessionOutputBudgetSourceV1::UserSet {
                        requested: 6000
                    }
                )
            );
        }
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
                .expect_err("frozen mismatch must reject recovery");
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
    assert_eq!(
        rebound_active_turn_lockdown_snapshot(&hub, &session_id, &run_id, true, "fake-b", Full,)
            .expect("activate recovered Full authority"),
        None
    );
    assert_eq!(
        hub.bound_session_lockdown(&session_id)
            .expect("active recovered Full authority"),
        Some(("fake".into(), Full)),
        "explicit Full recovery activates the accepted authority without rebinding it"
    );
    assert!(
        rebound_turn_lockdown_snapshot(&hub, &session_id, &run_id, false, "fake-b", Full).is_err(),
        "ordinary recovery cannot invent an unjournaled route change"
    );
    hub.shutdown().await.expect("shutdown");
    store.close().await.expect("close");
}
#[tokio::test]
async fn astra_divergent_headless_recovery_reapplies_active_limit() {
    for (headless, automatic_switch) in [(true, false), (true, true)] {
        let root = tempfile::tempdir().expect("store root");
        let (store, hub) = crate::session_hub::open_retention_test_hub(root.path())
            .await
            .expect("hub");
        hub.install_creatable_providers(std::collections::BTreeSet::from([
            "fake".to_owned(),
            "fake-b".to_owned(),
        ]))
        .expect("creatable");
        let providers = ["fake", "fake-b"].into_iter().map(|p| serde_json::from_value::<haider_rpc::ProviderSummaryWire>(serde_json::json!({
            "provider":p,"api_family":"openai_responses","availability":"available","enabled":true,
            "models":["initial-model","promoted-live-model","pending-next-turn-model"],
            "model_details":[
                {"name":"initial-model","max_output_tokens":8192,"max_output_tokens_source":"declared","context_window":1048576},
                {"name":"promoted-live-model","max_output_tokens":8192,"max_output_tokens_source":"declared","context_window":1048576},
                {"name":"pending-next-turn-model","max_output_tokens":30000,"max_output_tokens_source":"declared","context_window":1048576}
            ]
        })).expect("summary")).collect();
        hub.install_accounts(crate::accounts::AccountsFacade {
            login: None,
            oauth: None,
            snapshot: Arc::new(std::sync::Mutex::new(Vec::new())),
            management: crate::accounts::ManagementSnapshot::new(1, Vec::new(), providers),
            vault_supported: false,
            discovery_disabled: true,
            device_discovery: crate::accounts::DeviceDiscoverySnapshot::new(true),
            vault: None,
            sources: Arc::new(std::sync::Mutex::new(Vec::new())),
        })
        .expect("accounts");
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
                output_budget: None,
                expected_pair: None,
                automatic_run: None,
                account_alias: None,
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
            max_tokens: 20_000,
            max_tokens_source:
                haider_protocol::output_budget::SessionOutputBudgetSourceV1::UserSet {
                    requested: 20_000,
                },
            clamped: None,
            active_output_budget: (!automatic_switch).then(|| {
                haider_protocol::session::ActiveRunOutputBudgetV1 {
                    run_id: run_id.clone(),
                    model: "initial-model".into(),
                    budget: haider_protocol::output_budget::SessionOutputBudgetV1 {
                        max_tokens: 8192,
                        source:
                            haider_protocol::output_budget::SessionOutputBudgetSourceV1::UserSet {
                                requested: 20_000,
                            },
                        clamped: Some(haider_protocol::output_budget::OutputBudgetClampV1 {
                            requested: 20_000,
                            max_output_tokens: 8192,
                        }),
                    },
                }
            }),
            event_id: EventId::new("recovery-rebound"),
            device_id: device_id.clone(),
        })
        .await
        .expect("durable cross-provider route");
        let fact = store
            .read(&session_id, 0, 1024)
            .await
            .expect("journal")
            .into_iter()
            .filter_map(|event| {
                haider_protocol::session::SessionProviderRebound::from_payload_value(&event.payload)
            })
            .next_back()
            .expect("rebind fact");
        if !automatic_switch {
            let active = fact
                .active_output_budget
                .expect("active budget in route transaction");
            assert_eq!(active.run_id, run_id);
            assert_eq!(active.budget.max_tokens, 8192);
            assert_eq!(
                active.budget.clamped.expect("typed active clamp").requested,
                20_000
            );
        }
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
                    .next_back()
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
            assert_eq!(calls[0].max_tokens, 8192);
        }
        let requests = provider.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].model, expected_model);
        println!(
            "ASTRA divergent headless recovery automatic_switch={automatic_switch} receipt_budget=20000 active_model={} provider_received={}",
            requests[0].model, requests[0].max_tokens
        );
        assert_eq!(requests[0].max_tokens, 8192);
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

/// The same model on a later route must not lend its alias or epoch to an
/// already pinned turn, including after the daemon has reopened its Store.
#[tokio::test]
async fn pinned_route_provenance_is_run_scoped_across_restart_and_next_turn() {
    let root = tempfile::tempdir().expect("profile");
    let session = SessionId::new("route-scope");
    let run = RunId::new("active-route-scope");
    let device = DeviceId::new("scope-device");
    let (store, hub) = crate::session_hub::open_retention_test_hub(root.path())
        .await
        .expect("hub");
    create_rebind_test_session(&hub, &session).await;
    commit_test_rebind(&hub, &session, store.worker_generation(), 1).await;
    hub.accept_internal_turn(haider_core::TurnAcceptCommand {
        command_id: "scope-accept".into(),
        request_digest: "scope-accept".into(),
        request_json: "{}".into(),
        session_id: session.clone(),
        worker_generation: store.worker_generation(),
        run_id: run.clone(),
        agent_id: None,
        branch_id: None,
        text: "run-scoped route request".into(),
        attachments: Vec::new(),
        mode: haider_protocol::DeliveryMode::Queue,
        queued_event_id: EventId::new("scope-queued"),
        user_event_id: EventId::new("scope-user"),
        active_event_id: EventId::new("scope-active"),
        device_id: device.clone(),
    })
    .await
    .expect("accepted run");
    let lease = hub
        .acquire_worker_lease(session.clone())
        .await
        .expect("lease");
    let metadata = lease
        .session_metadata()
        .await
        .expect("metadata")
        .expect("typed");
    let pin = haider_protocol::session::RunOutputPairPinnedV1 {
        route_snapshot: Some(haider_protocol::session::RunOutputRouteSnapshotV1::capture(
            &metadata,
        )),
        provider: metadata.provider.clone(),
        model: metadata.model.clone(),
        max_tokens: metadata.max_tokens,
        source: haider_protocol::output_budget::SessionOutputBudgetSourceV1::Derived,
        provider_route: haider_protocol::session::RunOutputProviderRouteV1::capture(&metadata),
    };
    let route_fact = |mut route: haider_protocol::session::RunOutputProviderRouteV1,
                      alias: &str| {
        route.resolved_alias = Some(alias.to_owned());
        route.resolved_seen = true;
        EventPayload::Item(haider_protocol::item::ItemEvent::Completed {
            item_id: haider_protocol::ids::ItemId::new(format!("scope-{alias}")),
            item: haider_protocol::item::TurnItem::Extension {
                kind: haider_protocol::session::RunOutputProviderRouteV1::RESOLVED_KIND.into(),
                data: serde_json::to_value(route).expect("route fact"),
            },
        })
    };
    append_payloads(
        &lease,
        &device,
        &run,
        None,
        &EventIdGenerator::new("scope-pin"),
        vec![
            route_fact(pin.provider_route.clone().expect("route"), "pre-pin-alias"),
            pin.to_event(&run).expect("pin"),
            EventPayload::RunState(RunState::Thinking),
        ],
    )
    .await
    .expect("pin run");
    let mut initial = metadata.clone();
    hub.run_output_pair(&session, &run)
        .await
        .expect("initial pair")
        .expect("initial pair")
        .restore_route(&mut initial)
        .expect("complete run route");
    assert_eq!(
        initial.resolved_route_alias, None,
        "pre-pin facts are excluded"
    );
    assert!(!initial.resolved_route_seen);
    lease
        .commit_turn_resolved_route(&run, &metadata, Some("active-alias"))
        .await
        .expect("resolve active");
    let published = lease
        .session_metadata()
        .await
        .expect("session")
        .expect("typed");
    assert!(
        published.resolved_route_seen,
        "a matching route publishes session truth"
    );
    assert_eq!(
        published.resolved_route_alias.as_deref(),
        Some("active-alias")
    );
    let selected = hub
        .select_session_model(haider_store::SessionSelectModelCommand {
            command_id: "next-route".into(),
            request_digest: "next-route".into(),
            request_json: "{}".into(),
            session_id: session.clone(),
            worker_generation: store.worker_generation(),
            provider: "fake-next".into(),
            model: metadata.model.clone(),
            expected_pair: None,
            automatic_run: None,
            account_alias: None,
            output_budget: None,
            event_id: EventId::new("next-route"),
            device_id: device.clone(),
        })
        .await
        .expect("release pin for next turn");
    let epoch = match selected {
        haider_store::SessionSelectModelOutcome::Committed { selected, .. }
        | haider_store::SessionSelectModelOutcome::IdempotentReplay { selected } => {
            selected.selected_seq
        }
    };
    let next = lease
        .session_metadata()
        .await
        .expect("next")
        .expect("typed");
    assert_eq!(next.route_reset_epoch, Some(epoch));
    assert!(next.provider_rebind_id.is_none());
    lease
        .commit_resolved_route(&next, Some("next-alias"))
        .await
        .expect("resolve next");
    drop(lease);
    hub.shutdown().await.expect("shutdown");
    drop(hub);
    store.close().await.expect("close");
    let (store, hub) = crate::session_hub::open_retention_test_hub(root.path())
        .await
        .expect("restart");
    let lease = hub
        .acquire_worker_lease(session.clone())
        .await
        .expect("new lease");
    let next = lease
        .session_metadata()
        .await
        .expect("next")
        .expect("typed");
    assert_eq!(next.provider, "fake-next");
    assert_eq!(next.resolved_route_alias.as_deref(), Some("next-alias"));
    let pair = hub
        .run_output_pair(&session, &run)
        .await
        .expect("pin")
        .expect("durable");
    let mut active = next.clone();
    pair.restore_route(&mut active).expect("complete run route");
    active.provider = pair.provider;
    assert_eq!(active.provider, "fake");
    assert_eq!(active.resolved_route_alias.as_deref(), Some("active-alias"));
    assert!(active.resolved_route_seen);
    assert_ne!(active.selection_epoch, next.selection_epoch);
    lease
        .commit_turn_resolved_route(&run, &active, Some("rotated-active-alias"))
        .await
        .expect("rotate restored run");
    assert_eq!(
        lease
            .session_metadata()
            .await
            .expect("session")
            .expect("typed"),
        next,
        "even a restored rotation cannot change next-turn metadata"
    );
    let mut again = next.clone();
    hub.run_output_pair(&session, &run)
        .await
        .expect("pair")
        .expect("pair")
        .restore_route(&mut again)
        .expect("complete run route");
    assert_eq!(
        again.resolved_route_alias.as_deref(),
        Some("rotated-active-alias")
    );
    // Only a historical fixture can contain a trailing fact: the worker
    // append door correctly refuses anything after its terminal state.
    let event_ids = EventIdGenerator::new("scope-terminal");
    let mut historical = [
        EventPayload::RunState(RunState::Done),
        route_fact(
            haider_protocol::session::RunOutputProviderRouteV1::capture(&again).expect("route"),
            "post-terminal-alias",
        ),
    ]
    .into_iter()
    .map(|payload| {
        supervisor_envelope(
            &lease,
            &device,
            None,
            Some(run.clone()),
            event_ids.next(),
            payload,
        )
        .expect("historical envelope")
    })
    .collect::<Vec<_>>();
    haider_core::StoreHandle::append(&store, &mut historical)
        .await
        .expect("historical terminal control");
    let mut completed = next.clone();
    hub.run_output_pair(&session, &run)
        .await
        .expect("completed pair")
        .expect("completed pair")
        .restore_route(&mut completed)
        .expect("complete run route");
    assert_eq!(
        completed.resolved_route_alias, again.resolved_route_alias,
        "post-terminal facts are excluded"
    );
    assert_eq!(
        lease
            .session_metadata()
            .await
            .expect("session")
            .expect("typed"),
        next
    );
    drop(lease);
    hub.shutdown().await.expect("shutdown");
    store.close().await.expect("close");
}

/// O21 (families 1/2/7): a mid-turn credential rotation on a run whose pinned route IS the
/// session's current rebind route. The epoch stamped on the turn's footprints must remain one
/// the session (and so the TUI meter) can match. Paired control: same steps without a rebind.
#[tokio::test]
async fn matching_rebind_route_publishes_account_and_rotation_epoch() {
    let mut rows = Vec::new();
    for rebound in [false, true] {
        let root = tempfile::tempdir().expect("profile");
        let session = SessionId::new(format!("o21-rot-{rebound}"));
        let run = RunId::new(format!("o21-rot-run-{rebound}"));
        let device = DeviceId::new("o21-device");
        let (store, hub) = crate::session_hub::open_retention_test_hub(root.path())
            .await
            .expect("hub");
        create_rebind_test_session(&hub, &session).await;
        if rebound {
            commit_test_rebind(&hub, &session, store.worker_generation(), 1).await;
        }
        hub.accept_internal_turn(haider_core::TurnAcceptCommand {
            command_id: "o21-rot-accept".into(),
            request_digest: "o21-rot-accept".into(),
            request_json: "{}".into(),
            session_id: session.clone(),
            worker_generation: store.worker_generation(),
            run_id: run.clone(),
            agent_id: None,
            branch_id: None,
            text: "rotation".into(),
            attachments: Vec::new(),
            mode: haider_protocol::DeliveryMode::Queue,
            queued_event_id: EventId::new("o21-rot-q"),
            user_event_id: EventId::new("o21-rot-u"),
            active_event_id: EventId::new("o21-rot-a"),
            device_id: device.clone(),
        })
        .await
        .expect("accept");
        let lease = hub
            .acquire_worker_lease(session.clone())
            .await
            .expect("lease");
        let md = lease.session_metadata().await.expect("md").expect("typed");
        let pin = haider_protocol::session::RunOutputPairPinnedV1 {
            route_snapshot: Some(haider_protocol::session::RunOutputRouteSnapshotV1::capture(
                &md,
            )),
            provider: md.provider.clone(),
            model: md.model.clone(),
            max_tokens: md.max_tokens,
            source: haider_protocol::output_budget::SessionOutputBudgetSourceV1::Derived,
            provider_route: haider_protocol::session::RunOutputProviderRouteV1::capture(&md),
        };
        append_payloads(
            &lease,
            &device,
            &run,
            None,
            &EventIdGenerator::new("o21-rot-pin"),
            vec![
                pin.to_event(&run).expect("pin"),
                EventPayload::RunState(RunState::Thinking),
            ],
        )
        .await
        .expect("pin");
        let first = lease
            .commit_turn_resolved_route(&run, &md, Some("acct-1"))
            .await
            .expect("first");
        let mut turn_md = lease.session_metadata().await.expect("md").expect("typed");
        hub.run_output_pair(&session, &run)
            .await
            .expect("p")
            .expect("p")
            .restore_route(&mut turn_md)
            .expect("complete run route");
        let unchanged = lease
            .commit_turn_resolved_route(&run, &turn_md, Some("acct-1"))
            .await
            .expect("unchanged account");
        assert_eq!(unchanged, first, "the same account preserves its epoch");
        let rotated = lease
            .commit_turn_resolved_route(&run, &turn_md, Some("acct-2"))
            .await
            .expect("rotate");
        let after = lease.session_metadata().await.expect("md").expect("typed");
        println!(
            "O21-ROT rebound={rebound} first_epoch={first} rotated_epoch={rotated} session_epoch={:?} session_alias={:?} seen={:?}",
            after.selection_epoch, after.resolved_route_alias, after.resolved_route_seen
        );
        rows.push((
            rebound,
            rotated,
            after.selection_epoch,
            after.resolved_route_alias.clone(),
        ));
        for alias in [None, Some("acct-3")] {
            let mut turn = lease
                .session_metadata()
                .await
                .expect("metadata")
                .expect("typed");
            hub.run_output_pair(&session, &run)
                .await
                .expect("pair")
                .expect("admitted")
                .restore_route(&mut turn)
                .expect("complete run route");
            let epoch = lease
                .commit_turn_resolved_route(&run, &turn, alias)
                .await
                .expect("resolve rotated route");
            let session = lease
                .session_metadata()
                .await
                .expect("metadata")
                .expect("typed");
            assert_eq!(
                session.selection_epoch,
                Some(epoch),
                "footprints and session share the rotation epoch"
            );
            assert_eq!(session.resolved_route_alias.as_deref(), alias);
            assert!(session.resolved_route_seen);
        }
        drop(lease);
        hub.shutdown().await.expect("shutdown");
        store.close().await.expect("close");
    }
    for (rebound, rotated, session_epoch, alias) in &rows {
        assert_eq!(
            Some(*rotated),
            *session_epoch,
            "rebound={rebound}: turn epoch the meter can match"
        );
        assert_eq!(
            alias.as_deref(),
            Some("acct-2"),
            "rebound={rebound}: shown account is the served one"
        );
    }
}

/// Route facts belong to the pinned run's branch, including when its restored
/// route has diverged from the next-turn session route. The unbranched control
/// remains in pinned_route_provenance_is_run_scoped_across_restart_and_next_turn.
#[tokio::test]
async fn pinned_route_resolution_keeps_the_active_run_branch() {
    let root = tempfile::tempdir().expect("profile");
    let session = SessionId::new("branch-route-scope");
    let source_run = RunId::new("branch-route-source");
    let run = RunId::new("branch-route-active");
    let branch = haider_protocol::ids::BranchId::new("route-branch");
    let device = DeviceId::new("branch-route-device");
    let (store, hub) = crate::session_hub::open_retention_test_hub(root.path())
        .await
        .expect("hub");
    create_rebind_test_session(&hub, &session).await;
    commit_test_rebind(&hub, &session, store.worker_generation(), 1).await;
    let generation = store.worker_generation();
    let turn = |run: &RunId, branch_id| haider_core::TurnAcceptCommand {
        command_id: format!("accept-{run}"),
        request_digest: format!("accept-{run}"),
        request_json: "{}".into(),
        session_id: session.clone(),
        worker_generation: generation,
        run_id: run.clone(),
        agent_id: None,
        branch_id,
        text: "branch route witness".into(),
        attachments: Vec::new(),
        mode: haider_protocol::DeliveryMode::Queue,
        queued_event_id: EventId::new(format!("queued-{run}")),
        user_event_id: EventId::new(format!("user-{run}")),
        active_event_id: EventId::new(format!("active-{run}")),
        device_id: device.clone(),
    };
    hub.accept_internal_turn(turn(&source_run, None))
        .await
        .expect("source turn");
    let source = store.read(&session, 0, 1024).await.expect("source journal");
    let (fork_node_id, fork_seq) = source
        .iter()
        .find_map(|event| match event.payload.decode_event() {
            Ok(EventPayload::NodeCommitted(node)) if event.run_id.as_ref() == Some(&source_run) => {
                Some((node.node, event.seq))
            }
            _ => None,
        })
        .expect("source prompt node");
    let mut source_done = source
        .iter()
        .find(|event| {
            event.run_id.as_ref() == Some(&source_run)
                && matches!(
                    event.payload.decode_event(),
                    Ok(EventPayload::RunState(RunState::Queued))
                )
        })
        .expect("source queued")
        .clone();
    source_done.event_id = EventId::new("branch-route-source-done");
    source_done.seq = 0;
    source_done.payload = serde_json::to_value(EventPayload::RunState(RunState::Done))
        .expect("source done payload")
        .into();
    hub.append(&mut [source_done]).await.expect("source done");
    store
        .create_branch(haider_store::BranchCreateCommand {
            command_id: "branch-route-create".into(),
            request_digest: "branch-route-create".into(),
            request_json: "{}".into(),
            session_id: session.clone(),
            worker_generation: generation,
            branch_id: branch.clone(),
            source_branch_id: None,
            fork_node_id,
            fork_seq,
            name: Some("route branch".into()),
            event_id: EventId::new("branch-route-created"),
            device_id: device.clone(),
        })
        .await
        .expect("create real branch");
    hub.accept_internal_turn(turn(&run, Some(branch.clone())))
        .await
        .expect("branched turn");
    let lease = hub
        .acquire_worker_lease(session.clone())
        .await
        .expect("lease");
    let metadata = lease
        .session_metadata()
        .await
        .expect("metadata")
        .expect("typed");
    let pin = haider_protocol::session::RunOutputPairPinnedV1 {
        route_snapshot: Some(haider_protocol::session::RunOutputRouteSnapshotV1::capture(
            &metadata,
        )),
        provider: metadata.provider.clone(),
        model: metadata.model.clone(),
        max_tokens: metadata.max_tokens,
        source: haider_protocol::output_budget::SessionOutputBudgetSourceV1::Derived,
        provider_route: haider_protocol::session::RunOutputProviderRouteV1::capture(&metadata),
    };
    append_payloads(
        &lease,
        &device,
        &run,
        Some(&branch),
        &EventIdGenerator::new("branch-route-pin"),
        vec![
            pin.to_event(&run).expect("pin"),
            EventPayload::RunState(RunState::Thinking),
        ],
    )
    .await
    .expect("branched pin");
    lease
        .commit_turn_resolved_route(&run, &metadata, Some("branch-active-alias"))
        .await
        .expect("matching branch resolution");
    assert_eq!(
        lease
            .session_metadata()
            .await
            .expect("session")
            .expect("typed")
            .resolved_route_alias
            .as_deref(),
        Some("branch-active-alias"),
        "a matching branched route still publishes session truth"
    );
    hub.select_session_model(haider_store::SessionSelectModelCommand {
        command_id: "branch-route-next".into(),
        request_digest: "branch-route-next".into(),
        request_json: "{}".into(),
        session_id: session.clone(),
        worker_generation: generation,
        provider: "fake-next".into(),
        model: metadata.model.clone(),
        expected_pair: None,
        automatic_run: None,
        account_alias: None,
        output_budget: None,
        event_id: EventId::new("branch-route-next"),
        device_id: device.clone(),
    })
    .await
    .expect("next-turn route");
    let next = lease
        .session_metadata()
        .await
        .expect("next")
        .expect("typed");
    lease
        .commit_resolved_route(&next, Some("branch-next-alias"))
        .await
        .expect("next-turn resolution");
    let next = lease
        .session_metadata()
        .await
        .expect("next")
        .expect("typed");
    let pair = hub
        .run_output_pair(&session, &run)
        .await
        .expect("pair")
        .expect("pin");
    let mut active = next.clone();
    pair.restore_route(&mut active).expect("complete run route");
    active.provider = pair.provider;
    active.model = pair.model;
    lease
        .commit_turn_resolved_route(&run, &active, Some("branch-rotated-alias"))
        .await
        .expect("diverged branch resolution");
    assert_eq!(
        lease
            .session_metadata()
            .await
            .expect("session")
            .expect("typed"),
        next,
        "a diverged branched resolution remains run-only"
    );
    let events = store.read(&session, 0, 1024).await.expect("route journal");
    let resolved = events
        .iter()
        .filter(|event| {
            event.run_id.as_ref() == Some(&run)
                && matches!(event.payload.decode_event(),
                Ok(EventPayload::Item(haider_protocol::item::ItemEvent::Completed {
                    item: haider_protocol::item::TurnItem::Extension { kind, .. }, ..
                })) if kind == haider_protocol::session::RunOutputProviderRouteV1::RESOLVED_KIND)
        })
        .collect::<Vec<_>>();
    assert_eq!(resolved.len(), 2, "matching and diverged resolution facts");
    for event in resolved {
        assert_eq!(
            event.branch_id.as_ref(),
            Some(&branch),
            "resolved route fact keeps the pinned run branch"
        );
    }
    drop(lease);
    hub.shutdown().await.expect("shutdown");
    store.close().await.expect("close");
    let (store, hub) = crate::session_hub::open_retention_test_hub(root.path())
        .await
        .expect("restart");
    let pair = hub
        .run_output_pair(&session, &run)
        .await
        .expect("pair")
        .expect("pin");
    let mut restored = next.clone();
    pair.restore_route(&mut restored)
        .expect("complete run route");
    assert_eq!(
        restored.resolved_route_alias.as_deref(),
        Some("branch-rotated-alias")
    );
    hub.shutdown().await.expect("shutdown");
    store.close().await.expect("close");
}

fn rotation_route_facts(
    events: &[haider_protocol::envelope::RawEnvelope],
    run: &RunId,
) -> Vec<(String, Option<u64>, Option<String>)> {
    events
        .iter()
        .filter(|event| event.run_id.as_ref() == Some(run))
        .filter_map(|event| match event.payload.decode_event() {
            Ok(EventPayload::Item(haider_protocol::item::ItemEvent::Completed {
                item_id,
                item: haider_protocol::item::TurnItem::Extension { kind, data },
            })) if kind == haider_protocol::session::RunOutputProviderRouteV1::RESOLVED_KIND => {
                Some((
                    item_id.as_str().to_owned(),
                    data.get("selection_epoch")
                        .and_then(serde_json::Value::as_u64),
                    data.get("resolved_alias")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned),
                ))
            }
            _ => None,
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn select_rotation_test_model(
    hub: &SessionHub,
    session: &SessionId,
    generation: u64,
    id: &str,
    provider: &str,
    model: &str,
    account: Option<&str>,
    budget: Option<u64>,
) {
    hub.select_session_model(haider_store::SessionSelectModelCommand {
        command_id: format!("rotation-{id}"),
        request_digest: format!("rotation-{id}"),
        request_json: "{}".into(),
        session_id: session.clone(),
        worker_generation: generation,
        provider: provider.into(),
        model: model.into(),
        expected_pair: None,
        automatic_run: None,
        account_alias: account.map(str::to_owned),
        output_budget: budget.map(|max_tokens| {
            haider_protocol::output_budget::SessionOutputBudgetV1 {
                max_tokens,
                source: haider_protocol::output_budget::SessionOutputBudgetSourceV1::UserSet {
                    requested: max_tokens,
                },
                clamped: None,
            }
        }),
        event_id: EventId::new(format!("rotation-{id}")),
        device_id: DeviceId::new("rotation-device"),
    })
    .await
    .expect("select");
}

pub(super) async fn change_rotation_test_session(
    hub: &SessionHub,
    session: &SessionId,
    generation: u64,
    model: &str,
    variant: &str,
) {
    match variant {
        "budget_reselect" => {
            select_rotation_test_model(
                hub,
                session,
                generation,
                "b",
                "fake",
                model,
                None,
                Some(3000),
            )
            .await
        }
        "model_roundtrip" => {
            select_rotation_test_model(
                hub,
                session,
                generation,
                "m1",
                "fake",
                "other-model",
                None,
                None,
            )
            .await;
            select_rotation_test_model(hub, session, generation, "m2", "fake", model, None, None)
                .await;
        }
        "cross_provider" => {
            select_rotation_test_model(
                hub,
                session,
                generation,
                "x",
                "fake-next",
                model,
                None,
                None,
            )
            .await
        }
        "same_provider_model" => {
            select_rotation_test_model(
                hub,
                session,
                generation,
                "s",
                "fake",
                "other-model",
                None,
                None,
            )
            .await
        }
        "account_pin" => {
            select_rotation_test_model(
                hub,
                session,
                generation,
                "p",
                "fake",
                model,
                Some("pin-b"),
                None,
            )
            .await
        }
        "live_rebind" => commit_test_rebind(hub, session, generation, 2).await,
        "control" => {}
        _ => unreachable!("unknown rotation test variant: {variant}"),
    }
}

async fn pin_rotation_test_run(
    hub: &SessionHub,
    session: &SessionId,
    run: &RunId,
    generation: u64,
) -> (crate::session_hub::HubStoreHandle, SessionMetadataV1, u64) {
    let device = DeviceId::new("rotation-device");
    let variant = session.as_str();
    hub.accept_internal_turn(haider_core::TurnAcceptCommand {
        command_id: format!("rotation-accept-{variant}"),
        request_digest: format!("rotation-accept-{variant}"),
        request_json: "{}".into(),
        session_id: session.clone(),
        worker_generation: generation,
        run_id: run.clone(),
        agent_id: None,
        branch_id: None,
        text: "rotation".into(),
        attachments: Vec::new(),
        mode: haider_protocol::DeliveryMode::Queue,
        queued_event_id: EventId::new(format!("rotation-q-{variant}")),
        user_event_id: EventId::new(format!("rotation-u-{variant}")),
        active_event_id: EventId::new(format!("rotation-a-{variant}")),
        device_id: device.clone(),
    })
    .await
    .expect("accept");
    let lease = hub
        .acquire_worker_lease(session.clone())
        .await
        .expect("lease");
    let md = lease.session_metadata().await.expect("md").expect("typed");
    let pin = haider_protocol::session::RunOutputPairPinnedV1 {
        route_snapshot: Some(haider_protocol::session::RunOutputRouteSnapshotV1::capture(
            &md,
        )),
        provider: md.provider.clone(),
        model: md.model.clone(),
        max_tokens: md.max_tokens,
        source: haider_protocol::output_budget::SessionOutputBudgetSourceV1::Derived,
        provider_route: haider_protocol::session::RunOutputProviderRouteV1::capture(&md),
    };
    append_payloads(
        &lease,
        &device,
        run,
        None,
        &EventIdGenerator::new(format!("rotation-pin-{variant}")),
        vec![
            pin.to_event(run).expect("pin"),
            EventPayload::RunState(RunState::Thinking),
        ],
    )
    .await
    .expect("pin");
    // Turn start, exactly as worker.rs:9644 then the turn_selection set-up.
    let e0 = lease
        .commit_turn_resolved_route(run, &md, Some("acct-1"))
        .await
        .expect("turn start");
    (lease, md, e0)
}

/// The real rotation committer and restart pickup must advance run provenance
/// on every served-account change, even after an epoch-only session divergence.
#[tokio::test]
async fn matching_rebind_route_rotations_keep_distinct_provenance() {
    use haider_core::ProviderRouteCommitter as _;
    let mut failures = Vec::new();
    for restart in [false, true] {
        for variant in [
            "control",
            "budget_reselect",
            "model_roundtrip",
            "cross_provider",
            "same_provider_model",
            "account_pin",
            "live_rebind",
        ] {
            let root = tempfile::tempdir().expect("profile");
            let session = SessionId::new(format!("rotation-{variant}-{restart}"));
            let run = RunId::new(format!("rotation-run-{variant}"));
            let (store, hub) = crate::session_hub::open_retention_test_hub(root.path())
                .await
                .expect("hub");
            let generation = store.worker_generation();
            create_rebind_test_session(&hub, &session).await;
            commit_test_rebind(&hub, &session, generation, 1).await;
            let (lease, md, e0) = pin_rotation_test_run(&hub, &session, &run, generation).await;
            let mut turn = md.clone();
            turn.selection_epoch = Some(e0);
            turn.resolved_route_alias = Some("acct-1".into());
            turn.resolved_route_seen = true;
            let committer = crate::worker::DaemonProviderRouteCommitter {
                store: lease.clone(),
                run_id: run.clone(),
                selection: Arc::new(tokio::sync::Mutex::new(turn)),
            };
            change_rotation_test_session(&hub, &session, generation, &md.model, variant).await;
            let probe = hub.install_resolved_route_probe(&session, false).await;
            let before = lease.session_metadata().await.expect("md").expect("typed");
            let e2 = committer
                .commit(
                    "fake",
                    &md.model,
                    &haider_protocol::ids::CredentialAlias::new("acct-2"),
                )
                .await
                .expect("rotate 2");
            let mid = lease.session_metadata().await.expect("md").expect("typed");
            let unchanged = committer
                .commit(
                    "fake",
                    &md.model,
                    &haider_protocol::ids::CredentialAlias::new("acct-2"),
                )
                .await
                .expect("unchanged route");
            assert_eq!(unchanged, e2, "same account keeps its run epoch: {variant}");
            assert_eq!(
                rotation_route_facts(&store.read(&session, 0, 4096).await.expect("journal"), &run)
                    .len(),
                2,
                "same account appends no duplicate fact: {variant}"
            );
            // Restart pickup restores the durable run route onto fresh session
            // metadata before resolving, exactly as worker.rs:8755 -> 9644.
            drop(committer);
            drop(lease);
            let (store, hub, lease, probe) = if restart {
                hub.shutdown().await.expect("shutdown before restart");
                store.close().await.expect("close before restart");
                let (store, hub) = crate::session_hub::open_retention_test_hub(root.path())
                    .await
                    .expect("restart");
                let lease = hub
                    .acquire_worker_lease(session.clone())
                    .await
                    .expect("restart lease");
                let probe = hub.install_resolved_route_probe(&session, false).await;
                (store, hub, lease, probe)
            } else {
                let lease = hub
                    .acquire_worker_lease(session.clone())
                    .await
                    .expect("lease");
                (store, hub, lease, probe)
            };
            let mut recovered = lease.session_metadata().await.expect("md").expect("typed");
            let pair = hub
                .run_output_pair(&session, &run)
                .await
                .expect("pair")
                .expect("durable pin");
            assert!(pair.durably_pinned);
            pair.restore_route(&mut recovered)
                .expect("complete run route");
            recovered.provider = pair.provider;
            recovered.model = pair.model;
            let attempts_before = probe.attempts.load(Ordering::SeqCst);
            let committer = crate::worker::DaemonProviderRouteCommitter {
                store: lease.clone(),
                run_id: run.clone(),
                selection: Arc::new(tokio::sync::Mutex::new(recovered.clone())),
            };
            let e3 = if restart {
                lease
                    .commit_turn_resolved_route(&run, &recovered, Some("acct-3"))
                    .await
                    .expect("recovery resolution")
            } else {
                committer
                    .commit(
                        "fake",
                        &md.model,
                        &haider_protocol::ids::CredentialAlias::new("acct-3"),
                    )
                    .await
                    .expect("rotate 3")
            };
            let diverged = !matches!(variant, "control" | "live_rebind");
            assert_eq!(
                probe.attempts.load(Ordering::SeqCst) - attempts_before,
                u64::from(!diverged),
                "{variant}/{restart}: only an equal-epoch matching route may request session publication"
            );
            let after = lease.session_metadata().await.expect("md").expect("typed");
            let events = store.read(&session, 0, 4096).await.expect("journal");
            let facts = rotation_route_facts(&events, &run);
            assert_eq!(
                facts
                    .iter()
                    .map(|(_, epoch, alias)| (*epoch, alias.as_deref()))
                    .collect::<Vec<_>>(),
                vec![
                    (Some(e0), Some("acct-1")),
                    (Some(e2), Some("acct-2")),
                    (Some(e3), Some("acct-3"))
                ],
                "{variant}/{restart}: every served account has its returned epoch and journal item"
            );
            let mut ids = facts.iter().map(|fact| fact.0.clone()).collect::<Vec<_>>();
            let total = ids.len();
            ids.sort();
            ids.dedup();
            println!(
                "ROTATION variant={variant} restart={restart} e0={e0} e2={e2} e3={e3} before=(epoch {:?}, alias {:?}, seen {}) mid=(epoch {:?}, alias {:?}) after=(epoch {:?}, alias {:?}, seen {}) facts={facts:?}",
                before.selection_epoch,
                before.resolved_route_alias,
                before.resolved_route_seen,
                mid.selection_epoch,
                mid.resolved_route_alias,
                after.selection_epoch,
                after.resolved_route_alias,
                after.resolved_route_seen,
            );
            if !(e0 < e2 && e2 < e3) {
                failures.push(format!("{variant}: acct-2 and acct-3 share epoch {e2}"));
            }
            if ids.len() != total {
                failures.push(format!("{variant}: duplicate run-route item ids {facts:?}"));
            }

            if diverged && after != before {
                failures.push(format!(
                    "{variant}: diverged run leaked into session metadata"
                ));
            }
            if matches!(variant, "control" | "live_rebind")
                && (after.resolved_route_alias.as_deref() != Some("acct-3")
                    || after.selection_epoch != Some(e3))
            {
                failures.push(format!(
                    "{variant}: matching route did not publish acct-3 at the turn epoch"
                ));
            }
            drop(committer);
            drop(lease);
            hub.shutdown().await.expect("shutdown");
            store.close().await.expect("close");
        }
    }
    assert!(
        failures.is_empty(),
        "rotation provenance failures: {failures:#?}"
    );
}

/// A selection after the matching-route read must keep the served account
/// run-scoped and assign a fresh epoch rather than publish a refused commit.
#[tokio::test]
async fn resolved_route_refusal_after_matching_read_advances_run_epoch() {
    for variant in [
        "budget_reselect",
        "model_roundtrip",
        "cross_provider",
        "same_provider_model",
        "account_pin",
        "live_rebind",
        "concurrent_resolution",
    ] {
        let root = tempfile::tempdir().expect("profile");
        let session = SessionId::new(format!("route-race-{variant}"));
        let run = RunId::new(format!("route-race-run-{variant}"));
        let (store, hub) = crate::session_hub::open_retention_test_hub(root.path())
            .await
            .expect("hub");
        let generation = store.worker_generation();
        create_rebind_test_session(&hub, &session).await;
        commit_test_rebind(&hub, &session, generation, 1).await;
        let (lease, md, first) = pin_rotation_test_run(&hub, &session, &run, generation).await;
        let mut turn = lease.session_metadata().await.expect("md").expect("typed");
        hub.run_output_pair(&session, &run)
            .await
            .expect("pair")
            .expect("pin")
            .restore_route(&mut turn)
            .expect("complete run route");
        let probe = hub.install_resolved_route_probe(&session, true).await;
        let racing_lease = lease.clone();
        let racing_run = run.clone();
        let commit = tokio::spawn(async move {
            racing_lease
                .commit_turn_resolved_route(&racing_run, &turn, Some("acct-2"))
                .await
        });
        // This boundary is reached only after the route/equal-epoch check,
        // but before the actor/Store's atomic compare-and-commit.
        probe.reached.notified().await;
        if variant == "concurrent_resolution" {
            hub.install_resolved_route_probe(&session, false).await;
            let current = lease.session_metadata().await.expect("md").expect("typed");
            lease
                .commit_resolved_route(&current, Some("session-acct"))
                .await
                .expect("concurrent route");
        } else {
            change_rotation_test_session(&hub, &session, generation, &md.model, variant).await;
        }
        let before = lease.session_metadata().await.expect("md").expect("typed");
        probe.release.notify_one();
        let epoch = commit.await.expect("task").expect("route commit");
        let after = lease.session_metadata().await.expect("md").expect("typed");
        let facts =
            rotation_route_facts(&store.read(&session, 0, 4096).await.expect("journal"), &run);
        println!("ROUTE-RACE variant={variant} first={first} epoch={epoch} facts={facts:?}");
        assert!(
            epoch > first,
            "{variant}: refused session publish still advances run provenance"
        );
        assert_eq!(
            before, after,
            "{variant}: refusal cannot leak the old route into the session"
        );
        assert_eq!(
            facts.len(),
            2,
            "{variant}: both served accounts are journaled"
        );
        assert_ne!(
            facts[0].0, facts[1].0,
            "{variant}: distinct journal item ids"
        );
        assert_eq!(facts[1].2.as_deref(), Some("acct-2"));
        drop(lease);
        hub.shutdown().await.expect("shutdown");
        store.close().await.expect("close");
    }
}

/// A diverged route's next epoch is allocated by the session actor together
/// with its run-route append. A selection, rebind or session resolution that
/// commits while the append is pending can never be given the same epoch, and
/// later resolutions in the run (including A->B->A) keep distinct provenance.
#[tokio::test]
async fn run_scoped_route_epoch_is_allocated_with_its_append() {
    use haider_core::ProviderRouteCommitter as _;
    let mut failures = Vec::new();
    for variant in [
        "budget_reselect",
        "model_roundtrip",
        "cross_provider",
        "same_provider_model",
        "account_pin",
        "live_rebind",
        "concurrent_resolution",
    ] {
        let root = tempfile::tempdir().expect("profile");
        let session = SessionId::new(format!("route-alloc-{variant}"));
        let run = RunId::new(format!("route-alloc-run-{variant}"));
        let (store, hub) = crate::session_hub::open_retention_test_hub(root.path())
            .await
            .expect("hub");
        let generation = store.worker_generation();
        create_rebind_test_session(&hub, &session).await;
        commit_test_rebind(&hub, &session, generation, 1).await;
        let (lease, md, e0) = pin_rotation_test_run(&hub, &session, &run, generation).await;
        let mut turn = lease.session_metadata().await.expect("md").expect("typed");
        hub.run_output_pair(&session, &run)
            .await
            .expect("pair")
            .expect("pin")
            .restore_route(&mut turn)
            .expect("complete run route");
        // An epoch-only divergence sends the next served account to the
        // run-scoped path.
        select_rotation_test_model(
            &hub,
            &session,
            generation,
            "diverge",
            "fake",
            &md.model,
            None,
            Some(2500),
        )
        .await;
        let probe = hub.install_run_route_append_probe(&session).await;
        let racing_lease = lease.clone();
        let racing_run = run.clone();
        let racing_turn = turn.clone();
        let commit = tokio::spawn(async move {
            racing_lease
                .commit_turn_resolved_route(&racing_run, &racing_turn, Some("acct-2"))
                .await
        });
        // Reached after the run-scoped decision, before epoch allocation.
        probe.run_route_reached.notified().await;
        if variant == "concurrent_resolution" {
            let current = lease.session_metadata().await.expect("md").expect("typed");
            lease
                .commit_resolved_route(&current, Some("session-acct"))
                .await
                .expect("concurrent route");
        } else {
            change_rotation_test_session(&hub, &session, generation, &md.model, variant).await;
        }
        let raced = lease.session_metadata().await.expect("md").expect("typed");
        probe.run_route_release.notify_one();
        let e2 = commit.await.expect("task").expect("run-scoped route");
        let raced_epoch = raced.selection_epoch.unwrap_or(0);
        if e2 <= raced_epoch {
            failures.push(format!(
                "{variant}: run epoch {e2} was not allocated after the racing session epoch {raced_epoch}"
            ));
        }
        assert_eq!(
            lease.session_metadata().await.expect("md").expect("typed"),
            raced,
            "{variant}: a run-scoped route never writes session metadata"
        );
        // Later resolutions in the same run, back to the first account and
        // then a third one, through the real committer.
        hub.install_resolved_route_probe(&session, false).await;
        let mut selection = turn.clone();
        selection.selection_epoch = Some(e2);
        selection.resolved_route_alias = Some("acct-2".into());
        selection.resolved_route_seen = true;
        let committer = crate::worker::DaemonProviderRouteCommitter {
            store: lease.clone(),
            run_id: run.clone(),
            selection: Arc::new(tokio::sync::Mutex::new(selection)),
        };
        if variant == "live_rebind" {
            let pair = hub
                .run_output_pair(&session, &run)
                .await
                .expect("rebound pair")
                .expect("run");
            let mut own = raced.clone();
            pair.restore_route(&mut own)
                .expect("complete rebound snapshot");
            assert_eq!(
                own.provider_rebind_id, raced.provider_rebind_id,
                "a stale completion cannot replace the explicit rebind"
            );
            assert_eq!(own.selection_epoch, Some(raced_epoch));
            assert_eq!(
                own.resolved_route_alias.as_deref(),
                Some("acct-1"),
                "the rebind carries its complete seen state"
            );
        }
        let mut epochs = vec![e0, e2];
        for account in ["acct-1", "acct-3"] {
            epochs.push(
                committer
                    .commit(
                        "fake",
                        &md.model,
                        &haider_protocol::ids::CredentialAlias::new(account),
                    )
                    .await
                    .expect("later rotation"),
            );
        }
        let facts =
            rotation_route_facts(&store.read(&session, 0, 4096).await.expect("journal"), &run);
        println!(
            "ROUTE-ALLOC variant={variant} e0={e0} raced_session_epoch={raced_epoch} epochs={epochs:?} facts={facts:?}"
        );
        assert_eq!(
            facts
                .iter()
                .map(|(_, _, alias)| alias.as_deref())
                .collect::<Vec<_>>(),
            if variant == "live_rebind" {
                // Acct-1 is already owned by the full rebind fact; resolving
                // it again is a no-op, not another account change.
                assert_eq!(epochs[2], raced_epoch);
                vec![Some("acct-1"), Some("acct-2"), Some("acct-3")]
            } else {
                vec![
                    Some("acct-1"),
                    Some("acct-2"),
                    Some("acct-1"),
                    Some("acct-3"),
                ]
            },
            "{variant}: every served account change is journaled"
        );
        let mut fact_epochs = facts.iter().map(|fact| fact.1).collect::<Vec<_>>();
        fact_epochs.sort_unstable();
        fact_epochs.dedup();
        let mut ids = facts.iter().map(|fact| fact.0.clone()).collect::<Vec<_>>();
        ids.sort();
        ids.dedup();
        if fact_epochs.len() != facts.len() || ids.len() != facts.len() {
            failures.push(format!(
                "{variant}: served accounts share a run epoch or item id: {facts:?}"
            ));
        }
        drop(committer);
        drop(lease);
        hub.shutdown().await.expect("shutdown");
        store.close().await.expect("close");
    }
    assert!(
        failures.is_empty(),
        "run-scoped epoch allocation failures: {failures:#?}"
    );
}

#[path = "automatic_switch_rebind_tests.rs"]
mod automatic_switch_rebind_tests;
