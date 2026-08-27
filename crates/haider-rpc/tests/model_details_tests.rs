//! Additive provider model-detail wire compatibility.
#![allow(clippy::expect_used)]

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

/// MUTATION CHECK: omit model details or their context windows during
/// serialization. Expected runtime failure: the decoded summary differs from
/// the provider-declared names and windows below.
#[test]
fn provider_summary_model_details_round_trip_names_and_windows() {
    let summary = ProviderSummaryWire {
        provider: "openai".to_owned(),
        api_family: ProviderApiFamilyWire::OpenAiResponses,
        endpoint: Some("https://api.openai.com/v1/responses".to_owned()),
        response_open_timeout_ms: None,
        models: vec!["frontier-a".to_owned(), "frontier-b".to_owned()],
        model_details: vec![
            ModelDetailWire {
                name: "frontier-a".to_owned(),
                context_window: Some(200_000),
                supported_efforts: Vec::new(),
                default_effort: None,
                supported_speeds: Vec::new(),
                supports_thinking_type: None,
            },
            ModelDetailWire {
                name: "frontier-b".to_owned(),
                context_window: None,
                supported_efforts: Vec::new(),
                default_effort: None,
                supported_speeds: Vec::new(),
                supports_thinking_type: None,
            },
        ],
        auth_methods: vec![AuthMethod::ApiKey],
        availability: ProviderAvailabilityWire::Available,
        availability_reason: None,
        default_model: Some("frontier-a".to_owned()),
        enabled: true,
    };

    let encoded = serde_json::to_value(&summary).expect("provider summary encodes");
    let decoded: ProviderSummaryWire =
        serde_json::from_value(encoded).expect("provider summary decodes");

    assert_eq!(decoded, summary);
    assert_eq!(decoded.model_details[0].name, "frontier-a");
    assert_eq!(decoded.model_details[0].context_window, Some(200_000));
    assert_eq!(decoded.model_details[1].name, "frontier-b");
    assert_eq!(decoded.model_details[1].context_window, None);
}
