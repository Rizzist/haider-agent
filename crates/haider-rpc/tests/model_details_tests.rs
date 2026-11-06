#![allow(clippy::expect_used)]
//! Additive provider model-detail wire compatibility.

use haider_protocol::credential::AuthMethod;
use haider_rpc::{
    ModelDetailWire, ProviderApiFamilyWire, ProviderAvailabilityWire, ProviderSummaryWire,
};

#[test]
fn aging_projection_keeps_configured_ids_selectable_at_the_exact_boundary() {
    for models in [vec!["other"], vec![]] {
        for catalog in ["public", "authenticated", "custom", "adapter", "offline"] {
            let mut summary: ProviderSummaryWire = serde_json::from_value(serde_json::json!({
                "provider": "work", "api_family": "openai_chat_completions",
                "endpoint": "http://127.0.0.1:19373/v1", "enabled": true,
                "models": models, "configured_models": ["deployment"],
                "catalog": catalog, "inventory_authority": "authoritative",
                "inventory": {"state":"fetched", "fetched_at_ms":100},
                "availability": if models.is_empty() { "unavailable" } else { "available" }
            }))
            .expect("fresh negative listing");
            summary.project_inventory_at_time(100 + haider_rpc::MODEL_INVENTORY_TTL_MS - 1, &[]);
            assert!(!summary.model_is_selectable("deployment"));
            summary.project_inventory_at_time(100 + haider_rpc::MODEL_INVENTORY_TTL_MS, &[]);
            assert!(
                summary.model_is_selectable("deployment"),
                "{catalog}: {summary:?}"
            );
            assert!(summary.models.contains(&"deployment".into()));
            assert!(
                summary.model_details.is_empty(),
                "no fetched metadata invented"
            );
            assert!(
                summary
                    .selection_reason(None)
                    .expect("stale reason")
                    .contains("refresh")
            );

            let mut credential_missing = summary.clone();
            credential_missing.inventory =
                haider_rpc::ModelInventoryWire::Fetched { fetched_at_ms: 100 };
            credential_missing.auth_methods = vec![AuthMethod::ApiKey];
            credential_missing.availability = ProviderAvailabilityWire::Unavailable;
            credential_missing
                .project_inventory_at_time(100 + haider_rpc::MODEL_INVENTORY_TTL_MS, &[]);
            assert!(!credential_missing.model_is_selectable("deployment"));
            let mut disabled = summary.clone();
            disabled.enabled = false;
            disabled.project_inventory_at_time(u64::MAX, &[]);
            assert!(!disabled.model_is_selectable("deployment"));
            let mut rejected = summary;
            rejected.inventory = haider_rpc::ModelInventoryWire::Unavailable {
                reason: "authentication failed".into(),
            };
            rejected.availability = ProviderAvailabilityWire::Unavailable;
            rejected.project_inventory_at_time(u64::MAX, &[]);
            assert!(!rejected.model_is_selectable("deployment"));
        }
    }
    // An advisory custom provider need not declare picker IDs. Its empty
    // successful list still becomes uncertainty at the same TTL boundary,
    // permitting explicit input without inventing rows or a default.
    for catalog in ["custom", "public", "authenticated", "adapter", "offline"] {
        for authority in ["advisory", "authoritative"] {
            for ready in [false, true] {
                let mut summary: ProviderSummaryWire = serde_json::from_value(serde_json::json!({
                    "provider":"work", "api_family":"openai_chat_completions",
                    "endpoint":"http://127.0.0.1:19373/v1", "enabled":true,
                    "auth_methods":["api_key"], "models":[], "configured_models":[],
                    "catalog":catalog, "inventory_authority":authority,
                    "inventory":{"state":"fetched", "fetched_at_ms":100},
                    "availability":"unavailable"
                }))
                .expect("fresh empty advisory listing");
                let mut disabled = summary.clone();
                disabled.enabled = false;
                let mut rejected = summary.clone();
                rejected.inventory = haider_rpc::ModelInventoryWire::Unavailable {
                    reason: "authentication failed".into(),
                };
                summary.project_inventory_at_time_with_credential_ready(
                    100 + haider_rpc::MODEL_INVENTORY_TTL_MS - 1,
                    ready,
                );
                assert!(!summary.model_is_selectable("explicit-unlisted"));
                summary.project_inventory_at_time_with_credential_ready(
                    100 + haider_rpc::MODEL_INVENTORY_TTL_MS,
                    ready,
                );
                assert_eq!(
                    summary.model_is_selectable("explicit-unlisted"),
                    authority == "advisory" && ready,
                    "{catalog}/{authority}, credential_ready={ready}: {summary:?}"
                );
                assert!(summary.models.is_empty());
                assert!(summary.model_details.is_empty());
                assert!(summary.default_model.is_none());
                for negative in [&mut disabled, &mut rejected] {
                    negative.project_inventory_at_time_with_credential_ready(u64::MAX, true);
                    assert!(!negative.model_is_selectable("explicit-unlisted"));
                }
            }
        }
    }
}

