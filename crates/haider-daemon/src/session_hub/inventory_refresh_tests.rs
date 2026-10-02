use super::*;

#[test]
fn configured_advisory_inventory_probes_once_but_azure_unsupported_does_not() {
    let mut summary = ProviderSummaryWire {
        provider: "compatible".into(),
        api_family: haider_rpc::ProviderApiFamilyWire::OpenAiChatCompletions,
        endpoint: Some("http://127.0.0.1:8080".into()),
        inventory: haider_rpc::ModelInventoryWire::Configured,
        inventory_authority: haider_rpc::ModelInventoryAuthorityWire::Advisory,
        models: vec!["configured-model".into()],
        availability: haider_rpc::ProviderAvailabilityWire::Available,
        default_model: None,
        enabled: true,
        catalog_scope: None,
        configured_models: vec!["configured-model".into()],
        static_models: Vec::new(),
        model_details: Vec::new(),
        catalog: haider_rpc::ProviderCatalogKindWire::Custom,
        response_open_timeout_ms: None,
        chunk_idle_timeout_ms: None,
        semantic_progress_timeout_ms: None,
        auth_methods: Vec::new(),
        availability_reason: None,
        trust: haider_rpc::ProviderTrustWire::Full,
    };
    assert!(provider_inventory_needs_refresh(
        &summary,
        "configured-model"
    ));
    summary.endpoint = Some("https://example.openai.azure.com/openai/v1".into());
    assert!(!provider_inventory_needs_refresh(
        &summary,
        "configured-model"
    ));
    summary.endpoint = Some("http://127.0.0.1:8080".into());
    summary.inventory = haider_rpc::ModelInventoryWire::Fetched { fetched_at_ms: 1 };
    assert!(!provider_inventory_needs_refresh(
        &summary,
        "configured-model"
    ));
    assert!(provider_inventory_needs_refresh(&summary, "new-model"));
    summary.inventory = haider_rpc::ModelInventoryWire::Static;
    assert!(!provider_inventory_needs_refresh(&summary, "new-model"));
    summary.inventory = haider_rpc::ModelInventoryWire::Configured;
    summary.inventory_authority = haider_rpc::ModelInventoryAuthorityWire::Authoritative;
    assert!(!provider_inventory_needs_refresh(
        &summary,
        "configured-model"
    ));
}
