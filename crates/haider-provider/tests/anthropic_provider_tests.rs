#![allow(clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use haider_accounts::{CredentialAlias, MemoryVault, Vault};
use haider_protocol::ids::ArtifactRef;
use haider_protocol::provider::{Block, FeatureResolve, FinishReason, StreamEvent};
use haider_protocol::tool::AttachmentBlock;
use haider_provider::{
    AnthropicProvider, AnthropicRetryPolicy, Message, MessageRole, Provider, ProviderError,
    ProviderErrorKind, ProviderStreamItem, ResolvedAttachment, ToolDefinition, TurnRequest,
    replay_anthropic_http_error, replay_anthropic_sse,
};
use serde::Deserialize;

const FIXTURE_DIR: &str = "tests/fixtures/anthropic";

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
fn manifest_replays_every_declared_wire_fixture_in_either_promotion_state() {
    let directory = fixture_directory();
    let manifest_bytes = fs::read(directory.join("manifest.json")).expect("manifest bytes");
    let raw_manifest: serde_json::Value =
        serde_json::from_slice(&manifest_bytes).expect("manifest JSON");
    assert!(
        matches!(
            raw_manifest.get("provisional"),
            Some(serde_json::Value::Bool(_))
        ),
        "manifest must declare a boolean provisional flag"
    );
    let manifest: Manifest = serde_json::from_slice(&manifest_bytes).expect("typed manifest");
    assert_eq!(manifest.schema, "haider.anthropic-fixtures.v1");
    assert_eq!(
        raw_manifest
            .get("provisional")
            .and_then(serde_json::Value::as_bool),
        Some(manifest.provisional)
    );
    assert!(!manifest.provenance.trim().is_empty());
    assert!(!manifest.fixtures.is_empty());

    for fixture in manifest.fixtures {
        let wire = fs::read(directory.join(&fixture.wire)).expect("fixture wire bytes");
        match fixture.transport.as_str() {
            "sse" => {
                let expected: Vec<ExpectedItem> = read_json(&directory.join(&fixture.golden));
                let expected = expected
                    .into_iter()
                    .map(ExpectedItem::into_result)
                    .collect::<Vec<_>>();
                assert_eq!(
                    replay_anthropic_sse(&wire),
                    expected,
                    "fixture `{}`",
                    fixture.name
                );
            }
            "http" => {
                let expected: ProviderError = read_json(&directory.join(&fixture.golden));
                assert_eq!(
                    replay_anthropic_http_error(
                        fixture.status,
                        fixture.retry_after.as_deref(),
                        &wire,
                    ),
                    expected,
                    "fixture `{}`",
                    fixture.name
                );
            }
            other => panic!("unknown fixture transport `{other}`"),
        }
    }
}

#[test]
fn constructor_transport_config_disables_retries_and_bounds_connects_and_chunk_idle() {
    let config = AnthropicProvider::transport_config();

    assert_eq!(config.retry_policy, AnthropicRetryPolicy::Never);
    assert_eq!(config.connect_timeout, Duration::from_secs(10));
    assert_eq!(config.chunk_idle_timeout, Duration::from_secs(90));
}

#[test]
fn thinking_start_content_is_buffered_and_only_deltas_are_emitted() {
    for start_content in ["", "must not be emitted from start"] {
        let bytes = format!(
            "event: message_start\n\
             data: {{\"type\":\"message_start\",\"message\":{{\"content\":[],\"usage\":{{\"input_tokens\":1}}}}}}\n\n\
             event: content_block_start\n\
             data: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"thinking\",\"thinking\":{start_content:?},\"signature\":\"\"}}}}\n\n\
             event: content_block_delta\n\
             data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"thinking_delta\",\"thinking\":\"delta only\"}}}}\n\n\
             event: content_block_stop\n\
             data: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n\
             event: message_delta\n\
             data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}}}}\n\n\
             event: message_stop\n\
             data: {{\"type\":\"message_stop\"}}\n"
        );

        assert_eq!(
            replay_anthropic_sse(bytes.as_bytes()),
            vec![
                Ok(StreamEvent::ReasoningDelta {
                    text: "delta only".into(),
                }),
                Ok(StreamEvent::Finish {
                    reason: FinishReason::EndTurn,
                }),
            ],
            "thinking start content must not cross the provider boundary"
        );
    }
}

