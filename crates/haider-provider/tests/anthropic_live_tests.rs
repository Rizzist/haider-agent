//! Explicitly gated network smoke and fixture-promotion harness.
//!
//! These tests are ignored by default. The C1 lane must not run them.

#![allow(clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};

use haider_accounts::{CredentialAlias, MemoryVault, Vault, import_env};
use haider_protocol::ids::ArtifactRef;
use haider_protocol::provider::{Block, StreamEvent};
use haider_protocol::tool::AttachmentBlock;
use haider_provider::{
    AnthropicCapture, AnthropicProvider, Message, MessageRole, Provider, ResolvedAttachment,
    ToolDefinition, TurnRequest, replay_anthropic_http_error, replay_anthropic_sse,
};
use serde::Serialize;

const LIVE_GATE: &str = "HAIDER_LIVE_PROVIDER_TESTS";
const PROMOTE_GATE: &str = "HAIDER_PROMOTE_ANTHROPIC_FIXTURES";
const KEY_ENV: &str = "HAIDER_ANTHROPIC_API_KEY";
const MODEL_ENV: &str = "HAIDER_ANTHROPIC_MODEL";
const RATE_LIMIT_URL_ENV: &str = "HAIDER_ANTHROPIC_CAPTURE_429_URL";
const MALFORMED_URL_ENV: &str = "HAIDER_ANTHROPIC_CAPTURE_MALFORMED_URL";

#[tokio::test]
#[ignore = "live Anthropic smoke; requires HAIDER_LIVE_PROVIDER_TESTS=1"]
async fn live_anthropic_text_smoke_is_explicitly_gated() {
    if std::env::var(LIVE_GATE).as_deref() != Ok("1") {
        return;
    }
    let vault = import_live_key();
    let model = selected_model();
    let provider = live_provider(&vault, &model);
    let mut stream = provider
        .stream_turn(text_request(&model, "Reply with exactly: haider-live-ok"))
        .await
        .expect("live stream starts");
    let mut saw_text = false;
    let mut saw_finish = false;
    while let Some(item) = stream.recv().await {
        match item.expect("live stream item") {
            StreamEvent::TextDelta { text } => saw_text |= !text.is_empty(),
            StreamEvent::Finish { .. } => {
                saw_finish = true;
                break;
            }
            _ => {}
        }
    }
    assert!(saw_text);
    assert!(saw_finish);
}

