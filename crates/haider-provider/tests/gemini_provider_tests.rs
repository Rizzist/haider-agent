#![allow(clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use haider_accounts::{CredentialAlias, MemoryVault, Vault};
use haider_protocol::ids::ArtifactRef;
use haider_protocol::provider::{Block, FeatureResolve, FinishReason, StreamEvent};
use haider_protocol::tool::AttachmentBlock;
use haider_provider::{
    GeminiProvider, GeminiRetryPolicy, Message, MessageRole, Provider, ProviderError,
    ProviderErrorKind, ProviderStreamItem, ResolvedAttachment, ToolDefinition, TurnRequest,
    replay_gemini_http_error, replay_gemini_sse,
};
use serde::Deserialize;

const FIXTURE_DIR: &str = "tests/fixtures/gemini";

#[derive(Debug, Deserialize)]
struct Manifest {
    schema: String,
    provisional: bool,
    provenance: String,
    fixtures: Vec<Fixture>,
}

#[derive(Debug, Deserialize)]
struct Fixture {
    name: String,
    transport: String,
    status: u16,
    retry_after: Option<String>,
    wire: String,
    golden: String,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "result", content = "value", rename_all = "snake_case")]
enum ExpectedItem {
    Ok(StreamEvent),
    Err(ProviderError),
}

impl ExpectedItem {
    fn into_result(self) -> ProviderStreamItem {
        match self {
            Self::Ok(event) => Ok(event),
            Self::Err(error) => Err(error),
        }
    }
}

#[test]
fn manifest_replays_every_declared_gemini_fixture_in_either_promotion_state() {
    let directory = fixture_directory();
    let bytes = fs::read(directory.join("manifest.json")).expect("manifest bytes");
    let raw: serde_json::Value = serde_json::from_slice(&bytes).expect("manifest JSON");
    assert!(matches!(
        raw.get("provisional"),
        Some(serde_json::Value::Bool(_))
    ));
    let manifest: Manifest = serde_json::from_slice(&bytes).expect("typed manifest");
    assert_eq!(manifest.schema, "haider.gemini-fixtures.v1");
    assert_eq!(raw["provisional"], manifest.provisional);
    assert!(!manifest.provenance.trim().is_empty());
    assert!(!manifest.fixtures.is_empty());

    for fixture in manifest.fixtures {
        let wire = fs::read(directory.join(&fixture.wire)).expect("fixture wire");
        match fixture.transport.as_str() {
            "sse" => {
                let expected: Vec<ExpectedItem> = read_json(&directory.join(&fixture.golden));
                let expected = expected
                    .into_iter()
                    .map(ExpectedItem::into_result)
                    .collect::<Vec<_>>();
                assert_eq!(replay_gemini_sse(&wire), expected, "{}", fixture.name);
            }
            "http" => {
                let expected: ProviderError = read_json(&directory.join(&fixture.golden));
                assert_eq!(
                    replay_gemini_http_error(fixture.status, fixture.retry_after.as_deref(), &wire,),
                    expected,
                    "{}",
                    fixture.name
                );
            }
            other => panic!("unknown Gemini fixture transport `{other}`"),
        }
    }
}

/// Combined golden for text, visible thinking, opaque thought signatures,
/// one whole-args function call, reported usage, and tool-use termination.
#[test]
fn gemini_stream_decodes_text_reasoning_toolcall_usage_finish() {
    let directory = fixture_directory();
    let expected: Vec<ExpectedItem> = read_json(&directory.join("combined.events.json"));
    assert_eq!(
        replay_gemini_sse(&fs::read(directory.join("combined.sse")).expect("combined wire")),
        expected
            .into_iter()
            .map(ExpectedItem::into_result)
            .collect::<Vec<_>>()
    );
}

