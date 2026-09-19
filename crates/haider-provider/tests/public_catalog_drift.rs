//! Nightly, unauthenticated contract probe; ordinary tests remain hermetic.
#![allow(clippy::expect_used)]

use haider_provider::{ProviderCatalogDefinition, discover_models, provider_catalog_definition};

#[tokio::test]
#[ignore = "live public catalogs; scheduled nightly and explicit local proof"]
async fn public_catalog_drift() {
    for provider in haider_provider::PUBLIC_CATALOG_PROVIDERS {
        let definition = provider_catalog_definition(provider);
        let ProviderCatalogDefinition::Public { source } = &definition else {
            panic!("public catalog provider {provider} lost its public definition");
        };
        let live = discover_models(source.clone(), None, None)
            .await
            .expect("public catalog must be available without credentials");
        let ids = live
            .models
            .iter()
            .map(|model| model.slug.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        assert!(!ids.is_empty(), "{provider}: empty live catalog");
        // Remote variants cannot ship a static list. Keep the subset law
        // explicit so widening the definition later retains the drift gate.
        for shipped in definition.static_models() {
            assert!(
                ids.contains(shipped),
                "{provider}: shipped model {shipped} is missing from live catalog"
            );
        }
        println!(
            "{provider}: {} public models; shipped static list is a subset",
            ids.len()
        );
    }
}

#[test]
fn remote_catalog_definitions_cannot_supply_static_models() {
    for provider in [
        "haider-code",
        "deepseek",
        "xai",
        "openai-oauth",
        "anthropic-oauth",
        "kimi-oauth",
        "grok-oauth",
        "gemini",
    ] {
        assert!(
            provider_catalog_definition(provider)
                .static_models()
                .is_empty()
        );
    }
    assert!(
        !provider_catalog_definition("bedrock")
            .static_models()
            .is_empty()
    );
    assert!(
        !provider_catalog_definition("vertex")
            .static_models()
            .is_empty()
    );
}