#[test]
fn aging_projection_preserves_account_health_evidence() {
    use haider_protocol::credential::{
        CredentialAttentionReason, CredentialDescriptor, CredentialStatus,
    };
    for method in [AuthMethod::ApiKey, AuthMethod::OAuth] {
        for (status, active, provider, ready) in [
            (CredentialStatus::Ok, true, "work", true),
            (
                CredentialStatus::Limited { until_ms: u64::MAX },
                true,
                "work",
                true,
            ),
            (CredentialStatus::Expired, true, "work", false),
            (CredentialStatus::Revoked, true, "work", false),
            (
                CredentialStatus::NeedsAttention {
                    reason: CredentialAttentionReason::SourceUnreadable,
                },
                true,
                "work",
                false,
            ),
            (CredentialStatus::Ok, false, "work", false),
            (CredentialStatus::Ok, true, "other", false),
        ] {
            let descriptor: CredentialDescriptor = serde_json::from_value(serde_json::json!({
                "alias":"synthetic", "provider":provider, "auth_method":method,
                "identity":"synthetic", "status":status, "active":active
            }))
            .expect("metadata-only account");
            let mut summary: ProviderSummaryWire = serde_json::from_value(serde_json::json!({
                "provider":"work", "api_family":"openai_chat_completions",
                "endpoint":"http://127.0.0.1:19373/v1", "enabled":true,
                "auth_methods":[method], "configured_models":["deployment"],
                "inventory":{"state":"fetched", "fetched_at_ms":100},
                "availability":"unavailable"
            }))
            .expect("fresh empty listing");
            summary
                .project_inventory_at_time(100 + haider_rpc::MODEL_INVENTORY_TTL_MS, &[descriptor]);
            assert_eq!(
                summary.model_is_selectable("deployment"),
                ready,
                "{method:?}: {status:?}, active={active}, provider={provider}"
            );
        }
    }
}

/// MUTATION CHECK: remove `serde(default)` from
/// `ProviderSummaryWire::model_details`. Expected runtime failure: decoding
/// this old-daemon summary returns a missing-field error.
#[test]
fn provider_summary_without_model_details_decodes_an_empty_vector() {
    let summary: ProviderSummaryWire = serde_json::from_value(serde_json::json!({
        "provider": "openai",
        "api_family": "openai_responses",
        "models": ["frontier-a"],
        "auth_methods": ["api_key"],
        "availability": "available",
        "enabled": true
    }))
    .expect("old-daemon provider summary decodes");

    assert!(summary.model_details.is_empty());
}