#[test]
fn request_payload_maps_system_tools_tool_results_and_a2_images() {
    let provider = provider("claude-sonnet-5");
    let image = ArtifactRef::new("blake3:image");
    let request = TurnRequest {
        model: "claude-sonnet-5".into(),
        max_tokens: 512,
        system_prompt: Some("Be concise.".into()),
        tools: vec![ToolDefinition {
            name: "weather".into(),
            description: "Read weather".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {"city": {"type": "string"}},
                "required": ["city"]
            }),
        }],
        attachments: vec![ResolvedAttachment {
            artifact: image.clone(),
            data_base64: "iVBORw0KGgo=".into(),
        }],
        messages: vec![
            Message {
                role: MessageRole::User,
                blocks: vec![
                    Block::Attachment(AttachmentBlock::Image {
                        artifact: image,
                        mime: "image/png".into(),
                        width: Some(1),
                        height: Some(1),
                    }),
                    Block::Text {
                        text: "What is shown?".into(),
                    },
                ],
            },
            Message {
                role: MessageRole::Assistant,
                blocks: vec![Block::ToolCall {
                    call_id: "toolu_sanitized".into(),
                    name: "weather".into(),
                    args: serde_json::json!({"city": "Tehran"}),
                }],
            },
            Message {
                role: MessageRole::Tool,
                blocks: vec![Block::ToolResult {
                    call_id: "toolu_sanitized".into(),
                    preview: "sunny".into(),
                    truncated: false,
                }],
            },
        ],
    };

    let payload = provider
        .request_payload(&request)
        .expect("request maps to Anthropic JSON");

    assert_eq!(payload["model"], "claude-sonnet-5");
    assert_eq!(payload["max_tokens"], 512);
    assert_eq!(payload["stream"], true);
    assert_eq!(payload["system"], "Be concise.");
    assert_eq!(payload["tools"][0]["name"], "weather");
    assert_eq!(
        payload["messages"][0]["content"][0],
        serde_json::json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": "image/png",
                "data": "iVBORw0KGgo="
            }
        })
    );
    assert_eq!(payload["messages"][1]["content"][0]["type"], "tool_use");
    assert_eq!(payload["messages"][2]["role"], "user");
    assert_eq!(payload["messages"][2]["content"][0]["type"], "tool_result");
}

#[test]
fn missing_image_data_is_a_typed_invalid_request() {
    let provider = provider("claude-sonnet-5");
    let request = TurnRequest {
        model: "claude-sonnet-5".into(),
        max_tokens: 128,
        system_prompt: None,
        tools: Vec::new(),
        attachments: Vec::new(),
        messages: vec![Message {
            role: MessageRole::User,
            blocks: vec![Block::Attachment(AttachmentBlock::Image {
                artifact: ArtifactRef::new("blake3:missing"),
                mime: "image/png".into(),
                width: None,
                height: None,
            })],
        }],
    };

    let error = provider
        .request_payload(&request)
        .expect_err("unresolved image is rejected");

    assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
    assert!(!error.retryable);
}

/// MUTATION CHECK: map `model_context_window_exceeded` to MaxTokens or leave
/// the HTTP body generic. Expected runtime failure: one of these assertions
/// observes continuation/generic rejection instead of forced compaction.
#[test]
fn context_exceeded_http_and_sse_fixtures_are_distinct_from_max_tokens() {
    let http = replay_anthropic_http_error(
        400,
        None,
        include_bytes!("fixtures/anthropic/context_exceeded.http.json"),
    );
    assert_eq!(http.kind, ProviderErrorKind::ContextExceeded);
    assert!(!http.retryable);

    let items = replay_anthropic_sse(include_bytes!("fixtures/anthropic/context_exceeded.sse"));
    assert!(items.iter().any(|item| {
        matches!(item, Err(error) if error.kind == ProviderErrorKind::ContextExceeded)
    }));
    assert!(!items.iter().any(|item| {
        matches!(
            item,
            Ok(StreamEvent::Finish {
                reason: FinishReason::MaxTokens
            })
        )
    }));
}

