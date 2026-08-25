#![allow(clippy::expect_used)]

use reqwest::header::AUTHORIZATION;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::catalog::{CatalogSource, apply_catalog_credential, discover_models};

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
        CatalogSource::GrokOAuth,
        CatalogSource::DeepSeekApi,
        CatalogSource::HaiderCodeApi,
        CatalogSource::XaiApi,
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
        CatalogSource::GrokOAuth,
        CatalogSource::GeminiApiKey,
        CatalogSource::DeepSeekApi,
        CatalogSource::HaiderCodeApi,
        CatalogSource::XaiApi,
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

/// Option 1 changes only pinned turn startup. The unpinned catalog path must
/// still perform real discovery and return the provider's catalog unchanged.
///
/// MUTATION CHECK: short-circuit `discover_models` globally while removing
/// the per-turn membership probe. Expected runtime failure: the fixture sees
/// no GET or this exact discovered slug is absent.
#[tokio::test]
async fn unpinned_compatible_catalog_still_discovers() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind catalog fixture");
    let origin = format!(
        "http://{}/v1",
        listener.local_addr().expect("catalog fixture address")
    );
    let fixture = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept catalog request");
        let mut request = vec![0_u8; 4096];
        let read = socket
            .read(&mut request)
            .await
            .expect("read catalog request");
        let request = String::from_utf8_lossy(&request[..read]);
        assert!(
            request.starts_with("GET /v1/models HTTP/1.1"),
            "unexpected catalog request: {request}"
        );

        let body = br#"{"data":[{"id":"discovered-model","object":"model"}]}"#;
        let headers = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        socket
            .write_all(headers.as_bytes())
            .await
            .expect("write catalog headers");
        socket.write_all(body).await.expect("write catalog body");
    });

    let deadline = std::time::Duration::from_secs(2);
    let catalog = tokio::time::timeout(
        deadline,
        discover_models(CatalogSource::OpenAiCompatible { origin }, None, None),
    )
    .await
    .expect("unpinned discovery request deadline")
    .expect("unpinned discovery succeeds");
    tokio::time::timeout(deadline, fixture)
        .await
        .expect("catalog fixture deadline")
        .expect("catalog fixture joins");
    assert_eq!(catalog.models.len(), 1);
    assert_eq!(catalog.models[0].slug, "discovered-model");
}

/// WH3 request half — the named DeepSeek catalog source is a Bearer GET to
/// the fixed vendor `/models` endpoint, never a custom-origin request.
#[test]
fn wh3_deepseek_catalog_source_builds_fixed_models_get() {
    let request = request(
        &CatalogSource::DeepSeekApi,
        Some("DEEPSEEK_CATALOG_KEY_SENTINEL_19be"),
    );
    assert_eq!(request.method(), reqwest::Method::GET);
    assert_eq!(request.url().as_str(), "https://api.deepseek.com/models");
    let authorization = request
        .headers()
        .get(AUTHORIZATION)
        .expect("DeepSeek catalog Bearer");
    assert_eq!(
        authorization.as_bytes(),
        b"Bearer DEEPSEEK_CATALOG_KEY_SENTINEL_19be"
    );
    assert!(authorization.is_sensitive());
}

/// MUTATION CHECK: route Haider Code discovery through a custom origin or
/// drop bearer auth. Expected runtime failure: the fixed endpoint or exact
/// sensitive Authorization header assertion changes.
#[test]
fn haider_code_catalog_source_builds_fixed_bearer_models_get() {
    let request = request(
        &CatalogSource::HaiderCodeApi,
        Some("HAIDER_CODE_CATALOG_KEY_SENTINEL_f184"),
    );
    assert_eq!(request.method(), reqwest::Method::GET);
    assert_eq!(request.url().as_str(), "https://haidercode.ai/v1/models");
    let authorization = request
        .headers()
        .get(AUTHORIZATION)
        .expect("Haider Code catalog Bearer");
    assert_eq!(
        authorization.as_bytes(),
        b"Bearer HAIDER_CODE_CATALOG_KEY_SENTINEL_f184"
    );
    assert!(authorization.is_sensitive());
}

/// MUTATION CHECK: treating the proxy catalog as a generic OpenAI list loses
/// its provider-declared context and reasoning facts.
#[test]
fn grok_oauth_catalog_parses_proxy_model_metadata() {
    let models = crate::parse_catalog(
        CatalogSource::GrokOAuth,
        &serde_json::json!({"data": [{
            "id": "grok-4.6",
            "name": "Grok 4.6",
            "context_window": 500_000,
            "supports_reasoning_effort": true
        }]}),
    )
    .expect("parse Grok proxy catalog");
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].slug, "grok-4.6");
    assert_eq!(models[0].display_name, "Grok 4.6");
    assert_eq!(models[0].context_window, Some(500_000));
    assert!(
        models[0]
            .extensions
            .as_ref()
            .expect("Grok metadata")
            .supports_reasoning_effort
    );
    assert_eq!(
        CatalogSource::GrokOAuth.endpoint(),
        "https://cli-chat-proxy.grok.com/v1/models"
    );
    assert_eq!(
        CatalogSource::XaiApi.endpoint(),
        "https://api.x.ai/v1/models"
    );
}