/// The mandatory poison-every-session regression: decode the real first
/// response, rebuild the call-id-centric history in the same split-message
/// shape as durable core, add the result, and compare the complete next wire
/// request with a checked-in name-keyed functionResponse golden.
#[test]
fn two_turn_tool_roundtrip_continuation_payload_is_stable() {
    let directory = fixture_directory();
    let items = replay_gemini_sse(
        &fs::read(directory.join("two_turn_call.sse")).expect("two-turn response"),
    );
    let mut opaque = None;
    let mut call_id = None;
    let mut name = None;
    let mut args = String::new();
    for item in items {
        match item.expect("two-turn response is valid") {
            StreamEvent::ProviderOpaque { provider, data } => {
                opaque = Some(Block::ProviderOpaque { provider, data });
            }
            StreamEvent::ToolCallStart {
                call_id: id,
                name: tool_name,
            } => {
                call_id = Some(id);
                name = Some(tool_name);
            }
            StreamEvent::ToolCallArgsDelta {
                call_id: fragment_id,
                args_fragment,
            } => {
                assert_eq!(Some(fragment_id.as_str()), call_id.as_deref());
                args.push_str(&args_fragment);
            }
            StreamEvent::ToolCallEnd { call_id: end_id } => {
                assert_eq!(Some(end_id.as_str()), call_id.as_deref());
            }
            _ => {}
        }
    }
    let call_id = call_id.expect("synthesized call id");
    let name = name.expect("function name");
    let args: serde_json::Value = serde_json::from_str(&args).expect("complete args JSON");
    let provider = provider("gemini-2.5-flash");
    let request = TurnRequest {
        messages: vec![
            Message::user_text("What is the weather in Tehran?"),
            // Core reconstructs the opaque extension and tool completion as
            // separate assistant messages. The encoder must coalesce them.
            Message::assistant(vec![opaque.expect("signed native part")]),
            Message::assistant(vec![Block::ToolCall {
                call_id: call_id.clone(),
                name,
                args,
            }]),
            Message::tool_result(call_id, "sunny, 28 C", false),
        ],
        model: "gemini-2.5-flash".into(),
        max_tokens: 64,
        system_prompt: None,
        tools: vec![weather_tool()],
        attachments: Vec::new(),
    };
    let actual = provider
        .request_payload(&request)
        .expect("continuation request encodes");
    let expected: serde_json::Value =
        read_json(&directory.join("two_turn_continuation.request.json"));
    assert_eq!(actual, expected);
    assert_eq!(
        actual["contents"][2]["parts"][0]["functionResponse"]["name"], "lookup_weather",
        "the normalized call id must recover Gemini's required function name"
    );
}

#[test]
fn synthesized_call_ids_are_deterministic_and_replay_maps_to_names() {
    let bytes = fs::read(fixture_directory().join("two_turn_call.sse")).expect("fixture");
    assert_eq!(replay_gemini_sse(&bytes), replay_gemini_sse(&bytes));
    assert!(replay_gemini_sse(&bytes).iter().any(|item| matches!(
        item,
        Ok(StreamEvent::ToolCallStart { call_id, name })
            if call_id == "gemini-call-0000000000000000" && name == "lookup_weather"
    )));
}

#[test]
fn gemini_opaque_roundtrips_and_foreign_provider_opaque_is_rejected() {
    let signed = serde_json::json!({
        "kind": "signed_part",
        "call_id": "gemini-call-0000000000000000",
        "part": {
            "functionCall": {"name":"lookup_weather","args":{"city":"Tehran"}},
            "thoughtSignature":"opaque-roundtrip-signature"
        }
    });
    let request = TurnRequest {
        messages: vec![Message::assistant(vec![
            Block::ProviderOpaque {
                provider: "gemini".into(),
                data: signed.clone(),
            },
            Block::ToolCall {
                call_id: "gemini-call-0000000000000000".into(),
                name: "lookup_weather".into(),
                args: serde_json::json!({"city":"Tehran"}),
            },
        ])],
        model: "gemini-2.5-flash".into(),
        max_tokens: 32,
        system_prompt: None,
        tools: vec![weather_tool()],
        attachments: Vec::new(),
    };
    let payload = provider("gemini-2.5-flash")
        .request_payload(&request)
        .expect("same-family opaque replays");
    assert_eq!(payload["contents"][0]["parts"][0], signed["part"]);
    assert_eq!(
        payload["contents"][0]["parts"].as_array().map(Vec::len),
        Some(1)
    );

    let mut foreign = request;
    foreign.messages[0].blocks[0] = Block::ProviderOpaque {
        provider: "openai".into(),
        data: serde_json::json!({"type":"reasoning"}),
    };
    let error = provider("gemini-2.5-flash")
        .request_payload(&foreign)
        .expect_err("foreign opaque must be rejected");
    assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
}