#[tokio::test]
async fn capability_table_is_model_specific_and_conservative_for_unknown_ids() {
    let cases = [
        ("claude-fable-5", 1_000_000, FeatureResolve::Native),
        ("claude-opus-5", 1_000_000, FeatureResolve::Native),
        ("claude-sonnet-5", 1_000_000, FeatureResolve::Native),
        ("claude-haiku-4-5-20251001", 200_000, FeatureResolve::Native),
        (
            "claude-future-unknown",
            100_000,
            FeatureResolve::Unsupported,
        ),
    ];

    for (model, context_limit, thinking_visible) in cases {
        let capabilities = provider(model).capabilities().await;
        assert_eq!(capabilities.provider, "anthropic");
        assert_eq!(capabilities.parallel_tools, FeatureResolve::Native);
        assert_eq!(capabilities.streaming_tool_args, FeatureResolve::Native);
        assert_eq!(capabilities.vision, FeatureResolve::Native);
        assert_eq!(capabilities.context_limit, context_limit);
        assert_eq!(capabilities.thinking_visible, thinking_visible);
    }
}

#[test]
fn http_auth_permission_overload_and_server_failures_are_classified() {
    let body = |kind: &str| {
        serde_json::to_vec(&serde_json::json!({
            "type": "error",
            "error": {"type": kind, "message": "sanitized"}
        }))
        .expect("error fixture serializes")
    };
    let cases = [
        (
            401,
            "authentication_error",
            ProviderErrorKind::Authentication,
            false,
        ),
        (
            403,
            "permission_error",
            ProviderErrorKind::PermissionDenied,
            false,
        ),
        (529, "overloaded_error", ProviderErrorKind::Overloaded, true),
        (500, "api_error", ProviderErrorKind::Transport, true),
    ];

    for (status, wire_kind, expected_kind, retryable) in cases {
        let error = replay_anthropic_http_error(status, None, &body(wire_kind));
        assert_eq!(error.kind, expected_kind);
        assert_eq!(error.retryable, retryable);
        assert_eq!(error.retry_after_ms, None);
    }
}

#[test]
fn provider_debug_never_exposes_resolved_secret() {
    let secret = "never-log-anthropic-key";
    let vault = MemoryVault::new();
    let alias = CredentialAlias::new("anthropic-test");
    vault
        .put(&alias, secret.as_bytes())
        .expect("stores test secret");
    let handle = vault.resolve(&alias).expect("resolves test secret");
    let provider = AnthropicProvider::new(handle, "claude-sonnet-5")
        .expect("HTTP client constructs")
        .with_account(alias);

    let debug = format!("{provider:?}");

    assert!(!debug.contains(secret));
    assert!(debug.contains("[REDACTED]"));
}

#[test]
fn event_name_and_data_type_mismatch_is_malformed() {
    let bytes = b"event: ping\ndata: {\"type\":\"message_stop\"}\n\n";
    let items = replay_anthropic_sse(bytes);

    let error = items
        .into_iter()
        .next()
        .expect("one stream item")
        .expect_err("mismatch fails");
    assert_eq!(error.kind, ProviderErrorKind::MalformedFrame);
}

#[test]
fn unknown_future_sse_events_are_ignored_without_weakening_known_validation() {
    let bytes = br#"event: future_notice
data: {"type":"future_notice","payload":{"new":true}}

event: message_start
data: {"type":"message_start","message":{"content":[],"usage":{"input_tokens":1}}}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn"}}

event: message_stop
data: {"type":"message_stop"}

"#;

    assert_eq!(
        replay_anthropic_sse(bytes),
        vec![Ok(StreamEvent::Finish {
            reason: haider_protocol::provider::FinishReason::EndTurn,
        })]
    );
}

