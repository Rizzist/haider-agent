use super::*;

pub(super) async fn create_child_route_parent(
    hub: &SessionHub,
    session: &SessionId,
    root: &std::path::Path,
) {
    let workspace = root.join("workspace");
    std::fs::create_dir(&workspace).expect("owned child workspace");
    let cwd = std::fs::canonicalize(workspace)
        .expect("canonical workspace")
        .to_string_lossy()
        .into_owned();
    hub.create_internal_session(haider_core::SessionCreateCommand {
        command_id: format!("create-{session}"),
        request_digest: format!("create-digest-{session}"),
        request_json: "{}".into(),
        session_id: session.clone(),
        cwd,
        provider: "fake".into(),
        model: "initial-model".into(),
        max_tokens: 4096,
        max_tokens_source: None,
        permission_overrides: None,
        effort: Some("low".into()),
        fast: false,
        cache_policy: Default::default(),
        system_prompt_version: "test-system".into(),
        event_id: EventId::new(format!("created-{session}")),
        device_id: DeviceId::new("rebind-test-device"),
    })
    .await
    .expect("create child-capable parent");
}

pub(super) fn route_json(metadata: &SessionMetadataV1) -> serde_json::Value {
    serde_json::json!({
        "rebind_id": metadata.provider_rebind_id, "base_url": metadata.provider_base_url,
        "account": metadata.account_alias, "resolved_alias": metadata.resolved_route_alias,
        "resolved_seen": metadata.resolved_route_seen, "selection_epoch": metadata.selection_epoch,
        "reset_epoch": metadata.route_reset_epoch,
    })
}

pub(super) fn boundary_request(binding: &str, metadata: &SessionMetadataV1) -> String {
    if binding.starts_with("headless_") {
        let mut spec = rebound_recovery_headless_spec();
        spec.provider.clone_from(&metadata.provider);
        spec.model.clone_from(&metadata.model);
        spec.max_output_tokens = metadata.max_tokens;
        serde_json::json!({"headless": spec}).to_string()
    } else {
        "{}".into()
    }
}

