#![allow(clippy::expect_used)]

#[path = "support/provider_manifest.rs"]
mod provider_manifest;
mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use haider_accounts::{CredentialAlias, MemoryVault, Vault};
use haider_protocol::ids::ArtifactRef;
use haider_protocol::provider::{Block, FeatureResolve, FinishReason, StreamEvent};
use haider_protocol::tool::{AttachmentBlock, ImageBlockRef, PdfDeliveryMode};
use haider_provider::{
    AnthropicProvider, AnthropicRetryPolicy, BUILTIN_PROVIDER_NAMES, Message, MessageRole,
    Provider, ProviderError, ProviderErrorKind, ResolvedAttachment, ToolDefinition, TurnRequest,
    degrade_tool_result_images_to_placeholders, pdf_document_capability,
    replay_anthropic_http_error, replay_anthropic_sse,
};

use provider_manifest::Manifest;
use support::{ExpectedItem, read_json, reanchor_events};

const FIXTURE_DIR: &str = "tests/fixtures/anthropic";

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
                let actual = replay_anthropic_sse(&wire);
                reanchor_events(&directory.join(&fixture.golden), &actual);
                let expected: Vec<ExpectedItem> = read_json(&directory.join(&fixture.golden));
                let expected = expected
                    .into_iter()
                    .map(ExpectedItem::into_result)
                    .collect::<Vec<_>>();
                assert_eq!(actual, expected, "fixture `{}`", fixture.name);
            }
            "http" => {
                let mut expected: ProviderError = read_json(&directory.join(&fixture.golden));
                let actual = replay_anthropic_http_error(
                    fixture.status,
                    fixture.retry_after.as_deref(),
                    &wire,
                );
                // Absolute reset time is intentionally wall-clock derived;
                // the golden pins every stable field and relative delay.
                expected.presentation.reset_at_ms = actual.presentation.reset_at_ms;
                assert_eq!(actual, expected, "fixture `{}`", fixture.name);
            }
            other => panic!("unknown fixture transport `{other}`"),
        }
    }
}