fn provider(model: &str) -> AnthropicProvider {
    let vault = MemoryVault::new();
    let alias = CredentialAlias::new("anthropic-fixture");
    vault
        .put(&alias, b"fixture-secret")
        .expect("stores fixture secret");
    let handle = vault.resolve(&alias).expect("resolves fixture secret");
    AnthropicProvider::new(handle, model)
        .expect("HTTP client constructs")
        .with_account(alias)
}

fn fixture_directory() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE_DIR)
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> T {
    let bytes = fs::read(path).expect("reads JSON fixture");
    serde_json::from_slice(&bytes).expect("parses JSON fixture")
}

/// LAW (LT1, thinking capture): a scripted stream with a SIGNED thinking
/// block — signature split across two signature_delta frames — and a
/// redacted_thinking block yields provider-opaque facts carrying the EXACT
/// wire payloads, emitted at each block's stop and therefore BEFORE the
/// tool_use events that follow; an UNSIGNED thinking block captures nothing.
///
/// MUTATION CHECK (executed — see the G3 mutation notes): drop the
/// signature accumulation (keep only the last fragment). Expected runtime
/// failure: the concatenated-signature assertion below.
#[test]
fn signed_thinking_and_redacted_blocks_are_captured_for_replay() {
    let bytes = "event: message_start\n\
         data: {\"type\":\"message_start\",\"message\":{\"content\":[],\"usage\":{\"input_tokens\":1}}}\n\n\
         event: content_block_start\n\
         data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\"}}\n\n\
         event: content_block_delta\n\
         data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"weigh the options\"}}\n\n\
         event: content_block_delta\n\
         data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig-first-\"}}\n\n\
         event: content_block_delta\n\
         data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig-second\"}}\n\n\
         event: content_block_stop\n\
         data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
         event: content_block_start\n\
         data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"redacted_thinking\",\"data\":\"ENCRYPTED_PAYLOAD_b64\"}}\n\n\
         event: content_block_stop\n\
         data: {\"type\":\"content_block_stop\",\"index\":1}\n\n\
         event: content_block_start\n\
         data: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_capture\",\"name\":\"fs_read\",\"input\":{}}}\n\n\
         event: content_block_delta\n\
         data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{}\"}}\n\n\
         event: content_block_stop\n\
         data: {\"type\":\"content_block_stop\",\"index\":2}\n\n\
         event: message_delta\n\
         data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\n\
         event: message_stop\n\
         data: {\"type\":\"message_stop\"}\n";

    let items = replay_anthropic_sse(bytes.as_bytes());
    assert_eq!(
        items,
        vec![
            Ok(StreamEvent::ReasoningDelta {
                text: "weigh the options".into(),
            }),
            Ok(StreamEvent::ProviderOpaque {
                provider: "anthropic".into(),
                data: serde_json::json!({
                    "type": "thinking",
                    "thinking": "weigh the options",
                    "signature": "sig-first-sig-second",
                }),
            }),
            Ok(StreamEvent::ProviderOpaque {
                provider: "anthropic".into(),
                data: serde_json::json!({
                    "type": "redacted_thinking",
                    "data": "ENCRYPTED_PAYLOAD_b64",
                }),
            }),
            Ok(StreamEvent::ToolCallStart {
                call_id: "toolu_capture".into(),
                name: "fs_read".into(),
            }),
            Ok(StreamEvent::ToolCallArgsDelta {
                call_id: "toolu_capture".into(),
                args_fragment: "{}".into(),
            }),
            Ok(StreamEvent::ToolCallEnd {
                call_id: "toolu_capture".into(),
            }),
            Ok(StreamEvent::Finish {
                reason: FinishReason::ToolUse,
            }),
        ],
        "signed thinking + redacted blocks capture verbatim, BEFORE tool events"
    );

    // An unsigned thinking block (display-only) captures NOTHING.
    let unsigned = "event: message_start\n\
         data: {\"type\":\"message_start\",\"message\":{\"content\":[],\"usage\":{\"input_tokens\":1}}}\n\n\
         event: content_block_start\n\
         data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\"}}\n\n\
         event: content_block_delta\n\
         data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"loose thought\"}}\n\n\
         event: content_block_stop\n\
         data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
         event: message_delta\n\
         data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n\
         event: message_stop\n\
         data: {\"type\":\"message_stop\"}\n";
    assert_eq!(
        replay_anthropic_sse(unsigned.as_bytes()),
        vec![
            Ok(StreamEvent::ReasoningDelta {
                text: "loose thought".into(),
            }),
            Ok(StreamEvent::Finish {
                reason: FinishReason::EndTurn,
            }),
        ],
        "an unsigned thinking block must not fabricate a replay fact"
    );
}

/// LAW (LT2, thinking replay): captured anthropic thinking/redacted_thinking
/// facts replay VERBATIM, in original order, BEFORE the tool_use block in
/// the assistant message of the follow-up request — and normalized
/// `Block::Reasoning` display summaries stay REJECTED (they carry no
/// provider-valid signature; replaying them as thinking blocks is a live
/// 400, which is exactly why the fix rides the provider-opaque channel).
///
/// MUTATION CHECK (executed — see the G3 mutation notes): filter
/// provider-opaque blocks whose payload `type` is "thinking" out of the
/// anthropic content mapping (the classic redacted-thinking bug, inverted).
/// Expected runtime failure: the verbatim-order assertion below.
#[test]
fn thinking_facts_replay_verbatim_in_order_and_normalized_reasoning_stays_rejected() {
    let thinking = serde_json::json!({
        "type": "thinking",
        "thinking": "weigh the options",
        "signature": "sig-first-sig-second",
    });
    let redacted = serde_json::json!({
        "type": "redacted_thinking",
        "data": "ENCRYPTED_PAYLOAD_b64",
    });
    let request = TurnRequest {
        model: "claude-fable-5".into(),
        max_tokens: 512,
        system_prompt: None,
        tools: Vec::new(),
        attachments: Vec::new(),
        messages: vec![
            Message::user_text("read the file"),
            Message {
                role: MessageRole::Assistant,
                blocks: vec![
                    Block::ProviderOpaque {
                        provider: "anthropic".into(),
                        data: thinking.clone(),
                    },
                    Block::ProviderOpaque {
                        provider: "anthropic".into(),
                        data: redacted.clone(),
                    },
                    Block::ToolCall {
                        call_id: "toolu_capture".into(),
                        name: "fs_read".into(),
                        args: serde_json::json!({"path": "/tmp/x"}),
                    },
                ],
            },
            Message::tool_result("toolu_capture", "file contents", false),
        ],
    };
    let payload = provider("claude-fable-5")
        .request_payload(&request)
        .expect("tool-loop payload with thinking facts");
    let content = payload["messages"][1]["content"]
        .as_array()
        .expect("assistant content");
    assert_eq!(
        content[0], thinking,
        "the signed thinking block replays VERBATIM first"
    );
    assert_eq!(
        content[1], redacted,
        "redacted_thinking replays verbatim second — dropping it is the classic 400"
    );
    assert_eq!(content[2]["type"], "tool_use");
    assert_eq!(content[2]["id"], "toolu_capture");
    assert_eq!(content.len(), 3);

    // Normalized display reasoning stays rejected.
    let mut normalized = request;
    normalized.messages[1].blocks[0] = Block::Reasoning {
        summary: "a display summary".into(),
    };
    let error = provider("claude-fable-5")
        .request_payload(&normalized)
        .expect_err("normalized reasoning must stay rejected");
    assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
}
