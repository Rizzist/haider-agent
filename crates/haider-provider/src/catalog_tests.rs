#![allow(clippy::expect_used)]

use reqwest::header::AUTHORIZATION;

use crate::catalog::{CatalogSource, apply_catalog_credential};

fn request(source: &CatalogSource, credential: Option<&str>) -> reqwest::Request {
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("catalog test client");
    apply_catalog_credential(client.get(source.endpoint()), source, credential)
        .expect("catalog auth applies")
        .build()
        .expect("catalog request builds")
}

/// MUTATION CHECK: apply the new Gemini header mode globally or change any
/// existing source's bearer bytes. The old-source assertions pin the exact
/// pre-B6a behavior while the Gemini assertion pins the additive branch.
#[test]
fn catalog_auth_mode_is_source_specific_and_existing_sources_are_identical() {
    for source in [
        CatalogSource::OpenAiSubscription,
        CatalogSource::AnthropicSubscription,
        CatalogSource::KimiOAuth,
        CatalogSource::OpenAiCompatible {
            origin: "https://models.example.invalid/v1".into(),
        },
    ] {
        let request = request(&source, Some("CATALOG_BEARER_SENTINEL_71a2"));
        assert_eq!(
            request
                .headers()
                .get(AUTHORIZATION)
                .expect("existing bearer header"),
            "Bearer CATALOG_BEARER_SENTINEL_71a2"
        );
        assert!(!request.headers().contains_key("x-goog-api-key"));
    }

    let gemini = request(
        &CatalogSource::GeminiApiKey,
        Some("GEMINI_CATALOG_KEY_SENTINEL_42cd"),
    );
    let key = gemini
        .headers()
        .get("x-goog-api-key")
        .expect("Gemini catalog API key");
    assert_eq!(key, "GEMINI_CATALOG_KEY_SENTINEL_42cd");
    assert!(key.is_sensitive());
    assert!(!gemini.headers().contains_key(AUTHORIZATION));
}

#[test]
fn catalog_without_a_credential_keeps_auth_headers_absent() {
    for source in [
        CatalogSource::OpenAiSubscription,
        CatalogSource::AnthropicSubscription,
        CatalogSource::KimiOAuth,
        CatalogSource::GeminiApiKey,
        CatalogSource::OpenAiCompatible {
            origin: "http://127.0.0.1:11434/v1".into(),
        },
    ] {
        let request = request(&source, None);
        assert!(!request.headers().contains_key(AUTHORIZATION));
        assert!(!request.headers().contains_key("x-goog-api-key"));
    }
}

#[test]
fn invalid_gemini_catalog_header_is_sanitized() {
    let client = reqwest::Client::new();
    let secret = "invalid\nGEMINI_CATALOG_SECRET_eb20";
    let error = apply_catalog_credential(
        client.get(CatalogSource::GeminiApiKey.endpoint()),
        &CatalogSource::GeminiApiKey,
        Some(secret),
    )
    .expect_err("newline cannot enter a header");
    assert!(!error.to_string().contains(secret));
}