/// MUTATION CHECK: omit model details or their token limits during
/// serialization. Expected runtime failure: the decoded summary differs from
/// the provider-declared names and windows below.
#[test]
fn provider_summary_model_details_round_trip_token_limits() {
    let summary = ProviderSummaryWire {
        catalog_scope: None,
        configured_models: Vec::new(),
        static_models: Vec::new(),
        provider: "openai".to_owned(),
        api_family: ProviderApiFamilyWire::OpenAiResponses,
        endpoint: Some("https://api.openai.com/v1/responses".to_owned()),
        response_open_timeout_ms: None,
        chunk_idle_timeout_ms: None,
        semantic_progress_timeout_ms: None,
        models: vec!["frontier-a".to_owned(), "frontier-b".to_owned()],
        model_details: vec![
            ModelDetailWire {
                source: None,
                name: "frontier-a".to_owned(),
                display_name: Some("Frontier A".to_owned()),
                context_window: Some(200_000),
                max_output_tokens: Some(128_000),
                max_output_tokens_source: None,
                supported_efforts: Vec::new(),
                default_effort: None,
                supported_speeds: Vec::new(),
                supports_thinking_type: None,
                supports_vision: None,
            },
            ModelDetailWire {
                source: None,
                name: "frontier-b".to_owned(),
                display_name: None,
                context_window: None,
                max_output_tokens: None,
                max_output_tokens_source: None,
                supported_efforts: Vec::new(),
                default_effort: None,
                supported_speeds: Vec::new(),
                supports_thinking_type: None,
                supports_vision: None,
            },
        ],
        catalog: haider_rpc::ProviderCatalogKindWire::Unknown,
        inventory: haider_rpc::ModelInventoryWire::Static,
        inventory_authority: haider_rpc::ModelInventoryAuthorityWire::Authoritative,
        auth_methods: vec![AuthMethod::ApiKey],
        availability: ProviderAvailabilityWire::Available,
        availability_reason: None,
        default_model: Some("frontier-a".to_owned()),
        enabled: true,
        trust: haider_rpc::ProviderTrustWire::Full,
    };

    let encoded = serde_json::to_value(&summary).expect("provider summary encodes");
    assert_eq!(encoded["model_details"][0]["display_name"], "Frontier A");
    assert!(
        encoded["model_details"][1].get("display_name").is_none(),
        "absent labels stay byte-compatible with older model details"
    );
    let decoded: ProviderSummaryWire =
        serde_json::from_value(encoded).expect("provider summary decodes");

    assert_eq!(decoded, summary);
    assert_eq!(decoded.model_details[0].name, "frontier-a");
    assert_eq!(
        decoded.model_details[0].display_name.as_deref(),
        Some("Frontier A")
    );
    assert_eq!(decoded.model_details[0].context_window, Some(200_000));
    assert_eq!(decoded.model_details[1].name, "frontier-b");
    assert_eq!(decoded.model_details[1].context_window, None);
}

/// 973-output-cap-fixes B2: the output-maximum provenance is additive. An
/// older detail without it decodes as absent and re-encodes byte-identically;
/// each known spelling round-trips; an unknown future spelling decodes as
/// `Unknown` instead of failing the whole provider summary.
/// MUTATION CHECK: drop `serde(other)` from `ModelOutputLimitSourceWire`, or
/// `skip_serializing_if` from the field. Expected runtime failure: the future
/// spelling fails to decode, or the old detail gains a `null` key.
#[test]
fn model_detail_output_limit_source_is_additive_and_tolerant() {
    use haider_rpc::ModelOutputLimitSourceWire;
    let old = serde_json::json!({"name": "m", "max_output_tokens": 8192});
    let decoded: ModelDetailWire = serde_json::from_value(old.clone()).expect("old detail decodes");
    assert_eq!(decoded.max_output_tokens_source, None);
    assert_eq!(serde_json::to_value(&decoded).expect("encode"), old);
    for (spelling, expected) in [
        ("declared", ModelOutputLimitSourceWire::Declared),
        ("static", ModelOutputLimitSourceWire::Static),
        ("fallback", ModelOutputLimitSourceWire::Fallback),
        (
            "published_by_a_future_daemon",
            ModelOutputLimitSourceWire::Unknown,
        ),
    ] {
        let decoded: ModelDetailWire = serde_json::from_value(serde_json::json!({
            "name": "m",
            "max_output_tokens": 8192,
            "max_output_tokens_source": spelling,
        }))
        .expect("detail with a provenance decodes");
        assert_eq!(
            decoded.max_output_tokens_source,
            Some(expected),
            "{spelling}"
        );
    }
    let declared = ModelDetailWire {
        max_output_tokens_source: Some(ModelOutputLimitSourceWire::Declared),
        ..serde_json::from_value(old).expect("old detail decodes")
    };
    assert_eq!(
        serde_json::to_value(&declared).expect("encode")["max_output_tokens_source"],
        "declared"
    );
}

