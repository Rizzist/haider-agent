use super::*;

fn inventory_summary() -> ProviderSummaryWire {
    ProviderSummaryWire {
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
        configured_models: vec![],
        static_models: Vec::new(),
        model_details: Vec::new(),
        catalog: haider_rpc::ProviderCatalogKindWire::Custom,
        response_open_timeout_ms: None,
        chunk_idle_timeout_ms: None,
        semantic_progress_timeout_ms: None,
        auth_methods: Vec::new(),
        availability_reason: None,
        trust: haider_rpc::ProviderTrustWire::Full,
    }
}

#[test]
fn selectable_uncertainty_does_not_refresh_during_admission() {
    let mut summary = inventory_summary();
    assert!(!provider_inventory_needs_refresh(
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
    summary.inventory = haider_rpc::ModelInventoryWire::Stale {
        fetched_at_ms: 1,
        reason: "server returned 503 for /models".into(),
    };
    assert!(!provider_inventory_needs_refresh(
        &summary,
        "configured-model"
    ));
    assert!(!provider_inventory_needs_refresh(&summary, "new-model"));
    summary.inventory_authority = haider_rpc::ModelInventoryAuthorityWire::Authoritative;
    assert!(!provider_inventory_needs_refresh(
        &summary,
        "configured-model"
    ));
    assert!(provider_inventory_needs_refresh(&summary, "new-model"));
    summary.availability = haider_rpc::ProviderAvailabilityWire::Unavailable;
    assert!(provider_inventory_needs_refresh(
        &summary,
        "configured-model"
    ));
    assert!(!summary.model_is_selectable("configured-model"));
    summary.availability = haider_rpc::ProviderAvailabilityWire::Available;
    summary.inventory = haider_rpc::ModelInventoryWire::Static;
    assert!(!provider_inventory_needs_refresh(&summary, "new-model"));
    summary.inventory = haider_rpc::ModelInventoryWire::Configured;
    summary.inventory_authority = haider_rpc::ModelInventoryAuthorityWire::Authoritative;
    assert!(!provider_inventory_needs_refresh(
        &summary,
        "configured-model"
    ));
}

#[test]
fn configured_model_eligibility_does_not_trigger_admission_discovery() {
    for catalog in [
        haider_rpc::ProviderCatalogKindWire::Custom,
        haider_rpc::ProviderCatalogKindWire::Public,
        haider_rpc::ProviderCatalogKindWire::Authenticated,
        haider_rpc::ProviderCatalogKindWire::Offline,
        haider_rpc::ProviderCatalogKindWire::Adapter,
    ] {
        for inventory in [
            haider_rpc::ModelInventoryWire::NeverFetched,
            haider_rpc::ModelInventoryWire::Configured,
            haider_rpc::ModelInventoryWire::Fetched {
                fetched_at_ms: u64::MAX,
            },
            haider_rpc::ModelInventoryWire::Stale {
                fetched_at_ms: 1,
                reason: "refresh due".into(),
            },
        ] {
            for availability in [
                haider_rpc::ProviderAvailabilityWire::Available,
                haider_rpc::ProviderAvailabilityWire::Unavailable,
            ] {
                let mut summary = inventory_summary();
                summary.catalog = catalog;
                summary.inventory = inventory.clone();
                summary.availability = availability;
                summary.configured_models = vec!["deployment".into()];
                summary.models = vec!["other".into()];
                assert!(
                    !provider_inventory_needs_refresh(&summary, "deployment"),
                    "{summary:?}"
                );
                if availability == haider_rpc::ProviderAvailabilityWire::Unavailable
                    || matches!(inventory, haider_rpc::ModelInventoryWire::Fetched { .. })
                {
                    assert!(
                        !summary.model_is_selectable("deployment"),
                        "negative proof is still enforced"
                    );
                }
            }
        }
    }
}
