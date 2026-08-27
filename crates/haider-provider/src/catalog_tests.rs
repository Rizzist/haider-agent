#![allow(clippy::expect_used)]

use std::net::SocketAddr;
use std::sync::Arc;

use reqwest::header::AUTHORIZATION;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::catalog::{
    CatalogError, CatalogSource, apply_catalog_credential, discover_models,
    discover_models_with_resolver,
};

struct OneAddressResolver(SocketAddr);

#[async_trait::async_trait]
impl crate::origin::FixedDnsResolver for OneAddressResolver {
    async fn resolve(&self, _host: &str, _port: u16) -> std::io::Result<Vec<SocketAddr>> {
        Ok(vec![self.0])
    }
}

async fn one_shot_catalog(
    status: &'static str,
    body: &'static [u8],
) -> (String, tokio::task::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind one-shot catalog");
    let origin = format!("http://{}", listener.local_addr().expect("catalog address"));
    let fixture = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept catalog request");
        let mut request = vec![0_u8; 8_192];
        let read = socket
            .read(&mut request)
            .await
            .expect("read catalog request");
        let request = String::from_utf8_lossy(&request[..read]).into_owned();
        let headers = format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        socket
            .write_all(headers.as_bytes())
            .await
            .expect("write catalog headers");
        socket.write_all(body).await.expect("write catalog body");
        request
    });
    (origin, fixture)
}

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

/// MUTATION CHECK: apply the Gemini/Azure/Anthropic header modes globally,
/// route Azure login discovery back through Bearer, or change an existing
/// source's bearer bytes. The assertions pin each credential family and the
/// absence of an `Authorization` header on API-key-specific routes.
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

    let anthropic = request(
        &CatalogSource::AnthropicCompatible {
            origin: "https://anthropic-compatible.example.invalid".into(),
        },
        Some("ANTHROPIC_CATALOG_KEY_SENTINEL_8ee4"),
    );
    let key = anthropic
        .headers()
        .get("x-api-key")
        .expect("custom Anthropic catalog API key");
    assert_eq!(key, "ANTHROPIC_CATALOG_KEY_SENTINEL_8ee4");
    assert!(key.is_sensitive());
    assert!(!anthropic.headers().contains_key(AUTHORIZATION));

    let azure = request(
        &CatalogSource::OpenAiCompatible {
            origin: "https://contoso.openai.azure.com/openai/v1".into(),
        },
        Some("AZURE_CATALOG_KEY_SENTINEL_1d8f"),
    );
    let key = azure
        .headers()
        .get("api-key")
        .expect("Azure catalog API key");
    assert_eq!(key, "AZURE_CATALOG_KEY_SENTINEL_1d8f");
    assert!(key.is_sensitive());
    assert!(!azure.headers().contains_key(AUTHORIZATION));
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
        CatalogSource::AnthropicCompatible {
            origin: "http://127.0.0.1:11434/v1".into(),
        },
    ] {
        let request = request(&source, None);
        assert!(!request.headers().contains_key(AUTHORIZATION));
        assert!(!request.headers().contains_key("x-goog-api-key"));
        assert!(!request.headers().contains_key("x-api-key"));
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
        let lower = request.to_ascii_lowercase();
        assert!(!lower.contains("authorization:"));
        assert!(!lower.contains("api-key:"));

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

/// MUTATION CHECK: replace the caller-supplied resolver with the system
/// resolver in the custom branch. The reserved fixture hostname no longer
/// reaches the pinned loopback server and discovery fails.
#[tokio::test]
async fn custom_catalog_discovery_uses_its_injected_resolver_for_the_request() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind pinned catalog fixture");
    let address = listener.local_addr().expect("pinned catalog address");
    let origin = format!("http://router.test:{}/v1", address.port());
    let fixture = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept pinned request");
        let mut request = vec![0_u8; 4096];
        let read = socket
            .read(&mut request)
            .await
            .expect("read pinned request");
        let request = String::from_utf8_lossy(&request[..read]);
        assert!(request.starts_with("GET /v1/models HTTP/1.1"));
        assert!(request.to_ascii_lowercase().contains("host: router.test:"));
        let body = br#"{"data":[{"id":"pinned-model"}]}"#;
        let head = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        socket
            .write_all(head.as_bytes())
            .await
            .expect("write pinned headers");
        socket.write_all(body).await.expect("write pinned body");
    });

    let catalog = discover_models_with_resolver(
        CatalogSource::OpenAiCompatible { origin },
        Some("resolver-sentinel"),
        None,
        Arc::new(OneAddressResolver(address)),
    )
    .await
    .expect("custom discovery through injected resolver");
    fixture.await.expect("pinned fixture joins");
    assert_eq!(catalog.models[0].slug, "pinned-model");
}

#[tokio::test]
async fn custom_anthropic_discovery_uses_standard_keyed_models_get() {
    let (origin, fixture) = one_shot_catalog(
        "200 OK",
        br#"{"data":[{"id":"claude-local","type":"model"}]}"#,
    )
    .await;
    let catalog = discover_models(
        CatalogSource::AnthropicCompatible { origin },
        Some("anthropic-catalog-secret"),
        None,
    )
    .await
    .expect("custom Anthropic discovery");
    let request = fixture.await.expect("catalog fixture");
    assert!(request.starts_with("GET /v1/models HTTP/1.1"));
    assert!(request.contains("x-api-key: anthropic-catalog-secret\r\n"));
    assert!(request.contains("anthropic-version: 2023-06-01\r\n"));
    assert!(!request.to_ascii_lowercase().contains("authorization:"));
    assert_eq!(catalog.models[0].slug, "claude-local");
}

/// The daemon maps these three discovery errors to the public Q probe
/// taxonomy; pin the transport/parser source classifications at the mocked
/// `/v1/models` boundary.
#[tokio::test]
async fn custom_catalog_reports_unauthorized_invalid_body_and_empty_list() {
    for (status, body, expected) in [
        (
            "401 Unauthorized",
            br#"{"error":"unauthorized"}"#.as_slice(),
            "unauthorized",
        ),
        ("200 OK", b"not-json".as_slice(), "invalid_body"),
        ("200 OK", br#"{"data":[]}"#.as_slice(), "empty"),
    ] {
        let (origin, fixture) = one_shot_catalog(status, body).await;
        let error = discover_models(CatalogSource::OpenAiCompatible { origin }, None, None)
            .await
            .expect_err("scripted catalog failure");
        fixture.await.expect("catalog fixture");
        assert!(matches!(
            (expected, error),
            ("unauthorized", CatalogError::Unauthorized)
                | ("invalid_body", CatalogError::InvalidBody { .. })
                | ("empty", CatalogError::Empty)
        ));
    }
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
