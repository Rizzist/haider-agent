#![allow(clippy::expect_used)]

use super::*;

fn setup_reduction_envelope(
    seq: u64,
    run_id: Option<RunId>,
    branch_id: Option<BranchId>,
    agent_id: Option<AgentId>,
    committed_at_ms: u64,
    payload: serde_json::Value,
) -> RawEnvelope {
    RawEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(format!("setup-reduction-{seq}")),
        seq,
        session_id: SessionId::new("setup-reduction-session"),
        branch_id,
        run_id,
        agent_id,
        device_id: DeviceId::new("setup-reduction-device"),
        authority_epoch: 0,
        worker_generation: 1,
        causation_id: None,
        correlation_id: None,
        committed_at_ms,
        render: RenderTargets {
            ui: false,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload,
    }
}

fn setup_reduction_usage(scope: UsageScope, history_message_count: u64) -> Usage {
    Usage {
        input: 10,
        output: 2,
        reasoning: 0,
        cached: 0,
        source: haider_protocol::provider::UsageSource::ProviderReported,
        account: None,
        accounts: Vec::new(),
        normalized: None,
        scope: Some(scope),
        cache_cost: None,
        request: Some(RequestUsage {
            ordinal: 1,
            input: 10,
            output: 2,
            reasoning: None,
            cached: None,
            source: haider_protocol::provider::UsageSource::ProviderReported,
            account: None,
            normalized: None,
            cache_cost: None,
            cache: Some(haider_protocol::provider::CacheRequestDiagnosticV1 {
                history_message_count,
                stable_prefix_tokens: 8,
                breakpoint_hashes: haider_protocol::provider::CacheBreakpointHashesV1::default(),
                cache_domain_hash: Some(format!("domain-{history_message_count}")),
                cache_domain_changed: Some(false),
                previous_breakpoint: None,
                prefix_match: haider_protocol::provider::CachePrefixMatchV1::Same,
                control: CacheControlObservationV1::NotRequired,
                cacheable_minimum_tokens: None,
                reuse_gap_ms: None,
                rewarm: None,
                classification: None,
            }),
        }),
    }
}

