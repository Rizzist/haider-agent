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

fn reanchor_events(path: &Path, actual: &[ProviderStreamItem]) {
    if std::env::var_os("UPDATE_FIXTURES").is_none() {
        return;
    }
    let tagged = actual
        .iter()
        .map(|item| match item {
            Ok(event) => serde_json::json!({"result": "ok", "value": event}),
            Err(error) => serde_json::json!({"result": "err", "value": error}),
        })
        .collect::<Vec<_>>();
    fs::write(
        path,
        serde_json::to_string_pretty(&tagged).expect("serialize event golden"),
    )
    .expect("write event golden");
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
                let actual = replay_gemini_sse(&wire);
                reanchor_events(&directory.join(&fixture.golden), &actual);
                let expected: Vec<ExpectedItem> = read_json(&directory.join(&fixture.golden));
                let expected = expected
                    .into_iter()
                    .map(ExpectedItem::into_result)
                    .collect::<Vec<_>>();
                assert_eq!(actual, expected, "{}", fixture.name);
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

/// MUTATION CHECK: return `count` instead of `greatest + 1` from
/// `next_synthesized_call_index`. Expected RUNTIME failure: with a SPARSE
/// history (one prior call at index 5) the next synthesized id collides
/// into the dense range instead of continuing past the greatest replayed
/// index — the dense one-call fixture is degenerate (`count == greatest+1`)
/// and cannot see this.
#[test]
fn sparse_history_call_index_continues_past_the_greatest_not_the_count() {
    let bytes = fs::read(fixture_directory().join("two_turn_call.sse")).expect("fixture stream");
    let request = TurnRequest {
        messages: vec![
            Message::user_text("prior sparse turn"),
            Message::assistant(vec![Block::ToolCall {
                call_id: "gemini-call-0000000000000005".into(),
                name: "lookup_weather".into(),
                args: serde_json::json!({"city": "Tehran"}),
            }]),
            Message::tool_result("gemini-call-0000000000000005", "sunny", false),
        ],
        model: "gemini-2.5-flash".into(),
        max_tokens: 64,
        system_prompt: None,
        tools: vec![weather_tool()],
        attachments: Vec::new(),
    };
    let items = haider_provider::replay_gemini_sse_for_request(&request, &bytes)
        .expect("request-aware replay");
    assert!(
        items.iter().any(|item| matches!(
            item,
            Ok(StreamEvent::ToolCallStart { call_id, .. })
                if call_id == "gemini-call-0000000000000006"
        )),
        "next synthesized id must be greatest+1 (…0006), never the count"
    );
}

/// LAW (LE3, gemini half): the session effort rides
/// `generationConfig.thinkingConfig.thinkingLevel` for 3.x-named models
/// whose pinned static ladder declares the value; a 2.5-era model or an
/// out-of-ladder value injects NOTHING — never `thinkingBudget`, never both
/// fields (thinkingLevel + thinkingBudget together is a live 400).
///
/// MUTATION CHECK (executed — see the G3 mutation notes): drop the
/// name/ladder gate and inject the level for every model. Expected runtime
/// failure: the 2.5-era no-thinkingConfig assertion below.
#[test]
fn effort_injects_thinking_level_for_3x_models_only() {
    let request = |model: &str| TurnRequest {
        messages: vec![Message::user_text("hello")],
        model: model.into(),
        max_tokens: 64,
        system_prompt: None,
        tools: Vec::new(),
        attachments: Vec::new(),
    };

    let payload = provider("gemini-3-flash")
        .with_effort(Some("low".into()))
        .request_payload(&request("gemini-3-flash"))
        .expect("3.x payload with effort");
    assert_eq!(
        payload["generationConfig"],
        serde_json::json!({
            "maxOutputTokens": 64,
            "thinkingConfig": {"thinkingLevel": "low"},
        }),
        "3.x models take thinkingLevel beside the output cap: {payload}"
    );
    assert!(
        !payload["generationConfig"]
            .as_object()
            .expect("generationConfig")
            .contains_key("thinkingBudget"),
        "thinkingBudget must never appear: {payload}"
    );

    // 3.1-pro has no `minimal` in its ladder: the out-of-ladder value
    // injects nothing rather than a documented 400.
    let payload = provider("gemini-3.1-pro")
        .with_effort(Some("minimal".into()))
        .request_payload(&request("gemini-3.1-pro"))
        .expect("3.1-pro payload with out-of-ladder effort");
    assert_eq!(
        payload["generationConfig"],
        serde_json::json!({"maxOutputTokens": 64}),
        "an out-of-ladder value injects nothing: {payload}"
    );

    // 2.5-era models keep the exact pre-G3 generationConfig.
    let payload = provider("gemini-2.5-flash")
        .with_effort(Some("low".into()))
        .request_payload(&request("gemini-2.5-flash"))
        .expect("2.5 payload with effort");
    assert_eq!(
        payload["generationConfig"],
        serde_json::json!({"maxOutputTokens": 64}),
        "2.5-era models take no thinking field at all: {payload}"
    );
}

/// LAW (LW5, request half): the `google_search` + `url_context` built-ins
/// declare BESIDE function declarations on 3.x-named models — and only
/// there. A 2.5-era model keeps its exact pre-W-B tools array regardless of
/// the flag (built-ins cannot mix with function declarations there), and a
/// 3.x model without the flag declares no built-ins. Both directions of the
/// name gate are pinned.
#[test]
fn web_builtins_declare_on_3x_beside_function_declarations_and_never_on_25() {
    let request = |model: &str| TurnRequest {
        messages: vec![Message::user_text("search the web")],
        model: model.into(),
        max_tokens: 64,
        system_prompt: None,
        tools: vec![weather_tool()],
        attachments: Vec::new(),
    };

    let payload = provider("gemini-3-flash")
        .with_web_builtins(true)
        .request_payload(&request("gemini-3-flash"))
        .expect("3.x payload with builtins");
    let tools = payload["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 3, "declarations + both built-ins: {payload}");
    assert!(tools[0].get("functionDeclarations").is_some());
    assert_eq!(tools[1], serde_json::json!({"google_search": {}}));
    assert_eq!(tools[2], serde_json::json!({"url_context": {}}));

    let quarter = provider("gemini-2.5-flash")
        .with_web_builtins(true)
        .request_payload(&request("gemini-2.5-flash"))
        .expect("2.5 payload with the flag");
    let tools = quarter["tools"].as_array().expect("tools array");
    assert_eq!(
        tools.len(),
        1,
        "2.5-era models keep declarations only — built-ins cannot mix: {quarter}"
    );
    assert!(tools[0].get("functionDeclarations").is_some());

    let ungated = provider("gemini-3-flash")
        .request_payload(&request("gemini-3-flash"))
        .expect("3.x payload without the flag");
    assert_eq!(
        ungated["tools"].as_array().map(Vec::len),
        Some(1),
        "no built-ins without the flag: {ungated}"
    );

    // With no client tools at all, the built-ins still declare on 3.x.
    let mut bare = request("gemini-3-flash");
    bare.tools = Vec::new();
    let bare_payload = provider("gemini-3-flash")
        .with_web_builtins(true)
        .request_payload(&bare)
        .expect("bare 3.x payload with builtins");
    assert_eq!(
        bare_payload["tools"],
        serde_json::json!([{"google_search": {}}, {"url_context": {}}]),
        "built-ins declare alone when no function tools exist: {bare_payload}"
    );
}

/// LAW (LW5, decode half): groundingMetadata decodes TOLERANTLY into display
/// facts — executed webSearchQueries become closed `web_search` rows,
/// groundingChunks web sources and successfully retrieved url_context URLs
/// become sources (duplicates deduped, failed retrievals skipped) — and a
/// frame with no grounding decodes exactly as before.
#[test]
fn grounding_metadata_decodes_into_rows_and_sources_tolerantly() {
    let frame = serde_json::json!({
        "candidates": [{
            "content": {"parts": [{"text": "grounded answer"}]},
            "groundingMetadata": {
                "webSearchQueries": ["rust sse decoding"],
                "groundingChunks": [
                    {"web": {"uri": "https://example.com/a", "title": "A", "domain": "example.com"}},
                    {"web": {"uri": "https://example.com/a", "title": "A duplicate"}},
                    {"retrievedContext": {"note": "no web field — skipped, never fatal"}},
                ],
                "groundingSupports": [{"segment": {"startIndex": 0, "endIndex": 5}}],
                "searchEntryPoint": {"renderedContent": "<div>chip</div>"},
            },
            "url_context_metadata": {
                "url_metadata": [
                    {"retrieved_url": "https://example.com/doc", "url_retrieval_status": "URL_RETRIEVAL_STATUS_SUCCESS"},
                    {"retrieved_url": "https://example.com/broken", "url_retrieval_status": "URL_RETRIEVAL_STATUS_ERROR"},
                ],
            },
            "finishReason": "STOP",
        }],
    });
    let bytes = format!("data: {frame}\n\n");
    let items = haider_provider::replay_gemini_sse(bytes.as_bytes());
    assert_eq!(
        items,
        vec![
            Ok(StreamEvent::TextDelta {
                text: "grounded answer".into(),
            }),
            Ok(StreamEvent::ServerToolUse {
                call_id: "gemini-search-1".into(),
                name: "web_search".into(),
                args: serde_json::json!({"query": "rust sse decoding"}),
            }),
            Ok(StreamEvent::ServerToolResult {
                call_id: "gemini-search-1".into(),
                preview: "grounded".into(),
                is_error: false,
            }),
            Ok(StreamEvent::WebSources {
                sources: vec![
                    haider_protocol::provider::WebSource {
                        url: "https://example.com/a".into(),
                        title: Some("A".into()),
                    },
                    haider_protocol::provider::WebSource {
                        url: "https://example.com/doc".into(),
                        title: None,
                    },
                ],
            }),
            Ok(StreamEvent::Finish {
                reason: FinishReason::EndTurn,
            }),
        ],
        "grounding decodes into rows + deduped sources, failures skipped"
    );

    // Absent grounding decodes exactly as before.
    let plain = serde_json::json!({
        "candidates": [{
            "content": {"parts": [{"text": "plain"}]},
            "finishReason": "STOP",
        }],
    });
    let bytes = format!("data: {plain}\n\n");
    assert_eq!(
        haider_provider::replay_gemini_sse(bytes.as_bytes()),
        vec![
            Ok(StreamEvent::TextDelta {
                text: "plain".into(),
            }),
            Ok(StreamEvent::Finish {
                reason: FinishReason::EndTurn,
            }),
        ],
        "no grounding — no fabricated rows or sources"
    );
}