#[test]
fn missing_or_ambiguous_call_id_to_name_mapping_is_rejected_locally() {
    let missing = TurnRequest {
        messages: vec![Message::tool_result("missing-call", "sunny", false)],
        model: "gemini-2.5-flash".into(),
        max_tokens: 32,
        system_prompt: None,
        tools: Vec::new(),
        attachments: Vec::new(),
    };
    let error = provider("gemini-2.5-flash")
        .request_payload(&missing)
        .expect_err("unknown result id is not guessed");
    assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);

    let ambiguous = TurnRequest {
        messages: vec![Message::assistant(vec![
            Block::ToolCall {
                call_id: "same".into(),
                name: "first".into(),
                args: serde_json::json!({}),
            },
            Block::ToolCall {
                call_id: "same".into(),
                name: "second".into(),
                args: serde_json::json!({}),
            },
        ])],
        ..missing
    };
    let error = provider("gemini-2.5-flash")
        .request_payload(&ambiguous)
        .expect_err("one call id cannot name two functions");
    assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
}

#[test]
fn request_payload_maps_system_tools_results_and_inline_images() {
    let image = ArtifactRef::new("blake3:gemini-image");
    let request = TurnRequest {
        messages: vec![
            Message {
                role: MessageRole::User,
                blocks: vec![
                    Block::Attachment(AttachmentBlock::Image {
                        artifact: image.clone(),
                        mime: "image/png".into(),
                        width: Some(1),
                        height: Some(1),
                    }),
                    Block::Text {
                        text: "What is shown?".into(),
                    },
                ],
            },
            Message::assistant(vec![Block::ToolCall {
                call_id: "gemini-call-0000000000000000".into(),
                name: "lookup_weather".into(),
                args: serde_json::json!({"city":"Tehran"}),
            }]),
            Message::tool_result("gemini-call-0000000000000000", "sunny", true),
        ],
        model: "gemini-2.5-flash".into(),
        max_tokens: 512,
        system_prompt: Some("Be concise.".into()),
        tools: vec![weather_tool()],
        attachments: vec![ResolvedAttachment {
            artifact: image,
            data_base64: "iVBORw0KGgo=".into(),
        }],
    };
    let payload = provider("gemini-2.5-flash")
        .request_payload(&request)
        .expect("Gemini payload");
    assert_eq!(
        payload["system_instruction"]["parts"][0]["text"],
        "Be concise."
    );
    assert_eq!(payload["generationConfig"]["maxOutputTokens"], 512);
    assert_eq!(
        payload["contents"][0]["parts"][0]["inlineData"]["mimeType"],
        "image/png"
    );
    assert_eq!(payload["contents"][1]["role"], "model");
    assert_eq!(payload["contents"][2]["role"], "user");
    assert_eq!(
        payload["contents"][2]["parts"][0]["functionResponse"]["name"],
        "lookup_weather"
    );
    assert_eq!(
        payload["tools"][0]["functionDeclarations"][0]["parameters"]["required"][0],
        "city"
    );
}