#[test]
fn model_output_limit_error_data_round_trips_additively() {
    let data = haider_rpc::ErrorData::ModelOutputLimit {
        provider: "anthropic-oauth".into(),
        model: "claude-fable-5-1".into(),
        requested: 200_000,
        max_output_tokens: 128_000,
        context_window: Some(1_000_000),
    };
    let json = serde_json::to_value(&data).expect("serialize typed limit error");
    assert_eq!(json["kind"], "model_output_limit");
    assert_eq!(json["max_output_tokens"], 128_000);
    assert!(matches!(
        serde_json::from_value(json).expect("decode typed limit error"),
        haider_rpc::ErrorData::ModelOutputLimit {
            requested: 200_000,
            max_output_tokens: 128_000,
            context_window: Some(1_000_000),
            ..
        }
    ));
}

/// MUTATION CHECK: require the additive response-open budget, omit a present
/// override, or serialize the absent value. Expected runtime failure: the
/// N-1 decode or exact presence/absence assertions below change.
#[test]
fn provider_summary_transport_timeouts_preserve_typed_absence() {
    let old: ProviderSummaryWire = serde_json::from_value(serde_json::json!({
        "provider": "openai",
        "api_family": "openai_responses",
        "models": [],
        "auth_methods": [],
        "availability": "available",
        "enabled": true
    }))
    .expect("pre-timeout provider summary decodes");
    assert_eq!(old.response_open_timeout_ms, None);
    assert_eq!(old.chunk_idle_timeout_ms, None);
    assert_eq!(old.semantic_progress_timeout_ms, None);
    assert!(
        serde_json::to_value(&old)
            .expect("old summary encodes")
            .get("response_open_timeout_ms")
            .is_none()
    );

    let mut current = old;
    current.response_open_timeout_ms = Some(75_000);
    current.chunk_idle_timeout_ms = Some(95_000);
    current.semantic_progress_timeout_ms = Some(305_000);
    let value = serde_json::to_value(&current).expect("current summary encodes");
    assert_eq!(value["response_open_timeout_ms"], 75_000);
    assert_eq!(value["chunk_idle_timeout_ms"], 95_000);
    assert_eq!(value["semantic_progress_timeout_ms"], 305_000);
    assert_eq!(
        serde_json::from_value::<ProviderSummaryWire>(value)
            .expect("current summary decodes")
            .response_open_timeout_ms,
        Some(75_000)
    );
}

/// MUTATION CHECK: dropping or renaming the additive fetch timestamp loses
/// cache age; defaulting inventory authority to advisory grants old summaries
/// a passthrough exception; or classifying/appending the missing id fabricates
/// availability. Expected RUNTIME failure: the exact assertions below.
#[test]
fn provider_summary_inventory_fetch_time_and_authority_are_additive_and_pinned() {
    let mut summary: ProviderSummaryWire = serde_json::from_value(serde_json::json!({
        "provider": "custom-router",
        "api_family": "openai_chat_completions",
        "endpoint": "https://router.example/v1",
        "models": ["model-a"],
        "auth_methods": ["api_key"],
        "availability": "available",
        "enabled": true
    }))
    .expect("legacy provider summary");
    assert_eq!(summary.inventory.fetched_at_ms(), None);
    assert_eq!(
        summary.inventory_authority,
        haider_rpc::ModelInventoryAuthorityWire::Unknown
    );
    summary.inventory = haider_rpc::ModelInventoryWire::Fetched {
        fetched_at_ms: 1_753_500_000_000,
    };
    let encoded = serde_json::to_value(&summary).expect("encode fetch timestamp");
    assert_eq!(
        encoded["inventory"]["fetched_at_ms"],
        serde_json::json!(1_753_500_000_000_u64)
    );
    summary.inventory = haider_rpc::ModelInventoryWire::NeverFetched;
    let encoded = serde_json::to_value(&summary).expect("encode absent fetch timestamp");
    assert!(encoded.get("inventory_fetched_at_ms").is_none());
    assert!(encoded.get("inventory_authority").is_none());

    assert_eq!(
        summary.model_inventory_status("passthrough-model"),
        haider_rpc::ModelInventoryStatusWire::Unknown
    );
    summary.inventory = haider_rpc::ModelInventoryWire::Fetched {
        fetched_at_ms: 1_753_500_000_000,
    };
    summary.inventory_authority = haider_rpc::ModelInventoryAuthorityWire::Advisory;
    assert_eq!(
        summary.model_inventory_status("passthrough-model"),
        haider_rpc::ModelInventoryStatusWire::Unlisted
    );
    assert_eq!(summary.models, ["model-a"]);
    let encoded = serde_json::to_value(&summary).expect("encode advisory authority");
    assert_eq!(encoded["inventory_authority"], "advisory");
}