pub(super) async fn accept_headless_boundary(
    hub: &SessionHub,
    session: &SessionId,
    generation: u64,
    metadata: &SessionMetadataV1,
) {
    hub.accept_internal_turn(haider_core::TurnAcceptCommand {
        command_id: format!("accept-{session}"),
        request_digest: format!("accept-{session}"),
        request_json: boundary_request("headless_account", metadata),
        session_id: session.clone(),
        worker_generation: generation,
        run_id: RunId::new("rebind-test-run"),
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
    .expect("headless admission");
}

pub(super) async fn inherited_session(
    store: &haider_core::SqliteStoreHandle,
    hub: &SessionHub,
    source: &SessionId,
    kind: &str,
    failures: &mut Vec<String>,
) -> SessionId {
    let child = SessionId::new(format!("child-{source}"));
    let mut original = hub
        .session_metadata(source)
        .await
        .expect("metadata")
        .expect("typed");
    if kind == "endpoint_only" {
        original.account_alias = None;
        original.provider_rebind_id = None;
    }
    let expected_route = route_json(&original);
    if kind == "create" || kind == "endpoint_only" {
        hub.create_internal_session_with_interaction_mode(
            haider_core::SessionCreateCommand {
                command_id: format!("create-{child}"),
                request_digest: format!("create-{child}"),
                request_json: "{}".into(),
                session_id: child.clone(),
                cwd: original.cwd.clone(),
                provider: original.provider.clone(),
                model: original.model.clone(),
                max_tokens: original.max_tokens,
                max_tokens_source: original.max_tokens_source,
                permission_overrides: None,
                effort: original.effort.clone(),
                fast: original.fast,
                cache_policy: original.cache_policy,
                system_prompt_version: "test-system".into(),
                event_id: EventId::new(format!("created-{child}")),
                device_id: DeviceId::new("rebind-test-device"),
            },
            original.interaction_mode,
            None,
            Some(original),
        )
        .await
        .expect("inherited create");
    } else {
        hub.accept_internal_turn(haider_core::TurnAcceptCommand {
            command_id: "fork-source-accept".into(),
            request_digest: "fork-source-accept".into(),
            request_json: "{}".into(),
            session_id: source.clone(),
            worker_generation: store.worker_generation(),
            run_id: RunId::new("fork-source-run"),
            agent_id: None,
            branch_id: None,
            text: "source prompt".into(),
            attachments: Vec::new(),
            mode: haider_protocol::DeliveryMode::Queue,
            queued_event_id: EventId::new("fork-source-queued"),
            user_event_id: EventId::new("fork-source-user"),
            active_event_id: EventId::new("fork-source-active"),
            device_id: DeviceId::new("rebind-test-device"),
        })
        .await
        .expect("fork source prompt");
        let lease = hub
            .acquire_worker_lease(source.clone())
            .await
            .expect("source lease");
        append_payloads(
            &lease,
            &DeviceId::new("fork-source-device"),
            &RunId::new("fork-source-run"),
            None,
            &EventIdGenerator::new("fork-source-end"),
            vec![EventPayload::RunState(RunState::Done)],
        )
        .await
        .expect("terminal source turn");
        drop(lease);
        let journal = store.read(source, 0, 4096).await.expect("source journal");
        let (fork_node_id, fork_seq) = journal
            .iter()
            .find_map(|event| match event.payload.decode_event() {
                Ok(EventPayload::NodeCommitted(node)) => Some((node.node, event.seq)),
                _ => None,
            })
            .expect("fork node");
        let mut command = haider_store::SessionForkCommand {
            command_id: format!("fork-{child}"),
            request_digest: format!("fork-{child}"),
            request_json: "{}".into(),
            source_session_id: source.clone(),
            session_id: child.clone(),
            worker_generation: store.worker_generation(),
            source_branch_id: None,
            fork_node_id,
            fork_seq,
            name: None,
            metafork: None,
            audit_event_id: EventId::new(format!("forked-{child}")),
            device_id: DeviceId::new("rebind-test-device"),
        };
        if kind == "metafork" {
            let user_seq = journal
                .iter()
                .find(|event| {
                    matches!(
                        event.payload.decode_event(),
                        Ok(EventPayload::UserMessage { .. })
                    )
                })
                .expect("source user prompt")
                .seq;
            let model_proposal = haider_protocol::session_fork::SessionMetaforkProposal {
                removals: vec![haider_protocol::session_fork::SessionMetaforkRemoval {
                    from_seq: user_seq,
                    through_seq: user_seq,
                    reason: "omit source prompt".into(),
                    preview: None,
                    reviewed_events: Vec::new(),
                }],
            };
            let description = "historical route fixture".to_owned();
            let digest = haider_protocol::session_fork::SessionMetaforkReviewManifest {
                command_id: command.command_id.clone(),
                source_session_id: source.clone(),
                worker_generation: store.worker_generation(),
                source_branch_id: None,
                fork_node_id: command.fork_node_id.clone(),
                fork_seq,
                name: None,
                description: description.clone(),
                model_proposal: model_proposal.clone(),
            }
            .digest()
            .expect("review digest");
            command.metafork = Some(haider_store::SessionMetaforkCommit {
                description,
                model_proposal,
                accepted_proposal_digest: digest,
            });
        }
        store.fork_session(command).await.expect("inherited fork");
    }
    let inherited = hub
        .session_metadata(&child)
        .await
        .expect("child metadata")
        .expect("typed");
    if route_json(&inherited) != expected_route {
        failures.push(format!("{kind}: creation lost inherited route provenance"));
    }
    child
}

pub(super) async fn append_legacy_same_epoch_observation(
    store: &haider_core::SqliteStoreHandle,
    hub: &SessionHub,
    session: &SessionId,
    metadata: &SessionMetadataV1,
) {
    let lease = hub
        .acquire_worker_lease(session.clone())
        .await
        .expect("lease");
    let payload = haider_protocol::session::ModelSelected {
        provider: metadata.provider.clone(),
        model: metadata.model.clone(),
        selection_epoch: metadata.selection_epoch,
        route_only: true,
        cleared_account_pin: None,
        cleared_provider_endpoint: None,
        output_budget_clamp: None,
    }
    .to_payload_value()
    .expect("legacy observation");
    let mut facts = [supervisor_raw_envelope(
        &lease,
        &DeviceId::new("legacy-observation"),
        None,
        None,
        EventId::new("legacy-same-epoch-observation"),
        payload,
    )];
    haider_core::StoreHandle::append(store, &mut facts)
        .await
        .expect("legacy same-epoch publication");
}

pub(super) async fn append_old_recovery_checkpoint(
    hub: &SessionHub,
    session: &SessionId,
    run: &RunId,
    original: &SessionMetadataV1,
    borrowed_seen_state: bool,
) {
    let mut current = hub
        .session_metadata(session)
        .await
        .expect("metadata")
        .expect("typed");
    // Exactly the old restore fallback: retain mutable provenance on provider
    // equality, otherwise release selectors without restoring the old epoch.
    if current.provider != original.provider {
        current.provider_base_url = None;
        current.provider_rebind_id = None;
        current.account_alias = None;
        current.resolved_route_alias = None;
        current.resolved_route_seen = false;
    }
    current.provider.clone_from(&original.provider);
    current.model.clone_from(&original.model);
    current.max_tokens = original.max_tokens;
    let pin: haider_protocol::session::RunOutputPairPinnedV1 = serde_json::from_value(serde_json::json!({
        "provider": original.provider, "model": original.model, "max_tokens": original.max_tokens,
        "source": haider_protocol::output_budget::SessionOutputBudgetSourceV1::Derived,
        "provider_route": haider_protocol::session::RunOutputProviderRouteV1::capture(&current),
    })).expect("old recovery checkpoint");
    let lease = hub
        .acquire_worker_lease(session.clone())
        .await
        .expect("lease");
    append_payloads(
        &lease,
        &DeviceId::new("legacy-checkpoint"),
        run,
        None,
        &EventIdGenerator::new("legacy-checkpoint"),
        vec![pin.to_event(run).expect("pin")],
    )
    .await
    .expect("checkpoint");
    // Model old recovery's setup before its provider request: the borrowed
    // observation can turn an unchanged owned alias into a false rotation.
    let alias = if borrowed_seen_state {
        original.resolved_route_alias.as_deref()
    } else {
        current.account_alias.as_deref()
    };
    let epoch = lease
        .commit_resolved_route(&current, alias)
        .await
        .expect("old session publication");
    if let Some(mut route) = haider_protocol::session::RunOutputProviderRouteV1::capture(&current) {
        route.resolved_alias = alias.map(str::to_owned);
        route.resolved_seen = true;
        route.selection_epoch = Some(epoch);
        append_payloads(
            &lease,
            &DeviceId::new("legacy-checkpoint"),
            run,
            None,
            &EventIdGenerator::new("legacy-checkpoint-resolution"),
            vec![EventPayload::Item(ItemEvent::Completed {
                item_id: haider_protocol::ids::ItemId::new("legacy-poisoned-resolution"),
                item: TurnItem::Extension {
                    kind: haider_protocol::session::RunOutputProviderRouteV1::RESOLVED_KIND.into(),
                    data: serde_json::to_value(route).expect("old resolved route"),
                },
            })],
        )
        .await
        .expect("old owned resolution");
    }
}

pub(super) async fn setup_opaque_selection(
    hub: &SessionHub,
    session: &SessionId,
    generation: u64,
    binding: &str,
) {
    hub.select_session_model(haider_store::SessionSelectModelCommand {
        command_id: "opaque-before".into(),
        request_digest: "opaque-before".into(),
        // A model-shaped opaque body still does not record the account setter.
        request_json: if binding.ends_with("_empty") {
            "{}".into()
        } else {
            r#"{"provider":"fake","model":"initial-model"}"#.into()
        },
        session_id: session.clone(),
        worker_generation: generation,
        provider: "fake".into(),
        model: "initial-model".into(),
        expected_pair: None,
        account_alias: Some("selected-account".into()),
        output_budget: None,
        event_id: EventId::new("opaque-before"),
        device_id: DeviceId::new("opaque-device"),
    })
    .await
    .expect("account setter");
    if binding.starts_with("opaque_later_witness") {
        select_rotation_test_model(
            hub,
            session,
            generation,
            "later-witness",
            "fake",
            "initial-model",
            None,
            Some(4096),
        )
        .await;
    } else if binding.starts_with("opaque_legacy_rebind") {
        commit_test_rebind(hub, session, generation, 1).await;
    }
}

/// Shape only an owned synthetic profile into the older writer's journal.
/// The real Store/Hub is closed before this fixture edit and reopened afterward.
pub(super) fn strip_legacy_witnesses(
    root: &std::path::Path,
    events: &[haider_protocol::envelope::RawEnvelope],
    binding: &str,
) {
    let connection =
        rusqlite::Connection::open(root.join("store.sqlite")).expect("private fixture database");
    let names = connection
        .prepare("SELECT name FROM sqlite_master WHERE type = 'trigger' AND tbl_name = 'events'")
        .expect("triggers")
        .query_map([], |row| row.get::<_, String>(0))
        .expect("trigger names")
        .collect::<Result<Vec<_>, _>>()
        .expect("triggers");
    for name in names {
        connection
            .execute_batch(&format!("DROP TRIGGER \"{}\"", name.replace('"', "\"\"")))
            .expect("fixture trigger");
    }
    connection.execute("UPDATE command_receipts SET response_json = json_remove(response_json, '$.route_snapshot') WHERE command_id = 'opaque-before'", [])
        .expect("old receipt");
    for event in events {
        let strip = event.event_id.as_str() == "opaque-before"
            || (binding.starts_with("opaque_legacy_rebind")
                || binding.starts_with("opaque_post_start_rebind"))
                && haider_protocol::session::SessionProviderRebound::from_payload_value(
                    &event.payload,
                )
                .is_some();
        if !strip {
            continue;
        }
        let mut old = event.clone();
        old.payload
            .as_object_mut()
            .expect("fact")
            .remove("route_snapshot");
        connection
            .execute(
                "UPDATE events SET envelope_json = ?1 WHERE session_id = ?2 AND seq = ?3",
                rusqlite::params![
                    serde_json::to_string(&old).expect("old event"),
                    old.session_id.as_str(),
                    i64::try_from(old.seq).expect("seq")
                ],
            )
            .expect("old event shape");
    }
}

/// The resolved default account is an observation, never an explicit selector.
pub(super) struct BoundaryRecordingFactory {
    pub inner: Arc<RecordingRebindFactory>,
    pub default_alias: Option<String>,
}

#[async_trait::async_trait]
impl ProviderFactory for BoundaryRecordingFactory {
    async fn resolve_for_turn(
        &self,
        metadata: &SessionMetadataV1,
    ) -> Result<ResolvedTurnProvider, HaiderError> {
        let mut resolved = self.inner.resolve_for_turn(metadata).await?;
        if let Some(alias) = &self.default_alias {
            resolved.account_alias = Some(alias.clone());
        }
        Ok(resolved)
    }
}