#[test]
fn explicit_hosted_web_tool_rejection_has_the_one_shot_fallback_discriminator() {
    let error = replay_anthropic_http_error(
        400,
        None,
        br#"{"type":"error","error":{"type":"invalid_request_error","message":"web_search is not enabled for this organization"}}"#,
    );
    assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
    assert_eq!(
        error.presentation.subcode.as_str(),
        "provider-web-tool-rejected"
    );

    let generic = replay_anthropic_http_error(
        400,
        None,
        br#"{"type":"error","error":{"type":"invalid_request_error","message":"max_tokens is invalid"}}"#,
    );
    assert_ne!(
        generic.presentation.subcode.as_str(),
        "provider-web-tool-rejected",
        "generic invalid requests must never trigger capability fallback"
    );
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
    let pdf = ArtifactRef::new("blake3:pdf");
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
        attachments: vec![
            ResolvedAttachment {
                artifact: image.clone(),
                data_base64: "iVBORw0KGgo=".into(),
            },
            ResolvedAttachment {
                artifact: pdf.clone(),
                data_base64: "JVBERi0xLjQK".into(),
            },
        ],
        cache_metadata: None,
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
                    Block::Attachment(AttachmentBlock::Pdf {
                        artifact: pdf,
                        name: "report.pdf".into(),
                        pages: 12,
                        delivery: PdfDeliveryMode::NativeDocument,
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
                    images: Vec::new(),
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
    assert_eq!(
        payload["messages"][0]["content"][1],
        serde_json::json!({
            "type": "document",
            "source": {
                "type": "base64",
                "media_type": "application/pdf",
                "data": "JVBERi0xLjQK"
            }
        })
    );
    assert_eq!(payload["messages"][1]["content"][0]["type"], "tool_use");
    assert_eq!(payload["messages"][2]["role"], "user");
    assert_eq!(payload["messages"][2]["content"][0]["type"], "tool_result");
}

#[test]
fn image_bearing_tool_result_uses_native_nested_content_or_named_placeholder() {
    let provider = provider("claude-sonnet-5");
    let artifact = ArtifactRef::new("blake3:tool-capture");
    let result = ImageBlockRef {
        artifact: artifact.clone(),
        media_type: "image/png".into(),
        width: 800,
        height: 600,
        byte_len: 12,
    };
    let mut request = TurnRequest {
        messages: vec![
            Message::assistant(vec![Block::ToolCall {
                call_id: "toolu_capture".into(),
                name: "capture".into(),
                args: serde_json::json!({}),
            }]),
            Message::tool_result_with_images("toolu_capture", "captured", false, vec![result]),
        ],
        model: "claude-sonnet-5".into(),
        max_tokens: 64,
        system_prompt: None,
        tools: Vec::new(),
        attachments: vec![ResolvedAttachment {
            artifact: artifact.clone(),
            data_base64: "iVBORw0KGgo=".into(),
        }],
        cache_metadata: None,
    };

    let payload = provider
        .request_payload(&request)
        .expect("native image result");
    let content = &payload["messages"][1]["content"][0]["content"];
    assert_eq!(
        content[0],
        serde_json::json!({"type": "text", "text": "captured"})
    );
    assert_eq!(content[1]["type"], "image");
    assert_eq!(content[1]["source"]["data"], "iVBORw0KGgo=");

    request.attachments.clear();
    assert!(
        provider.request_payload(&request).is_err(),
        "native missing resolution must fail closed"
    );
    degrade_tool_result_images_to_placeholders(&mut request.messages);
    let payload = provider
        .request_payload(&request)
        .expect("unsupported image degrades honestly");
    let placeholder = payload["messages"][1]["content"][0]["content"]
        .as_str()
        .expect("placeholder text");
    assert!(placeholder.contains(artifact.as_str()));
    assert!(placeholder.contains("unavailable"));
}

#[test]
fn provider_catalog_declares_the_pdf_capability_split() {
    for provider in BUILTIN_PROVIDER_NAMES {
        let expected = if matches!(
            provider,
            "anthropic" | "anthropic-oauth" | "bedrock" | "vertex"
        ) {
            FeatureResolve::Native
        } else {
            FeatureResolve::ExplicitlyEmulated
        };
        assert_eq!(
            pdf_document_capability(provider),
            expected,
            "PDF capability drifted for {provider}"
        );
    }
    assert_eq!(
        pdf_document_capability("custom-profile"),
        FeatureResolve::ExplicitlyEmulated,
        "unknown/custom providers must take the bounded extraction lane"
    );
}

#[test]
fn native_pdf_enforces_anthropics_complete_request_size_limit() {
    let provider = provider("claude-sonnet-5");
    let artifact = ArtifactRef::new("blake3:oversized-native-pdf");
    let request = TurnRequest {
        model: "claude-sonnet-5".into(),
        max_tokens: 128,
        system_prompt: None,
        tools: Vec::new(),
        attachments: vec![ResolvedAttachment {
            artifact: artifact.clone(),
            // Already-base64 data at the documented complete-request cap;
            // JSON framing necessarily puts the request over it.
            data_base64: "A".repeat(32 * 1024 * 1024),
        }],
        cache_metadata: None,
        messages: vec![Message {
            role: MessageRole::User,
            blocks: vec![Block::Attachment(AttachmentBlock::Pdf {
                artifact,
                name: "large.pdf".into(),
                pages: 1,
                delivery: PdfDeliveryMode::NativeDocument,
            })],
        }],
    };

    let error = provider
        .request_payload(&request)
        .expect_err("complete request cap");
    assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
    assert_eq!(
        error.presentation.subcode.as_str(),
        "pdf-provider-request-too-large"
    );
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
        cache_metadata: None,
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
fn e1c_anthropic_billing_exhaustion_is_non_retryable_quota() {
    let body = serde_json::to_vec(&serde_json::json!({
        "type": "error",
        "error": {
            "type": "billing_error",
            "message": "Credit balance is too low; update billing"
        }
    }))
    .expect("body");
    let error = replay_anthropic_http_error(429, Some("2"), &body);
    assert_eq!(error.kind, ProviderErrorKind::QuotaExhausted);
    assert!(!error.retryable);
    assert_eq!(error.retry_after_ms, None);
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
        cache_metadata: None,
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

/// LAW (LW1, anthropic request golden): with the web-tools flag the request
/// declares BOTH server tools with these exact shapes — basic
/// `web_search_20250305` (max_uses 8) and `web_fetch_20250910` with
/// citations disabled and the pinned content budget — appended after the
/// client tools; without the flag the tools array is byte-identical to the
/// pre-W-B shape (and absent entirely when no client tool is advertised).
#[test]
fn web_tools_declaration_is_exact_and_absent_without_the_flag() {
    let request = TurnRequest {
        model: "claude-fable-5".into(),
        max_tokens: 512,
        system_prompt: None,
        tools: vec![ToolDefinition {
            name: "fs_read".into(),
            description: "Read a UTF-8 file".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }],
        attachments: Vec::new(),
        cache_metadata: None,
        messages: vec![Message::user_text("search the web")],
    };

    let with_tools = provider("claude-fable-5")
        .with_web_tools(true)
        .request_payload(&request)
        .expect("web-tools payload");
    let tools = with_tools["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 3, "client tool + exactly two server tools");
    assert_eq!(tools[0]["name"], "fs_read");
    assert_eq!(
        tools[1],
        serde_json::json!({
            "type": "web_search_20250305",
            "name": "web_search",
            "max_uses": 8,
        }),
        "the basic 2025-03-05 search version, never the dynamic-filtering ones"
    );
    assert_eq!(
        tools[2],
        serde_json::json!({
            "type": "web_fetch_20250910",
            "name": "web_fetch",
            "citations": {"enabled": false},
            "max_content_tokens": 100000,
            "max_uses": 10,
        })
    );

    let without_flag = provider("claude-fable-5")
        .request_payload(&request)
        .expect("default payload");
    assert_eq!(
        without_flag["tools"].as_array().map(Vec::len),
        Some(1),
        "no server tools without the flag"
    );

    let mut bare = request;
    bare.tools = Vec::new();
    let bare_payload = provider("claude-fable-5")
        .request_payload(&bare)
        .expect("bare payload");
    assert!(
        bare_payload.get("tools").is_none(),
        "an empty tool set still omits the array entirely"
    );
    let bare_with_web = provider("claude-fable-5")
        .with_web_tools(true)
        .request_payload(&bare)
        .expect("bare web payload");
    assert_eq!(
        bare_with_web["tools"].as_array().map(Vec::len),
        Some(2),
        "server tools declare even with no client tools"
    );
}

/// LAW (LW2, opaque echo — capture half): a scripted stream with a
/// `server_tool_use` block (input streamed via split input_json_delta
/// frames), a `web_search_tool_result` carrying `encrypted_content` plus an
/// unknown sibling field, and a cited text block yields provider-opaque
/// facts carrying the EXACT wire payloads — unknown fields included — plus
/// display rows and the cited sources. The search call NEVER surfaces as a
/// client ToolCallStart (it must not enter the dispatch loop).
#[test]
fn server_tool_blocks_and_cited_text_are_captured_verbatim_for_replay() {
    let bytes = "event: message_start\n\
         data: {\"type\":\"message_start\",\"message\":{\"content\":[],\"usage\":{\"input_tokens\":3}}}\n\n\
         event: content_block_start\n\
         data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"server_tool_use\",\"id\":\"srvtoolu_1\",\"name\":\"web_search\",\"input\":{}}}\n\n\
         event: content_block_delta\n\
         data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"que\"}}\n\n\
         event: content_block_delta\n\
         data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"ry\\\":\\\"rust sse\\\"}\"}}\n\n\
         event: content_block_stop\n\
         data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
         event: content_block_start\n\
         data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"web_search_tool_result\",\"tool_use_id\":\"srvtoolu_1\",\"content\":[{\"type\":\"web_search_result\",\"url\":\"https://example.com/a\",\"title\":\"A\",\"encrypted_content\":\"ENC_A\",\"page_age\":\"1 day\"}],\"future_field\":\"kept\"}}\n\n\
         event: content_block_stop\n\
         data: {\"type\":\"content_block_stop\",\"index\":1}\n\n\
         event: content_block_start\n\
         data: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
         event: content_block_delta\n\
         data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"text_delta\",\"text\":\"cited \"}}\n\n\
         event: content_block_delta\n\
         data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"citations_delta\",\"citation\":{\"type\":\"web_search_result_location\",\"url\":\"https://example.com/a\",\"title\":\"A\",\"encrypted_index\":\"IDX_A\",\"cited_text\":\"quote\"}}}\n\n\
         event: content_block_delta\n\
         data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"text_delta\",\"text\":\"answer\"}}\n\n\
         event: content_block_stop\n\
         data: {\"type\":\"content_block_stop\",\"index\":2}\n\n\
         event: message_delta\n\
         data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n\
         event: message_stop\n\
         data: {\"type\":\"message_stop\"}\n";

    let items = replay_anthropic_sse(bytes.as_bytes());
    assert_eq!(
        items,
        vec![
            Ok(StreamEvent::ProviderOpaque {
                provider: "anthropic".into(),
                data: serde_json::json!({
                    "type": "server_tool_use",
                    "id": "srvtoolu_1",
                    "name": "web_search",
                    "input": {"query": "rust sse"},
                }),
            }),
            Ok(StreamEvent::ServerToolUse {
                call_id: "srvtoolu_1".into(),
                name: "web_search".into(),
                args: serde_json::json!({"query": "rust sse"}),
            }),
            Ok(StreamEvent::ProviderOpaque {
                provider: "anthropic".into(),
                data: serde_json::json!({
                    "type": "web_search_tool_result",
                    "tool_use_id": "srvtoolu_1",
                    "content": [{
                        "type": "web_search_result",
                        "url": "https://example.com/a",
                        "title": "A",
                        "encrypted_content": "ENC_A",
                        "page_age": "1 day",
                    }],
                    "future_field": "kept",
                }),
            }),
            Ok(StreamEvent::ServerToolResult {
                call_id: "srvtoolu_1".into(),
                preview: "1 result".into(),
                is_error: false,
            }),
            Ok(StreamEvent::TextDelta {
                text: "cited ".into(),
            }),
            Ok(StreamEvent::TextDelta {
                text: "answer".into(),
            }),
            Ok(StreamEvent::ProviderOpaque {
                provider: "anthropic".into(),
                data: serde_json::json!({
                    "type": "text",
                    "text": "cited answer",
                    "citations": [{
                        "type": "web_search_result_location",
                        "url": "https://example.com/a",
                        "title": "A",
                        "encrypted_index": "IDX_A",
                        "cited_text": "quote",
                    }],
                }),
            }),
            Ok(StreamEvent::WebSources {
                sources: vec![haider_protocol::provider::WebSource {
                    url: "https://example.com/a".into(),
                    title: Some("A".into()),
                }],
            }),
            Ok(StreamEvent::Finish {
                reason: FinishReason::EndTurn,
            }),
        ],
        "server tool blocks capture verbatim (unknown fields kept) and never open a client tool call"
    );
}

/// LAW (LW2, opaque echo — replay half): captured server_tool_use /
/// web_search_tool_result facts and the citation-signed text block replay
/// VERBATIM into the follow-up request, in order — and the normalized text
/// deltas that streamed the cited characters are CONSUMED so the text is
/// sent exactly once. A rehydrated cross-turn message carrying the signed
/// text alone passes through; a signed text that disagrees with normalized
/// history is refused.
#[test]
fn server_tool_facts_replay_verbatim_and_cited_text_dedups_normalized_history() {
    let server_use = serde_json::json!({
        "type": "server_tool_use",
        "id": "srvtoolu_1",
        "name": "web_search",
        "input": {"query": "rust sse"},
    });
    let server_result = serde_json::json!({
        "type": "web_search_tool_result",
        "tool_use_id": "srvtoolu_1",
        "content": [{
            "type": "web_search_result",
            "url": "https://example.com/a",
            "title": "A",
            "encrypted_content": "ENC_A",
            "page_age": "1 day",
        }],
        "future_field": "kept",
    });
    let signed_text = serde_json::json!({
        "type": "text",
        "text": "cited answer",
        "citations": [{
            "type": "web_search_result_location",
            "url": "https://example.com/a",
            "title": "A",
            "encrypted_index": "IDX_A",
            "cited_text": "quote",
        }],
    });
    let opaque = |data: &serde_json::Value| Block::ProviderOpaque {
        provider: "anthropic".into(),
        data: data.clone(),
    };
    let request = TurnRequest {
        model: "claude-fable-5".into(),
        max_tokens: 512,
        system_prompt: None,
        tools: Vec::new(),
        attachments: Vec::new(),
        cache_metadata: None,
        messages: vec![
            Message::user_text("search the web"),
            Message {
                role: MessageRole::Assistant,
                blocks: vec![
                    opaque(&server_use),
                    opaque(&server_result),
                    Block::Text {
                        text: "cited ".into(),
                    },
                    Block::Text {
                        text: "answer".into(),
                    },
                    opaque(&signed_text),
                ],
            },
            Message::user_text("thanks — continue"),
        ],
    };
    let payload = provider("claude-fable-5")
        .with_web_tools(true)
        .request_payload(&request)
        .expect("server-tool replay payload");
    let content = payload["messages"][1]["content"]
        .as_array()
        .expect("assistant content");
    assert_eq!(content[0], server_use, "server_tool_use replays VERBATIM");
    assert_eq!(
        content[1], server_result,
        "the result block — encrypted_content and unknown fields included — replays VERBATIM"
    );
    assert_eq!(
        content[2], signed_text,
        "the citation-signed text replays verbatim with every encrypted_index"
    );
    assert_eq!(
        content.len(),
        3,
        "the normalized text deltas were consumed — the cited text is sent exactly once"
    );

    // Rehydrated cross-turn shape: the signed text stands alone.
    let rehydrated = TurnRequest {
        model: "claude-fable-5".into(),
        max_tokens: 512,
        system_prompt: None,
        tools: Vec::new(),
        attachments: Vec::new(),
        cache_metadata: None,
        messages: vec![
            Message::user_text("search the web"),
            Message::assistant(vec![opaque(&signed_text)]),
            Message::user_text("next question"),
        ],
    };
    let payload = provider("claude-fable-5")
        .request_payload(&rehydrated)
        .expect("rehydrated payload");
    assert_eq!(
        payload["messages"][1]["content"]
            .as_array()
            .expect("content")
            .as_slice(),
        std::slice::from_ref(&signed_text),
        "a rehydrated signed text block passes through untouched"
    );

    // A signed text disagreeing with normalized history is refused.
    let disagreeing = TurnRequest {
        model: "claude-fable-5".into(),
        max_tokens: 512,
        system_prompt: None,
        tools: Vec::new(),
        attachments: Vec::new(),
        cache_metadata: None,
        messages: vec![
            Message::user_text("search the web"),
            Message::assistant(vec![
                Block::Text {
                    text: "different words".into(),
                },
                opaque(&signed_text),
            ]),
        ],
    };
    let error = provider("claude-fable-5")
        .request_payload(&disagreeing)
        .expect_err("disagreement is refused");
    assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
}

/// LAW (LW2, pause_turn): the `pause_turn` stop_reason normalizes to the
/// dedicated continuation finish — never a terminal EndTurn — so the turn
/// engine resends the paused assistant message unchanged.
#[test]
fn pause_turn_stop_reason_normalizes_to_a_continuation_finish() {
    let bytes = "event: message_start\n\
         data: {\"type\":\"message_start\",\"message\":{\"content\":[],\"usage\":{\"input_tokens\":1}}}\n\n\
         event: message_delta\n\
         data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"pause_turn\"}}\n\n\
         event: message_stop\n\
         data: {\"type\":\"message_stop\"}\n";
    assert_eq!(
        replay_anthropic_sse(bytes.as_bytes()),
        vec![Ok(StreamEvent::Finish {
            reason: FinishReason::PauseTurn,
        })]
    );
}

/// LAW (LW3): server tool result content decodes tolerantly on BOTH shapes —
/// the error form is an OBJECT carrying `error_code` (HTTP still 200), the
/// search success form is a LIST (empty = zero results, not an error), and
/// the fetch success form is a single object. Every shape still replays
/// verbatim through the opaque channel.
#[test]
fn search_error_object_and_success_list_decode_tolerantly() {
    let error_stream = "event: message_start\n\
         data: {\"type\":\"message_start\",\"message\":{\"content\":[],\"usage\":{\"input_tokens\":1}}}\n\n\
         event: content_block_start\n\
         data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"web_search_tool_result\",\"tool_use_id\":\"srvtoolu_9\",\"content\":{\"type\":\"web_search_tool_result_error\",\"error_code\":\"max_uses_exceeded\"}}}\n\n\
         event: content_block_stop\n\
         data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
         event: message_delta\n\
         data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n\
         event: message_stop\n\
         data: {\"type\":\"message_stop\"}\n";
    assert_eq!(
        replay_anthropic_sse(error_stream.as_bytes()),
        vec![
            Ok(StreamEvent::ProviderOpaque {
                provider: "anthropic".into(),
                data: serde_json::json!({
                    "type": "web_search_tool_result",
                    "tool_use_id": "srvtoolu_9",
                    "content": {
                        "type": "web_search_tool_result_error",
                        "error_code": "max_uses_exceeded",
                    },
                }),
            }),
            Ok(StreamEvent::ServerToolResult {
                call_id: "srvtoolu_9".into(),
                preview: "max_uses_exceeded".into(),
                is_error: true,
            }),
            Ok(StreamEvent::Finish {
                reason: FinishReason::EndTurn,
            }),
        ],
        "the error OBJECT decodes as a failed row and still replays verbatim"
    );

    let empty_stream = "event: message_start\n\
         data: {\"type\":\"message_start\",\"message\":{\"content\":[],\"usage\":{\"input_tokens\":1}}}\n\n\
         event: content_block_start\n\
         data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"web_search_tool_result\",\"tool_use_id\":\"srvtoolu_9\",\"content\":[]}}\n\n\
         event: content_block_stop\n\
         data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
         event: message_delta\n\
         data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n\
         event: message_stop\n\
         data: {\"type\":\"message_stop\"}\n";
    let items = replay_anthropic_sse(empty_stream.as_bytes());
    assert!(
        items.contains(&Ok(StreamEvent::ServerToolResult {
            call_id: "srvtoolu_9".into(),
            preview: "0 results".into(),
            is_error: false,
        })),
        "an EMPTY list is zero results, never an error: {items:?}"
    );

    let fetch_stream = "event: message_start\n\
         data: {\"type\":\"message_start\",\"message\":{\"content\":[],\"usage\":{\"input_tokens\":1}}}\n\n\
         event: content_block_start\n\
         data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"web_fetch_tool_result\",\"tool_use_id\":\"srvtoolu_f\",\"content\":{\"type\":\"web_fetch_result\",\"url\":\"https://example.com/doc\",\"content\":{\"type\":\"document\",\"source\":{\"type\":\"text\",\"media_type\":\"text/plain\",\"data\":\"body\"},\"title\":\"Doc\"},\"retrieved_at\":\"2026-08-06T00:00:00Z\"}}}\n\n\
         event: content_block_stop\n\
         data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
         event: message_delta\n\
         data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n\
         event: message_stop\n\
         data: {\"type\":\"message_stop\"}\n";
    let items = replay_anthropic_sse(fetch_stream.as_bytes());
    assert!(
        items.contains(&Ok(StreamEvent::ServerToolResult {
            call_id: "srvtoolu_f".into(),
            preview: "fetched https://example.com/doc".into(),
            is_error: false,
        })),
        "the fetch success OBJECT decodes as a completed row: {items:?}"
    );
    assert!(
        items.iter().any(|item| matches!(
            item,
            Ok(StreamEvent::ProviderOpaque { data, .. })
                if data["type"] == "web_fetch_tool_result"
                    && data["content"]["type"] == "web_fetch_result"
        )),
        "the fetch result replays verbatim through the opaque channel"
    );
}
