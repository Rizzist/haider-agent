#![allow(clippy::expect_used)]
//! Additive provider model-detail wire compatibility.

use haider_protocol::credential::AuthMethod;
use haider_rpc::{
    ModelDetailWire, ProviderApiFamilyWire, ProviderAvailabilityWire, ProviderSummaryWire,
};

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