#[tokio::test]
#[ignore = "promotes fixtures; requires both live and explicit promotion gates"]
async fn promote_sanitized_real_anthropic_captures() {
    if std::env::var(LIVE_GATE).as_deref() != Ok("1")
        || std::env::var(PROMOTE_GATE).as_deref() != Ok("1")
    {
        return;
    }
    let rate_limit_url = std::env::var(RATE_LIMIT_URL_ENV)
        .expect("promotion requires an endpoint that returns a real captured 429");
    let malformed_url = std::env::var(MALFORMED_URL_ENV)
        .expect("promotion requires an endpoint that returns a captured malformed stream");
    let vault = import_live_key();
    let capture_secret = vault
        .resolve(&CredentialAlias::new("anthropic-env"))
        .expect("resolves key for capture redaction");
    let model = selected_model();
    let image_ref = ArtifactRef::new("blake3:capture-image");

    let shapes = [
        CaptureShape::new(
            "text_only",
            text_request(&model, "Reply in one short sentence."),
            None,
            200,
        ),
        CaptureShape::new("tool_call", tool_request(&model), None, 200),
        CaptureShape::new("image_in", image_request(&model, image_ref), None, 200),
        CaptureShape::new("usage_heavy", usage_request(&model), None, 200),
        CaptureShape::new(
            "rate_limit",
            text_request(&model, "Return the configured rate-limit capture."),
            Some(rate_limit_url),
            429,
        ),
        CaptureShape::new(
            "malformed",
            text_request(&model, "Return the configured malformed capture."),
            Some(malformed_url),
            200,
        ),
    ];

    // Capture and validate everything before replacing any fixture.
    let mut captured = Vec::new();
    for shape in shapes {
        let mut provider = live_provider(&vault, &model);
        if let Some(api_url) = &shape.api_url {
            provider = provider.with_api_url(api_url);
        }
        let response = provider
            .capture_response(&shape.request)
            .await
            .expect("capture request succeeds");
        assert_eq!(
            response.status, shape.expected_status,
            "shape `{}` returned an unexpected status",
            shape.name
        );
        captured.push((
            shape,
            sanitize_capture(response, capture_secret.expose_secret()),
        ));
    }

    let directory = fixture_directory();
    let mut entries = Vec::new();
    for (shape, response) in captured {
        if response.status == 200 {
            let wire_name = format!("{}.sse", shape.name);
            let golden_name = format!("{}.events.json", shape.name);
            let items = replay_anthropic_sse(&response.body);
            assert!(
                !items.is_empty(),
                "shape `{}` produced no replay items",
                shape.name
            );
            write_bytes(&directory.join(&wire_name), &response.body);
            write_json(&directory.join(&golden_name), &golden_items(items));
            entries.push(ManifestEntry {
                name: shape.name,
                transport: "sse",
                status: response.status,
                retry_after: None,
                wire: wire_name,
                golden: golden_name,
            });
        } else {
            let wire_name = format!("{}.http.json", shape.name);
            let golden_name = format!("{}.error.json", shape.name);
            let error = replay_anthropic_http_error(
                response.status,
                response.retry_after.as_deref(),
                &response.body,
            );
            write_bytes(&directory.join(&wire_name), &response.body);
            write_json(&directory.join(&golden_name), &error);
            entries.push(ManifestEntry {
                name: shape.name,
                transport: "http",
                status: response.status,
                retry_after: response.retry_after,
                wire: wire_name,
                golden: golden_name,
            });
        }
    }
    let manifest = Manifest {
        schema: "haider.anthropic-fixtures.v1",
        provisional: false,
        provenance: "Sanitized real captures promoted by tests/anthropic_live_tests.rs; IDs stripped.",
        fixtures: entries,
    };
    write_json(&directory.join("manifest.json"), &manifest);
}

struct CaptureShape {
    name: &'static str,
    request: TurnRequest,
    api_url: Option<String>,
    expected_status: u16,
}

impl CaptureShape {
    fn new(
        name: &'static str,
        request: TurnRequest,
        api_url: Option<String>,
        expected_status: u16,
    ) -> Self {
        Self {
            name,
            request,
            api_url,
            expected_status,
        }
    }
}

#[derive(Serialize)]
struct Manifest {
    schema: &'static str,
    provisional: bool,
    provenance: &'static str,
    fixtures: Vec<ManifestEntry>,
}

#[derive(Serialize)]
struct ManifestEntry {
    name: &'static str,
    transport: &'static str,
    status: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    retry_after: Option<String>,
    wire: String,
    golden: String,
}

fn import_live_key() -> MemoryVault {
    let vault = MemoryVault::new();
    import_env(&vault, "anthropic", KEY_ENV).expect("imports live key through accounts bridge");
    vault
}

fn live_provider(vault: &MemoryVault, model: &str) -> AnthropicProvider {
    let alias = CredentialAlias::new("anthropic-env");
    let handle = vault.resolve(&alias).expect("resolves imported live key");
    AnthropicProvider::new(handle, model)
        .expect("HTTP client constructs")
        .with_account(alias)
}

fn selected_model() -> String {
    std::env::var(MODEL_ENV).unwrap_or_else(|_| "claude-haiku-4-5".into())
}

fn text_request(model: &str, prompt: &str) -> TurnRequest {
    TurnRequest {
        messages: vec![Message::user_text(prompt)],
        model: model.into(),
        max_tokens: 128,
        system_prompt: None,
        tools: Vec::new(),
        attachments: Vec::new(),
    }
}

fn tool_request(model: &str) -> TurnRequest {
    TurnRequest {
        messages: vec![Message::user_text(
            "Call get_weather exactly once for Tehran; do not answer directly.",
        )],
        model: model.into(),
        max_tokens: 256,
        system_prompt: None,
        tools: vec![ToolDefinition {
            name: "get_weather".into(),
            description: "Get current weather for a city".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {"city": {"type": "string"}},
                "required": ["city"]
            }),
        }],
        attachments: Vec::new(),
    }
}

