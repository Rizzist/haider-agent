//! Explicitly gated Gemini smoke and sanitized capture-promotion harness.

#![allow(clippy::expect_used)]

use std::fs;
use std::path::PathBuf;

use haider_accounts::{CredentialAlias, MemoryVault, Vault, import_env};
use haider_protocol::provider::StreamEvent;
use haider_provider::{
    GeminiCapture, GeminiProvider, Message, Provider, ToolDefinition, TurnRequest,
    replay_gemini_sse,
};

const LIVE_GATE: &str = "HAIDER_LIVE_PROVIDER_TESTS";
const PROMOTE_GATE: &str = "HAIDER_PROMOTE_GEMINI_FIXTURES";
const PROMOTION_DIR_ENV: &str = "HAIDER_GEMINI_PROMOTION_DIR";
const KEY_ENV: &str = "HAIDER_GEMINI_API_KEY";
const MODEL_ENV: &str = "HAIDER_GEMINI_MODEL";

#[tokio::test]
#[ignore = "live Gemini smoke; requires HAIDER_LIVE_PROVIDER_TESTS=1"]
async fn live_gemini_text_smoke_is_explicitly_gated() {
    if std::env::var(LIVE_GATE).as_deref() != Ok("1") {
        return;
    }
    let (vault, model) = live_inputs();
    let provider = live_provider(&vault, &model);
    let mut stream = provider
        .stream_turn(text_request(&model, "Reply with exactly: haider-live-ok"))
        .await
        .expect("live Gemini stream starts");
    let mut saw_text = false;
    let mut saw_usage = false;
    let mut saw_finish = false;
    while let Some(item) = stream.recv().await {
        match item.expect("live Gemini stream item") {
            StreamEvent::TextDelta { text } => saw_text |= !text.is_empty(),
            StreamEvent::UsageUpdate(_) => saw_usage = true,
            StreamEvent::Finish { .. } => {
                saw_finish = true;
                break;
            }
            _ => {}
        }
    }
    assert!(saw_text);
    assert!(saw_usage);
    assert!(saw_finish);
}

/// Captures sanitized real text and tool SSE into an explicitly supplied
/// staging directory. Promotion into checked-in fixtures remains a reviewed
/// copy step; this harness never overwrites fixture provenance implicitly.
#[tokio::test]
#[ignore = "captures Gemini fixtures; requires live, promotion, and output-dir gates"]
async fn promote_sanitized_real_gemini_captures() {
    if std::env::var(LIVE_GATE).as_deref() != Ok("1")
        || std::env::var(PROMOTE_GATE).as_deref() != Ok("1")
    {
        return;
    }
    let output = PathBuf::from(
        std::env::var(PROMOTION_DIR_ENV)
            .expect("Gemini promotion requires HAIDER_GEMINI_PROMOTION_DIR"),
    );
    fs::create_dir_all(&output).expect("creates promotion staging directory");
    let (vault, model) = live_inputs();
    let secret = vault
        .resolve(&CredentialAlias::new("gemini-env"))
        .expect("resolves live key for redaction");
    let provider = live_provider(&vault, &model);
    let captures = [
        (
            "text",
            provider
                .capture_response(&text_request(&model, "Reply in one short sentence."))
                .await
                .expect("captures live text"),
        ),
        (
            "tool",
            provider
                .capture_response(&tool_request(&model))
                .await
                .expect("captures live tool call"),
        ),
    ];
    for (name, capture) in captures {
        assert_eq!(capture.status, 200, "capture `{name}` status");
        let body = sanitize_capture(capture, secret.expose_secret());
        let items = replay_gemini_sse(&body);
        assert!(items.iter().all(Result::is_ok), "capture `{name}` replays");
        assert!(
            items
                .iter()
                .any(|item| matches!(item, Ok(StreamEvent::Finish { .. })))
        );
        fs::write(output.join(format!("{name}.sse")), body).expect("writes staged capture");
    }
}

fn live_inputs() -> (MemoryVault, String) {
    let vault = MemoryVault::new();
    let alias =
        import_env(&vault, "gemini", KEY_ENV).expect("imports live key through accounts bridge");
    assert_eq!(alias, CredentialAlias::new("gemini-env"));
    let model = std::env::var(MODEL_ENV).unwrap_or_else(|_| "gemini-2.5-flash".into());
    (vault, model)
}

fn live_provider(vault: &MemoryVault, model: &str) -> GeminiProvider {
    GeminiProvider::new(
        vault
            .resolve(&CredentialAlias::new("gemini-env"))
            .expect("resolves imported live key"),
        model,
    )
    .expect("constructs live Gemini provider")
    .with_account(CredentialAlias::new("gemini-env"))
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
            "Call lookup_weather exactly once for Example City; do not answer directly.",
        )],
        model: model.into(),
        max_tokens: 256,
        system_prompt: None,
        tools: vec![ToolDefinition {
            name: "lookup_weather".into(),
            description: "Look up weather".into(),
            input_schema: serde_json::json!({
                "type":"object",
                "properties":{"city":{"type":"string"}},
                "required":["city"]
            }),
        }],
        attachments: Vec::new(),
    }
}

fn sanitize_capture(capture: GeminiCapture, secret: &[u8]) -> Vec<u8> {
    let body = redact_bytes(&capture.body, secret);
    let text = String::from_utf8(body).expect("Gemini SSE is UTF-8");
    let mut sanitized = String::new();
    for line in text.lines() {
        if let Some(data) = line.strip_prefix("data: ")
            && let Ok(mut value) = serde_json::from_str::<serde_json::Value>(data)
        {
            sanitize_json(&mut value);
            sanitized.push_str("data: ");
            sanitized.push_str(&value.to_string());
            sanitized.push_str("\n\n");
        }
    }
    sanitized.into_bytes()
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
                if key == "thoughtSignature" {
                    *value = serde_json::Value::String("thought-signature-sanitized".into());
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