#[test]
fn provenance_refresh_is_a_state_check_even_for_an_empty_successful_catalog() {
    use haider_rpc::ModelInventoryWire;
    assert!(ModelInventoryWire::NeverFetched.needs_refresh());
    assert!(!ModelInventoryWire::Fetched { fetched_at_ms: 10 }.needs_refresh());
    assert!(!ModelInventoryWire::Static.needs_refresh());
    assert!(
        !ModelInventoryWire::Unavailable {
            reason: "credential missing".into()
        }
        .needs_refresh()
    );
    assert!(
        !ModelInventoryWire::Fetched { fetched_at_ms: 10 }
            .at_time(10 + haider_rpc::MODEL_INVENTORY_TTL_MS - 1)
            .needs_refresh()
    );
    assert!(
        ModelInventoryWire::Fetched { fetched_at_ms: 10 }
            .at_time(10 + haider_rpc::MODEL_INVENTORY_TTL_MS)
            .needs_refresh()
    );
    for state in [
        ModelInventoryWire::NeverFetched,
        ModelInventoryWire::Fetched { fetched_at_ms: 42 },
        ModelInventoryWire::Stale {
            fetched_at_ms: 42,
            reason: "offline".into(),
        },
        ModelInventoryWire::Unavailable {
            reason: "bad OAuth credential".into(),
        },
    ] {
        let encoded = serde_json::to_value(&state).expect("encode provenance");
        assert_eq!(
            serde_json::from_value::<ModelInventoryWire>(encoded).expect("decode provenance"),
            state
        );
    }
}

#[test]
fn static_model_source_preserves_membership_under_a_failed_fetch() {
    let mut summary: haider_rpc::ProviderSummaryWire = serde_json::from_value(serde_json::json!({
        "provider":"anthropic-oauth", "api_family":"anthropic_messages", "models":["claude-fable-5-1"],
        "model_details":[{"name":"claude-fable-5-1","source":"static","context_window":1000000}],
        "inventory":{"state":"unavailable","reason":"403"}, "availability":"available", "enabled":true
    })).expect("static provider summary");
    assert!(summary.has_static_models());
    assert_eq!(
        summary.model_inventory_status("claude-fable-5-1"),
        haider_rpc::ModelInventoryStatusWire::Listed
    );
    assert_eq!(
        summary.model_inventory_status("invented"),
        haider_rpc::ModelInventoryStatusWire::Unlisted
    );
    summary.model_details[0].source = None;
    assert!(!summary.has_static_models());
    assert_eq!(
        summary.model_inventory_status("claude-fable-5-1"),
        haider_rpc::ModelInventoryStatusWire::Unknown
    );
}

#[test]
fn future_model_source_decodes_without_losing_the_row() {
    let detail: ModelDetailWire = serde_json::from_value(serde_json::json!({
        "name": "future-model",
        "source": "future_catalog"
    }))
    .expect("future row source decodes");
    assert_eq!(detail.name, "future-model");
    assert_eq!(
        detail.source,
        Some(haider_rpc::ModelDetailSourceWire::Unknown)
    );
}