#[test]
fn fused_turn_setup_reduction_preserves_every_standalone_head() {
    let current_run = RunId::new("setup-current");
    let current_branch = BranchId::new("setup-main");
    let selector = TurnSetupReductionSelector {
        run_id: current_run.clone(),
        branch_id: Some(current_branch.clone()),
        agent_id: None,
        provider: "provider-a".into(),
        model: "model-a".into(),
        account_scope: None,
        auth_scope: "api_key".into(),
    };
    let mut reduction = TurnSetupReduction::new(selector);
    let instruction_fact = ProjectInstructionsLoaded {
        files: vec![
            haider_protocol::project_instructions::ProjectInstructionFileFact {
                path: "AGENTS.md".into(),
                digest: "instruction-digest".into(),
                bytes: 17,
                truncated: false,
            },
        ],
    };
    reduction
        .observe_envelope(setup_reduction_envelope(
            1,
            Some(current_run.clone()),
            Some(current_branch.clone()),
            None,
            1,
            instruction_fact
                .to_payload_value()
                .expect("instruction payload"),
        ))
        .expect("reduce instruction fact");

    let effect = EffectId::new("setup-effect");
    let menu_id = MenuId::new("setup-menu");
    let tool_payloads = [
        EventPayload::Effect(EffectPhase::Intent(EffectIntent {
            effect: effect.clone(),
            class: EffectClass::FsWrite,
            summary: "write setup".into(),
            args_digest: "args-digest".into(),
            workspace_revision: None,
        })),
        EventPayload::Effect(EffectPhase::Authorized {
            effect: effect.clone(),
            verdict: AuthorizationVerdict::Ask {
                menu: menu_id.clone(),
            },
        }),
        EventPayload::MenuOpened(Menu {
            id: menu_id.clone(),
            kind: MenuKind::Permission {
                effect_summary: "write setup".into(),
            },
            title: "Allow write".into(),
            body: Vec::new(),
            options: vec![MenuOption {
                key: "always".into(),
                label: "Always".into(),
                detail: None,
                decision: Some(DecisionKind::AllowAlways),
            }],
            blocking: true,
            scope: MenuScope::Session,
            origin: "setup-test".into(),
            ttl_ms: None,
            timeout_option: None,
        }),
        EventPayload::MenuAnswered(MenuAnswer {
            menu: menu_id.clone(),
            option_key: Some("always".into()),
            option_index: 0,
            value: None,
            via: AnswerVia::Rpc,
        }),
        EventPayload::Effect(EffectPhase::Outcome {
            effect: effect.clone(),
            outcome: EffectOutcome::Ok,
            freshness: Some(FileFreshness {
                path: "src/lib.rs".into(),
                digest: "fresh-digest".into(),
            }),
            workspace_mutation: None,
        }),
        EventPayload::UserMessage {
            text: "/mobile-use".into(),
            attachments: Vec::new(),
            mode: DeliveryMode::Steer,
        },
    ];
    for (offset, payload) in tool_payloads.into_iter().enumerate() {
        let seq = u64::try_from(offset).expect("small offset") + 2;
        reduction
            .observe_envelope(setup_reduction_envelope(
                seq,
                Some(current_run.clone()),
                Some(current_branch.clone()),
                None,
                seq,
                serde_json::to_value(payload).expect("tool payload"),
            ))
            .expect("reduce tool payload");
    }

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("wall clock")
        .as_millis();
    let now_ms = u64::try_from(now_ms).expect("test clock fits u64");
    let matching_scope = UsageScope {
        provider: "provider-a".into(),
        model: "model-a".into(),
        account_scope: None,
        auth_scope: "api_key".into(),
        cache_epoch: "matching-epoch".into(),
        stable_prefix_tokens: 55,
        ..UsageScope::default()
    };
    reduction
        .observe_envelope(setup_reduction_envelope(
            8,
            Some(RunId::new("setup-previous")),
            Some(current_branch.clone()),
            None,
            now_ms.saturating_sub(1_000),
            serde_json::to_value(EventPayload::Usage(setup_reduction_usage(
                matching_scope.clone(),
                41,
            )))
            .expect("matching usage"),
        ))
        .expect("reduce matching usage");
    let mut delegated_matching_scope = matching_scope.clone();
    delegated_matching_scope.request_kind = UsageRequestKind::DelegatedAgent;
    delegated_matching_scope.agent = Some(AgentId::new("setup-agent"));
    reduction
        .observe_envelope(setup_reduction_envelope(
            9,
            Some(RunId::new("setup-delegated")),
            Some(current_branch.clone()),
            Some(AgentId::new("setup-agent")),
            now_ms.saturating_sub(250),
            serde_json::to_value(EventPayload::Usage(setup_reduction_usage(
                delegated_matching_scope,
                99,
            )))
            .expect("delegated usage"),
        ))
        .expect("reduce delegated usage");
    let mut provider_mismatch = matching_scope.clone();
    provider_mismatch.provider = "provider-b".into();
    provider_mismatch.cache_epoch = "provider-mismatch".into();
    let mut model_mismatch = matching_scope.clone();
    model_mismatch.model = "model-b".into();
    model_mismatch.cache_epoch = "model-mismatch".into();
    let mut account_mismatch = matching_scope.clone();
    account_mismatch.account_scope =
        Some(haider_protocol::ids::CredentialAlias::new("other-account"));
    account_mismatch.cache_epoch = "account-mismatch".into();
    let mut auth_mismatch = matching_scope.clone();
    auth_mismatch.auth_scope = "oauth_subscription".into();
    auth_mismatch.cache_epoch = "auth-mismatch".into();
    for (offset, (scope, history_message_count)) in [
        (provider_mismatch, 91),
        (model_mismatch, 92),
        (account_mismatch, 93),
        (auth_mismatch, 94),
    ]
    .into_iter()
    .enumerate()
    {
        let seq = u64::try_from(offset).expect("small offset") + 10;
        reduction
            .observe_envelope(setup_reduction_envelope(
                seq,
                Some(RunId::new(format!("setup-mismatch-{offset}"))),
                Some(current_branch.clone()),
                None,
                now_ms,
                serde_json::to_value(EventPayload::Usage(setup_reduction_usage(
                    scope,
                    history_message_count,
                )))
                .expect("one-field mismatch usage"),
            ))
            .expect("reduce one-field mismatch usage");
    }
    let mut current_run_scope = matching_scope;
    current_run_scope.agent = Some(AgentId::new("setup-current-agent"));
    reduction
        .observe_envelope(setup_reduction_envelope(
            14,
            Some(current_run.clone()),
            Some(current_branch.clone()),
            Some(AgentId::new("setup-current-agent")),
            now_ms,
            serde_json::to_value(EventPayload::Usage(setup_reduction_usage(
                current_run_scope,
                100,
            )))
            .expect("current-run usage"),
        ))
        .expect("reduce current-run usage");
    let block = haider_protocol::cache::ProviderViewBlockRefV1::for_bytes(b"setup-view");
    let provider_view = ProviderViewLedgerV1 {
        provider: "provider-a".into(),
        model: "model-a".into(),
        dialect: "setup-dialect".into(),
        serialization_version: "setup-v1".into(),
        header_epoch: "setup-header".into(),
        cache_epoch: "matching-epoch".into(),
        compaction_epoch: "root".into(),
        reasoning_retention: "setup-retention".into(),
        account_scope: None,
        stable_history_end: 1,
        current_user_start: 1,
        latest_compaction_summary_end: None,
        trim_sentinel: "root".into(),
        boundaries: Vec::new(),
        system_block: block.clone(),
        tool_schema_block: block,
        history_blocks: Vec::new(),
        storage: None,
    };
    let provider_view_item = ProviderViewAttemptV1 {
        ordinal: 1,
        view: provider_view.clone(),
    }
    .extension_item()
    .expect("provider-view item");
    reduction
        .observe_envelope(setup_reduction_envelope(
            15,
            Some(RunId::new("setup-provider-view")),
            Some(current_branch.clone()),
            None,
            now_ms,
            serde_json::to_value(EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new("setup-provider-view-item"),
                item: provider_view_item,
            }))
            .expect("provider-view payload"),
        ))
        .expect("reduce provider view");
    let transition = CacheEpochTransitionV1 {
        reason: CacheEpochTransitionReason::InstructionsChanged,
        planned: false,
        changed_fields: vec!["instructions".into()],
        invalidated_stable_tokens: 55,
        rewarm_cost_usd: None,
        rewarm_base_input_equivalent_tokens: None,
        transition_id: "setup-transition".into(),
        from_cache_epoch: Some("matching-epoch".into()),
        to_cache_epoch: None,
    };
    let transition_item = transition.extension_item().expect("transition item");
    reduction
        .observe_envelope(setup_reduction_envelope(
            16,
            Some(current_run),
            Some(current_branch),
            None,
            now_ms,
            serde_json::to_value(EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new("setup-transition-item"),
                item: transition_item,
            }))
            .expect("transition payload"),
        ))
        .expect("reduce transition");

    assert_eq!(
        reduction.same_run_instruction_fact,
        Some(instruction_fact.clone())
    );
    assert_eq!(reduction.latest_instruction_fact, Some(instruction_fact));
    let durable = reduction.durable_tool_state();
    assert!(durable.mobile_use_active);
    assert_eq!(
        durable.bindings.get(&menu_id),
        Some(&(EffectClass::FsWrite, "args-digest".into()))
    );
    assert_eq!(
        durable.freshness.get("src/lib.rs"),
        Some(&FileFreshness {
            path: "src/lib.rs".into(),
            digest: "fresh-digest".into(),
        })
    );
    assert_eq!(
        durable.grants,
        vec![SessionGrant::for_effect(
            EffectClass::FsWrite,
            "args-digest"
        )]
    );
    assert_eq!(
        reduction
            .latest_main_usage_scope
            .as_ref()
            .map(|scope| scope.cache_epoch.as_str()),
        Some("auth-mismatch")
    );
    let (previous, previous_provider_view, rewarm) = reduction.prior_cache_request_context();
    assert_eq!(
        previous,
        Some(PreviousCacheRequest {
            history_message_count: 41,
            breakpoint_hashes: haider_protocol::provider::CacheBreakpointHashesV1::default(),
            cache_domain_hash: Some("domain-41".into()),
        })
    );
    assert_eq!(previous_provider_view, Some(provider_view));
    assert_eq!(rewarm, Some(CacheRewarmReasonV1::ConfigurationChange));
    assert!(reduction.cache_transition_was_emitted(&transition));
    assert!(
        reduction
            .prior_cache_domain_gap_ms()
            .is_some_and(|gap| { (250..5_000).contains(&gap) })
    );

    let appended_transition = CacheEpochTransitionV1 {
        reason: CacheEpochTransitionReason::Compaction,
        planned: true,
        transition_id: "setup-appended-transition".into(),
        ..transition.clone()
    };
    reduction.record_cache_transition(&appended_transition);
    assert!(reduction.cache_transition_was_emitted(&appended_transition));
    assert_eq!(
        reduction.prior_cache_request_context().2,
        Some(CacheRewarmReasonV1::PlannedCompaction)
    );
    let appended_instructions = ProjectInstructionsLoaded::default();
    reduction.record_instruction_fact(appended_instructions.clone());
    assert_eq!(
        reduction.same_run_instruction_fact,
        Some(appended_instructions.clone())
    );
    assert_eq!(
        reduction.latest_instruction_fact,
        Some(appended_instructions)
    );
}

#[test]
fn fused_turn_setup_reduction_rejects_malformed_matching_provider_view() {
    let branch_id = BranchId::new("setup-main");
    let mut reduction = TurnSetupReduction::new(TurnSetupReductionSelector {
        run_id: RunId::new("setup-current"),
        branch_id: Some(branch_id.clone()),
        agent_id: None,
        provider: "provider-a".into(),
        model: "model-a".into(),
        account_scope: None,
        auth_scope: "api_key".into(),
    });
    let malformed = TurnItem::Extension {
        kind: haider_protocol::cache::PROVIDER_VIEW_ATTEMPT_EXTENSION_KIND.into(),
        data: serde_json::json!({"ordinal": 1}),
    };
    let error = reduction
        .observe_envelope(setup_reduction_envelope(
            1,
            Some(RunId::new("setup-previous")),
            Some(branch_id),
            None,
            1,
            serde_json::to_value(EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new("malformed-provider-view"),
                item: malformed,
            }))
            .expect("malformed provider-view payload"),
        ))
        .expect_err("matching malformed provider view must fail closed");
    assert_eq!(error.code, ErrorCode::Internal);
    assert!(error.message.contains("provider-view ledger is malformed"));
}