fn image_request(model: &str, artifact: ArtifactRef) -> TurnRequest {
    TurnRequest {
        messages: vec![Message {
            role: MessageRole::User,
            blocks: vec![
                Block::Attachment(AttachmentBlock::Image {
                    artifact: artifact.clone(),
                    mime: "image/png".into(),
                    width: Some(1),
                    height: Some(1),
                }),
                Block::Text {
                    text: "Describe this image briefly.".into(),
                },
            ],
        }],
        model: model.into(),
        max_tokens: 128,
        system_prompt: None,
        tools: Vec::new(),
        attachments: vec![ResolvedAttachment {
            artifact,
            data_base64:
                "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII="
                    .into(),
        }],
    }
}

fn usage_request(model: &str) -> TurnRequest {
    TurnRequest {
        messages: vec![Message::user_text(
            "Summarize the system context in five words.",
        )],
        model: model.into(),
        max_tokens: 512,
        system_prompt: Some("stable usage context ".repeat(4_000)),
        tools: Vec::new(),
        attachments: Vec::new(),
    }
}

fn sanitize_capture(mut capture: AnthropicCapture, secret: &[u8]) -> AnthropicCapture {
    capture.body = redact_bytes(&capture.body, secret);
    if let Ok(text) = std::str::from_utf8(&capture.body) {
        if text.lines().any(|line| line.starts_with("data:")) {
            let mut sanitized = String::new();
            for line in text.lines() {
                if let Some(data) = line.strip_prefix("data: ")
                    && let Ok(mut value) = serde_json::from_str::<serde_json::Value>(data)
                {
                    sanitize_json(&mut value);
                    sanitized.push_str("data: ");
                    sanitized.push_str(&value.to_string());
                    sanitized.push('\n');
                    continue;
                }
                sanitized.push_str(line);
                sanitized.push('\n');
            }
            capture.body = sanitized.into_bytes();
        } else if let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(&capture.body) {
            sanitize_json(&mut value);
            capture.body = serde_json::to_vec_pretty(&value).expect("sanitized body serializes");
        }
    }
    capture
}

fn redact_bytes(input: &[u8], secret: &[u8]) -> Vec<u8> {
    if secret.is_empty() {
        return input.to_vec();
    }
    let mut output = Vec::with_capacity(input.len());
    let mut remaining = input;
    while let Some(index) = remaining
        .windows(secret.len())
        .position(|window| window == secret)
    {
        output.extend_from_slice(&remaining[..index]);
        output.extend_from_slice(b"[REDACTED]");
        remaining = &remaining[index + secret.len()..];
    }
    output.extend_from_slice(remaining);
    output
}

fn sanitize_json(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(object) => {
            for (key, value) in object {
                if matches!(key.as_str(), "id" | "request_id")
                    && matches!(value, serde_json::Value::String(_))
                {
                    *value = serde_json::Value::String(sanitized_id(key));
                } else {
                    sanitize_json(value);
                }
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                sanitize_json(value);
            }
        }
        _ => {}
    }
}

fn sanitized_id(key: &str) -> String {
    match key {
        "request_id" => "req_sanitized".into(),
        _ => "id_sanitized".into(),
    }
}

fn golden_items(
    items: Vec<Result<StreamEvent, haider_provider::ProviderError>>,
) -> Vec<serde_json::Value> {
    items
        .into_iter()
        .map(|item| match item {
            Ok(event) => serde_json::json!({"result": "ok", "value": event}),
            Err(error) => serde_json::json!({"result": "err", "value": error}),
        })
        .collect()
}

fn fixture_directory() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/anthropic")
}

fn write_bytes(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).expect("writes promoted fixture");
}

fn write_json(path: &Path, value: &impl Serialize) {
    let mut bytes = serde_json::to_vec_pretty(value).expect("serializes promoted fixture");
    bytes.push(b'\n');
    write_bytes(path, &bytes);
}