#[test]
fn catalog_selection_policy_is_shared_across_provider_kinds_and_uncertainty() {
    for catalog in ["custom", "public", "authenticated", "offline", "adapter"] {
        let mut summary: ProviderSummaryWire = serde_json::from_value(serde_json::json!({
            "provider":"synthetic", "api_family":"openai_chat_completions", "models":["listed"],
            "configured_models":["deployment"], "auth_methods":[], "availability":"available",
            "enabled":true, "catalog":catalog
        }))
        .expect("summary");
        summary.inventory = haider_rpc::ModelInventoryWire::Fetched {
            fetched_at_ms: u64::MAX,
        };
        assert!(summary.model_is_selectable("listed"));
        assert!(
            !summary.model_is_selectable("deployment"),
            "fresh negative membership: {catalog}"
        );
        summary.inventory = haider_rpc::ModelInventoryWire::Stale {
            fetched_at_ms: 1,
            reason: "catalog refresh due".into(),
        };
        assert!(
            summary.model_is_selectable("deployment"),
            "staleness is uncertainty: {catalog}"
        );
        summary.inventory = haider_rpc::ModelInventoryWire::Configured;
        assert!(
            summary.model_is_selectable("deployment"),
            "unsupported or transient configured fallback: {catalog}"
        );
        summary.availability = ProviderAvailabilityWire::Unavailable;
        assert!(!summary.model_is_selectable("deployment"));
        summary.availability = ProviderAvailabilityWire::Available;
        summary.enabled = false;
        assert!(!summary.model_is_selectable("listed"));
        assert_eq!(
            summary.selection_reason(Some("listed")).as_deref(),
            Some("provider is disabled")
        );
    }
}

#[test]
fn static_catalog_does_not_admit_unfetched_extra_deployments() {
    let mut summary: ProviderSummaryWire = serde_json::from_value(serde_json::json!({
        "provider":"synthetic", "api_family":"openai_chat_completions", "models":["known-static"],
        "model_details":[{"name":"known-static","source":"static"}],
        "static_models":["known-static"], "configured_models":["deployment"],
        "auth_methods":["api_key"], "availability":"available", "enabled":true,
        "inventory_authority":"authoritative"
    }))
    .expect("summary");
    // Assign the typed inventory so this regression also pins policy rather
    // than relying on an unrelated JSON representation.
    summary.inventory = haider_rpc::ModelInventoryWire::Static;
    assert!(summary.model_is_selectable("known-static"));
    assert!(!summary.model_is_selectable("deployment"));
    summary.inventory = haider_rpc::ModelInventoryWire::Unavailable {
        reason: "synthetic 503".into(),
    };
    summary.availability_reason = Some("synthetic 503 — static catalog available".into());
    assert!(summary.model_is_selectable("known-static"));
    assert!(!summary.model_is_selectable("deployment"));
    assert!(
        summary
            .selection_reason(Some("deployment"))
            .expect("reason")
            .contains("503")
    );
    summary.inventory = haider_rpc::ModelInventoryWire::Stale {
        fetched_at_ms: 1,
        reason: "refresh due".into(),
    };
    assert!(
        summary.model_is_selectable("deployment"),
        "prior successful listing is uncertainty"
    );
    summary.inventory = haider_rpc::ModelInventoryWire::Configured;
    summary.inventory_authority = haider_rpc::ModelInventoryAuthorityWire::Advisory;
    assert!(
        summary.model_is_selectable("deployment"),
        "unsupported discovery is selectable"
    );
    summary.availability = ProviderAvailabilityWire::Unavailable;
    assert!(
        !summary.model_is_selectable("known-static"),
        "credential rejection still wins"
    );
}

