#![allow(clippy::expect_used)]

use super::*;
use haider_rpc::{ModelInventoryAuthorityWire, ProviderApiFamilyWire, ProviderAvailabilityWire};

#[test]
fn provider_trust_flags_are_mutually_exclusive() {
    let lockdown =
        parse(&["set".into(), "research".into(), "--lockdown".into()]).expect("lockdown flag");
    assert_eq!(lockdown.2, Some(ProviderTrustWire::Lockdown));
    let full = parse(&["set".into(), "research".into(), "--full".into()]).expect("full flag");
    assert_eq!(full.2, Some(ProviderTrustWire::Full));
    assert!(
        parse(&[
            "set".into(),
            "research".into(),
            "--lockdown".into(),
            "--full".into(),
        ])
        .is_err()
    );
}

/// MUTATION CHECK: remove `trust` from `ProviderSummaryWire`, skip lockdown
/// values, or rename the CLI schema. Expected failure: this exact document
/// no longer matches the machine-readable provider-list contract.
#[test]
fn provider_list_json_golden_exposes_lockdown_trust() {
    let document = ProviderListDocument {
        schema: "haider.providers.v1",
        revision: 7,
        providers: vec![ProviderSummaryWire {
            provider: "research".into(),
            api_family: ProviderApiFamilyWire::OpenAiChatCompletions,
            endpoint: None,
            response_open_timeout_ms: None,
            chunk_idle_timeout_ms: None,
            semantic_progress_timeout_ms: None,
            models: vec!["search-1".into()],
            model_details: Vec::new(),
            inventory_fetched_at_ms: None,
            inventory_authority: ModelInventoryAuthorityWire::Unknown,
            auth_methods: Vec::new(),
            availability: ProviderAvailabilityWire::Available,
            availability_reason: None,
            default_model: None,
            enabled: true,
            trust: ProviderTrustWire::Lockdown,
        }],
    };
    assert_eq!(
        serde_json::to_value(document).expect("provider list JSON"),
        serde_json::json!({
            "schema": "haider.providers.v1",
            "revision": 7,
            "providers": [{
                "provider": "research",
                "api_family": "openai_chat_completions",
                "models": ["search-1"],
                "model_details": [],
                "auth_methods": [],
                "availability": "available",
                "enabled": true,
                "trust": "lockdown"
            }]
        })
    );
}

#[test]
fn provider_list_json_does_not_omit_full_trust() {
    let provider = ProviderSummaryWire {
        provider: "built-in".into(),
        api_family: ProviderApiFamilyWire::OpenAiChatCompletions,
        endpoint: None,
        response_open_timeout_ms: None,
        chunk_idle_timeout_ms: None,
        semantic_progress_timeout_ms: None,
        models: Vec::new(),
        model_details: Vec::new(),
        inventory_fetched_at_ms: None,
        inventory_authority: ModelInventoryAuthorityWire::Unknown,
        auth_methods: Vec::new(),
        availability: ProviderAvailabilityWire::Available,
        availability_reason: None,
        default_model: None,
        enabled: true,
        trust: ProviderTrustWire::Full,
    };
    let value = serde_json::to_value(ProviderListDocument {
        schema: "haider.providers.v1",
        revision: 1,
        providers: vec![provider],
    })
    .expect("provider list JSON");
    assert_eq!(value["providers"][0]["trust"], "full");
}

#[test]
fn provider_show_json_golden_includes_the_fixed_envelope_and_quota() {
    let provider = ProviderSummaryWire {
        provider: "research".into(),
        api_family: ProviderApiFamilyWire::OpenAiChatCompletions,
        endpoint: Some("https://research.example.invalid/v1".into()),
        response_open_timeout_ms: None,
        chunk_idle_timeout_ms: None,
        semantic_progress_timeout_ms: None,
        models: vec!["search-1".into()],
        model_details: Vec::new(),
        inventory_fetched_at_ms: None,
        inventory_authority: ModelInventoryAuthorityWire::Advisory,
        auth_methods: Vec::new(),
        availability: ProviderAvailabilityWire::Available,
        availability_reason: None,
        default_model: Some("search-1".into()),
        enabled: true,
        trust: ProviderTrustWire::Lockdown,
    };
    let document = ProviderShowDocument {
        schema: "haider.provider.v1",
        revision: 9,
        provider,
        envelope: haider_rpc::LockdownStatusWire {
            provider: Some("research".into()),
            activation: Some(haider_rpc::LockdownActivationWire::Configured),
            reason: Some("the provider is explicitly configured for lockdown".into()),
            tools_allowed: vec!["fs_read".into(), "web_search".into()],
            quota_used: 4_096,
            quota_limit: 1_073_741_824,
        },
    };
    let value = serde_json::to_value(document).expect("provider show JSON");
    assert_eq!(value["provider"]["trust"], "lockdown");
    assert_eq!(value["envelope"]["provider"], "research");
    assert_eq!(value["envelope"]["activation"], "configured");
    assert_eq!(
        value["envelope"]["reason"],
        "the provider is explicitly configured for lockdown"
    );
    assert_eq!(
        value["envelope"]["tools_allowed"],
        serde_json::json!(["fs_read", "web_search"])
    );
    assert_eq!(value["envelope"]["quota_used"], 4_096);
    assert_eq!(value["envelope"]["quota_limit"], 1_073_741_824_u64);
}