#[test]
fn http_errors_classify_typed_without_leaking_bodies() {
    let secret = "GEMINI_BODY_SECRET_SENTINEL_8d21";
    let cases = [
        (401, "UNAUTHENTICATED", ProviderErrorKind::Authentication),
        (
            403,
            "PERMISSION_DENIED",
            ProviderErrorKind::PermissionDenied,
        ),
        (503, "UNAVAILABLE", ProviderErrorKind::Overloaded),
    ];
    for (status, wire_status, expected) in cases {
        let body = serde_json::to_vec(&serde_json::json!({
            "error": {"status": wire_status, "message": secret}
        }))
        .expect("error body");
        let error = replay_gemini_http_error(status, None, &body);
        assert_eq!(error.kind, expected);
        assert!(!error.message.contains(secret));
    }

    let rate = replay_gemini_http_error(
        429,
        Some("99"),
        include_bytes!("fixtures/gemini/rate_limit.http.json"),
    );
    assert_eq!(rate.kind, ProviderErrorKind::RateLimited);
    assert_eq!(
        rate.retry_after_ms,
        Some(2_250),
        "RetryInfo wins over header"
    );
    let context = replay_gemini_http_error(
        400,
        None,
        include_bytes!("fixtures/gemini/context_exceeded.http.json"),
    );
    assert_eq!(context.kind, ProviderErrorKind::ContextExceeded);
}

#[tokio::test]
async fn capability_backstop_is_native_where_the_api_is_native() {
    let current = provider("gemini-2.5-flash").capabilities().await;
    assert_eq!(current.provider, "gemini");
    assert_eq!(current.parallel_tools, FeatureResolve::Native);
    assert_eq!(
        current.streaming_tool_args,
        FeatureResolve::ExplicitlyEmulated
    );
    assert_eq!(current.vision, FeatureResolve::Native);
    assert_eq!(current.thinking_visible, FeatureResolve::Native);
    assert_eq!(current.context_limit, 1_048_576);

    let unknown = provider("gemini-future-unknown").capabilities().await;
    assert_eq!(unknown.context_limit, 128_000);
    assert_eq!(unknown.thinking_visible, FeatureResolve::Unsupported);
}

#[test]
fn provider_debug_never_exposes_resolved_secret() {
    let secret = "never-log-gemini-key";
    let vault = MemoryVault::new();
    let alias = CredentialAlias::new("gemini-debug");
    vault.put(&alias, secret.as_bytes()).expect("stores key");
    let provider = GeminiProvider::new(
        vault.resolve(&alias).expect("resolves key"),
        "gemini-2.5-flash",
    )
    .expect("provider")
    .with_account(alias);
    let debug = format!("{provider:?}");
    assert!(!debug.contains(secret));
    assert!(debug.contains("[REDACTED]"));
}

#[test]
fn constructor_policy_and_fixture_eof_are_stable() {
    let config = GeminiProvider::transport_config();
    assert_eq!(config.retry_policy, GeminiRetryPolicy::Never);
    assert_eq!(config.response_open_timeout, Duration::from_secs(30));
    assert!(matches!(
        replay_gemini_sse(include_bytes!("fixtures/gemini/max_tokens.sse")).last(),
        Some(Ok(StreamEvent::Finish {
            reason: FinishReason::MaxTokens
        }))
    ));
}

fn weather_tool() -> ToolDefinition {
    ToolDefinition {
        name: "lookup_weather".into(),
        description: "Look up weather".into(),
        input_schema: serde_json::json!({
            "type":"object",
            "properties":{"city":{"type":"string"}},
            "required":["city"]
        }),
    }
}

fn provider(model: &str) -> GeminiProvider {
    let vault = MemoryVault::new();
    let alias = CredentialAlias::new("gemini-fixture");
    vault
        .put(&alias, b"fixture-secret")
        .expect("stores fixture secret");
    GeminiProvider::new(
        vault.resolve(&alias).expect("resolves fixture secret"),
        model,
    )
    .expect("constructs provider")
    .with_account(alias)
}

fn fixture_directory() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE_DIR)
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> T {
    serde_json::from_slice(&fs::read(path).expect("reads JSON fixture"))
        .expect("parses JSON fixture")
}