#[test]
fn selector_aliases_share_configured_negative_evidence_and_uncertainty() {
    for (declared, aliases) in [
        (
            "deployment",
            vec![
                "deployment",
                "DEPLOYMENT",
                "de.ploy_ment",
                " de-ploy ment ",
                "\tDEPLOY\r\nMENT",
                "de\u{a0}ploy\u{2003}ment",
            ],
        ),
        (
            "MÖDEL.東京 Δ",
            vec!["MÖDEL.東京 Δ", "mödel_東京-δ", "MÖDEL\u{2003}東京.Δ"],
        ),
        ("İ-model", vec!["İ-model", "i\u{307}.mo_DEL"]),
    ] {
        for authority in ["advisory", "authoritative"] {
            for catalog in ["public", "authenticated", "custom", "adapter", "offline"] {
                let mut summary: ProviderSummaryWire = serde_json::from_value(serde_json::json!({
                    "provider":"work", "api_family":"openai_chat_completions", "enabled":true,
                    "models":["healthy"], "configured_models":[declared], "catalog":catalog,
                    "inventory_authority":authority,
                    "inventory":{"state":"fetched", "fetched_at_ms":u64::MAX},
                    "availability":"available"
                }))
                .expect("fresh successful inventory");
                for alias in &aliases {
                    assert_eq!(summary.canonical_model_selector(alias), declared);
                    assert!(!summary.model_is_selectable(alias), "{catalog}: {alias}");
                    assert_eq!(
                        summary.selection_reason(Some(alias)).as_deref(),
                        Some("deployment is absent from the latest model list")
                    );
                }
                assert!(summary.model_is_selectable("HEALTHY"));
                summary.inventory = haider_rpc::ModelInventoryWire::Stale {
                    fetched_at_ms: 1,
                    reason: "server returned 503 for /models".into(),
                };
                for alias in &aliases {
                    assert!(summary.model_is_selectable(alias), "stale success: {alias}");
                    assert!(
                        summary
                            .selection_reason(Some(alias))
                            .expect("visible uncertainty")
                            .contains("503")
                    );
                }
                summary.inventory = haider_rpc::ModelInventoryWire::Configured;
                summary.availability_reason =
                    Some("configured (unverified): server returned 405 for /models".into());
                for alias in &aliases {
                    assert!(summary.model_is_selectable(alias));
                }
                summary.availability = ProviderAvailabilityWire::Unavailable;
                summary.availability_reason = Some("authentication failed".into());
                for alias in &aliases {
                    assert!(!summary.model_is_selectable(alias));
                    assert_eq!(
                        summary.selection_reason(Some(alias)).as_deref(),
                        Some("authentication failed")
                    );
                }
            }
        }
    }
    let mut summary: ProviderSummaryWire = serde_json::from_value(serde_json::json!({
        "provider":"work", "api_family":"openai_chat_completions", "enabled":true,
        "models":["de-ployment"], "configured_models":["deployment"], "catalog":"custom",
        "inventory_authority":"advisory", "inventory":{"state":"fetched", "fetched_at_ms":u64::MAX},
        "availability":"available"
    }))
    .expect("literal priority controls");
    assert!(!summary.model_is_selectable("deployment"));
    assert!(summary.model_is_selectable("de-ployment"));
    assert_eq!(
        summary.canonical_model_selector("DEPLOY.MENT"),
        "de-ployment"
    );
    assert!(summary.model_is_selectable("DEPLOY.MENT"));
    assert_eq!(
        summary.canonical_model_selector("future/model"),
        "future/model"
    );
    assert!(summary.model_is_selectable("future/model"));
    for literal in [
        "deploy%6dent",
        "de\u{2010}ployment",
        "de\u{200b}ployment",
        "deployment/",
    ] {
        assert_eq!(summary.canonical_model_selector(literal), literal);
        assert!(
            summary.model_is_selectable(literal),
            "non-alias advisory literal: {literal}"
        );
    }
    summary.models.push("de-ployment".into());
    assert_eq!(
        summary.canonical_model_selector("DEPLOY.MENT"),
        "de-ployment"
    );
    assert!(
        summary.model_is_selectable("DEPLOY.MENT"),
        "identical listed rows are one identity"
    );
    summary.models.push("deploy_ment".into());
    assert_eq!(
        summary.canonical_model_selector("DEPLOY.MENT"),
        "DEPLOY.MENT"
    );
    assert!(
        summary.model_is_selectable("DEPLOY.MENT"),
        "ambiguous advisory literal remains passthrough"
    );
    summary.models.clear();
    summary.configured_models.push("de-ployment".into());
    assert_eq!(
        summary.canonical_model_selector("DEPLOY.MENT"),
        "DEPLOY.MENT"
    );
    assert!(!summary.model_is_selectable("deployment"));
    summary.models = vec!["healthy".into()];
    summary.configured_models = vec!["deployment".into(), "deployment".into()];
    assert_eq!(
        summary.canonical_model_selector("DEPLOY.MENT"),
        "deployment"
    );
    assert!(
        !summary.model_is_selectable("DEPLOY.MENT"),
        "repeated declared IDs cannot bypass fresh absence"
    );
    summary.inventory = haider_rpc::ModelInventoryWire::Stale {
        fetched_at_ms: 1,
        reason: "refresh due".into(),
    };
    assert!(
        summary.model_is_selectable("DEPLOY.MENT"),
        "duplicate IDs do not block uncertainty"
    );
    summary.enabled = false;
    assert!(!summary.model_is_selectable("future/model"));
}
