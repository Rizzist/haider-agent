#![allow(clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use haider_accounts::{CredentialAlias, MemoryVault, Vault};
use haider_protocol::provider::{Block, FeatureResolve, FinishReason, StreamEvent};
use haider_provider::{
    Message, MessageRole, OpenAiCompatibleProvider, OpenAiProvider, OpenAiRetryPolicy, Provider,
    ProviderError, ProviderErrorKind, ProviderStreamItem, ToolDefinition, TurnRequest,
    replay_openai_chat_sse, replay_openai_http_error, replay_openai_models_response,
    replay_openai_responses_sse,
};
use serde::Deserialize;

const FIXTURE_DIR: &str = "tests/fixtures/openai";

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
    family: String,
    wire: String,
    golden: String,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "result", content = "value", rename_all = "snake_case")]
enum ExpectedItem {
    Ok(StreamEvent),
    Err(haider_provider::ProviderError),
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
fn native_responses_fixture_maps_to_the_shared_stream_events() {
    let directory = fixture_directory();
    let wire = fs::read(directory.join("responses.sse")).expect("fixture wire");
    let expected: Vec<ExpectedItem> = read_json(&directory.join("responses.events.json"));
    let expected = expected
        .into_iter()
        .map(ExpectedItem::into_result)
        .collect::<Vec<_>>();

    assert_eq!(replay_openai_responses_sse(&wire), expected);
}

#[test]
fn compatible_chat_fixture_maps_to_the_same_shared_stream_events() {
    let directory = fixture_directory();
    let wire = fs::read(directory.join("chat.sse")).expect("fixture wire");
    let expected: Vec<ExpectedItem> = read_json(&directory.join("chat.events.json"));
    let expected = expected
        .into_iter()
        .map(ExpectedItem::into_result)
        .collect::<Vec<_>>();

    assert_eq!(replay_openai_chat_sse(&wire), expected);
}

#[test]
fn manifest_replays_every_declared_openai_fixture_with_provenance() {
    let directory = fixture_directory();
    let manifest_bytes = fs::read(directory.join("manifest.json")).expect("manifest bytes");
    let manifest: Manifest = serde_json::from_slice(&manifest_bytes).expect("manifest JSON");

    assert_eq!(manifest.schema, "haider.openai-fixtures.v1");
    assert!(manifest.provisional);
    assert!(!manifest.provenance.trim().is_empty());
    assert!(!manifest.fixtures.is_empty());

    for fixture in manifest.fixtures {
        let wire = fs::read(directory.join(&fixture.wire)).expect("fixture wire");
        let expected: Vec<ExpectedItem> = read_json(&directory.join(&fixture.golden));
        let expected = expected
            .into_iter()
            .map(ExpectedItem::into_result)
            .collect::<Vec<_>>();
        let actual = match fixture.family.as_str() {
            "responses" => replay_openai_responses_sse(&wire),
            "chat_completions" => replay_openai_chat_sse(&wire),
            other => panic!("unknown OpenAI fixture family `{other}`"),
        };
        assert_eq!(actual, expected, "fixture `{}`", fixture.name);
    }
}

#[test]
fn responses_max_tokens_drops_partial_tool_call_before_actor_sees_it() {
    let wire = br#"event: response.output_item.added
data: {"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_partial","call_id":"call_partial","name":"write_file","arguments":""}}

event: response.function_call_arguments.delta
data: {"type":"response.function_call_arguments.delta","item_id":"fc_partial","output_index":0,"delta":"{\"path\":"}

event: response.incomplete
data: {"type":"response.incomplete","response":{"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"usage":{"input_tokens":10,"output_tokens":2,"output_tokens_details":{"reasoning_tokens":0}}}}

"#;

    let items = replay_openai_responses_sse(wire);

    assert_no_tool_events(&items, "call_partial");
    assert!(matches!(
        items.last(),
        Some(Ok(StreamEvent::Finish {
            reason: FinishReason::MaxTokens
        }))
    ));
}

/// MUTATION CHECK: classify `context_length_exceeded` as InvalidRequest.
/// Expected runtime failure: forced compaction cannot distinguish overflow.
#[test]
fn context_exceeded_http_fixture_has_a_distinct_non_retryable_kind() {
    let error = replay_openai_http_error(
        400,
        None,
        include_bytes!("fixtures/openai/context_exceeded.http.json"),
    );

    assert_eq!(error.kind, ProviderErrorKind::ContextExceeded);
    assert!(!error.retryable);
}

#[test]
fn chat_max_tokens_drops_partial_tool_call_before_actor_sees_it() {
    let wire = br#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_partial","type":"function","function":{"name":"write_file","arguments":"{\"path\":"}}]},"finish_reason":null}]}

data: {"choices":[{"index":0,"delta":{},"finish_reason":"length"}]}

data: [DONE]

"#;

    let items = replay_openai_chat_sse(wire);

    assert_no_tool_events(&items, "call_partial");
    assert!(matches!(
        items.last(),
        Some(Ok(StreamEvent::Finish {
            reason: FinishReason::MaxTokens
        }))
    ));
}

#[test]
fn transport_config_disables_reqwest_retries_and_bounds_stream_waits() {
    let native = OpenAiProvider::transport_config();
    let compatible = OpenAiCompatibleProvider::transport_config();

    assert_eq!(native, compatible);
    assert_eq!(native.retry_policy, OpenAiRetryPolicy::Never);
    assert_eq!(native.connect_timeout, Duration::from_secs(10));
    assert_eq!(native.response_open_timeout, Duration::from_secs(30));
    assert_eq!(native.chunk_idle_timeout, Duration::from_secs(90));
}

#[test]
fn responses_payload_uses_native_input_tools_and_reasoning_summary() {
    let provider = native_provider("gpt-5-test");
    let request = TurnRequest {
        messages: vec![Message::user_text("What is the weather?")],
        model: "gpt-5-test".into(),
        max_tokens: 256,
        system_prompt: Some("Be concise.".into()),
        tools: vec![ToolDefinition {
            name: "get_weather".into(),
            description: "Get weather".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {"city": {"type": "string"}},
                "required": ["city"]
            }),
        }],
        attachments: Vec::new(),
    };

    let payload = provider
        .request_payload(&request)
        .expect("Responses payload");

    assert_eq!(payload["model"], "gpt-5-test");
    assert_eq!(payload["max_output_tokens"], 256);
    assert_eq!(payload["instructions"], "Be concise.");
    assert_eq!(payload["input"][0]["type"], "message");
    assert_eq!(payload["input"][0]["content"][0]["type"], "input_text");
    assert_eq!(payload["tools"][0]["type"], "function");
    assert_eq!(payload["tools"][0]["name"], "get_weather");
    assert_eq!(payload["reasoning"]["summary"], "auto");
    assert_eq!(payload["store"], false);
}

#[test]
fn compatible_payload_uses_chat_completions_lingua_franca() {
    let provider = compatible_provider("llama-test", "http://127.0.0.1:12345/v1");
    let request = TurnRequest {
        messages: vec![Message::user_text("Hello")],
        model: "llama-test".into(),
        max_tokens: 64,
        system_prompt: Some("Brief.".into()),
        tools: Vec::new(),
        attachments: Vec::new(),
    };

    let payload = provider.request_payload(&request).expect("Chat payload");

    assert_eq!(provider.base_url(), "http://127.0.0.1:12345/v1");
    assert_eq!(payload["max_tokens"], 64);
    assert_eq!(payload["messages"][0]["role"], "system");
    assert_eq!(payload["messages"][1]["role"], "user");
    assert_eq!(payload["stream_options"]["include_usage"], true);
}

#[test]
fn compatible_base_url_and_fake_v1_models_response_produce_a_capability_doc() {
    let provider = compatible_provider("gpt-oss-120b", "http://127.0.0.1:11434");
    let body = br#"{"object":"list","data":[{"id":"gpt-oss-120b","object":"model"}]}"#;

    let capabilities =
        replay_openai_models_response("gpt-oss-120b", body).expect("capability response");

    assert_eq!(provider.models_url(), "http://127.0.0.1:11434/v1/models");
    assert_eq!(capabilities.provider, "openai-compatible");
    assert_eq!(capabilities.parallel_tools, FeatureResolve::Unsupported);
    assert_eq!(
        capabilities.streaming_tool_args,
        FeatureResolve::Unsupported
    );
    assert_eq!(capabilities.thinking_visible, FeatureResolve::Unsupported);
    assert_eq!(capabilities.vision, FeatureResolve::Unsupported);
    assert_eq!(capabilities.context_limit, 0);
}

#[test]
fn compatible_origin_policy_rejects_credential_ssrf_and_accepts_safe_origins() {
    let rejected = [
        "http://169.254.169.254",
        "https://169.254.169.254",
        "http://10.0.0.8:8080",
        "https://10.0.0.8",
        "http://172.16.0.8",
        "https://172.31.255.254",
        "http://192.168.1.8",
        "https://192.168.1.8",
        "https://[fe80::1]",
        "https://[::ffff:169.254.169.254]",
        "https://[::ffff:10.0.0.8]",
        "http://203.0.113.7",
    ];
    for base_url in rejected {
        let error = compatible_provider_result("test-model", base_url)
            .expect_err("unsafe origin must be rejected before transport construction");
        assert_eq!(error.kind, ProviderErrorKind::InvalidRequest, "{base_url}");
        assert!(!error.retryable, "{base_url}");
        assert!(!error.message.contains("fixture-secret"), "{base_url}");
    }

    // Hostname safety is decided by the async resolve-validate-pin preflight,
    // covered with injected A/AAAA answers in the adapter's unit tests.
    for base_url in [
        "https://api.example.com/openai",
        "http://127.0.0.1:11434",
        "http://127.255.255.254:1234",
        "http://[::1]:1234",
    ] {
        compatible_provider_result("test-model", base_url)
            .unwrap_or_else(|error| panic!("safe origin `{base_url}` rejected: {error}"));
    }
}

#[test]
fn encrypted_reasoning_continuation_reconstructs_exact_next_responses_input() {
    let directory = fixture_directory();
    let wire = fs::read(directory.join("reasoning_continuation.sse")).expect("fixture wire");
    let items = replay_openai_responses_sse(&wire);
    let opaque = items
        .iter()
        .find_map(|item| match item {
            Ok(StreamEvent::ProviderOpaque { provider, data }) => {
                Some((provider.clone(), data.clone()))
            }
            _ => None,
        })
        .expect("encrypted reasoning continuation event");
    let provider = native_provider("gpt-5-test");
    let request = TurnRequest {
        messages: vec![
            Message::user_text("first"),
            Message {
                role: MessageRole::Assistant,
                blocks: vec![Block::ProviderOpaque {
                    provider: opaque.0,
                    data: opaque.1.clone(),
                }],
            },
            Message::user_text("continue"),
        ],
        model: "gpt-5-test".into(),
        max_tokens: 64,
        system_prompt: None,
        tools: Vec::new(),
        attachments: Vec::new(),
    };

    let payload = provider
        .request_payload(&request)
        .expect("opaque continuation replays");

    assert_eq!(
        payload["include"],
        serde_json::json!(["reasoning.encrypted_content"])
    );
    assert_eq!(payload["input"][1], opaque.1);
}

#[tokio::test]
async fn native_capability_doc_is_model_specific() {
    let reasoning = native_provider("gpt-5.6-test").capabilities().await;
    let classic = native_provider("gpt-4o-test").capabilities().await;

    assert_eq!(reasoning.provider, "openai");
    assert_eq!(reasoning.thinking_visible, FeatureResolve::Native);
    assert_eq!(reasoning.context_limit, 1_000_000);
    assert_eq!(classic.thinking_visible, FeatureResolve::Unsupported);
    assert_eq!(classic.context_limit, 128_000);
}

#[test]
fn openai_http_errors_are_typed_without_leaking_bodies() {
    let body = br#"{"error":{"type":"invalid_request_error","code":"rate_limit_exceeded","message":"secret provider detail"}}"#;
    let error = replay_openai_http_error(429, Some("2"), body);

    assert_eq!(error.kind, ProviderErrorKind::RateLimited);
    assert!(error.retryable);
    assert_eq!(error.retry_after_ms, Some(2_000));
    assert!(!error.message.contains("secret provider detail"));

    let failed = br#"event: response.failed
data: {"type":"response.failed","response":{"id":"resp_failed","status":"failed","error":{"code":"server_error","message":"private failure detail"}}}

"#;
    let stream_error = replay_openai_responses_sse(failed)
        .into_iter()
        .next()
        .expect("one failed item")
        .expect_err("response.failed is an error");
    assert_eq!(stream_error.kind, ProviderErrorKind::Transport);
    assert!(stream_error.retryable);
    assert!(!stream_error.message.contains("private failure detail"));
}

/// MUTATION CHECK: remove the `overloaded_error` stream mapping.
/// Expected failure: `InvalidRequest` replaces retryable `Overloaded`.
/// Verified by revert on 2026-07-29.
#[test]
fn streamed_overloaded_error_is_retryable_overload() {
    let wire =
        fs::read(fixture_directory().join("overloaded_responses.sse")).expect("overload fixture");
    let error = replay_openai_responses_sse(&wire)
        .into_iter()
        .next()
        .expect("overload item")
        .expect_err("overload is an error");

    assert_eq!(error.kind, ProviderErrorKind::Overloaded);
    assert!(error.retryable);
    assert!(!error.message.contains("private overload detail"));
}

fn assert_no_tool_events(items: &[ProviderStreamItem], call_id: &str) {
    assert!(
        !items.iter().any(|item| matches!(
            item,
            Ok(StreamEvent::ToolCallStart {
                call_id: actual,
                ..
            } | StreamEvent::ToolCallArgsDelta {
                call_id: actual,
                ..
            } | StreamEvent::ToolCallEnd {
                call_id: actual,
            }) if actual == call_id
        )),
        "partial tool call `{call_id}` crossed the adapter boundary"
    );
}

#[test]
fn provider_debug_never_exposes_openai_secrets() {
    let secret = "never-log-openai-key";
    let provider = provider_with_secret(secret, "gpt-5-test");

    let debug = format!("{provider:?}");

    assert!(!debug.contains(secret));
    assert!(debug.contains("[REDACTED]"));
}

fn native_provider(model: &str) -> OpenAiProvider {
    provider_with_secret("fixture-secret", model)
}

fn provider_with_secret(secret: &str, model: &str) -> OpenAiProvider {
    let vault = MemoryVault::new();
    let alias = CredentialAlias::new("openai-fixture");
    vault
        .put(&alias, secret.as_bytes())
        .expect("stores fixture secret");
    let handle = vault.resolve(&alias).expect("resolves fixture secret");
    OpenAiProvider::new(handle, model)
        .expect("HTTP client")
        .with_account(alias)
}

fn compatible_provider(model: &str, base_url: impl AsRef<str>) -> OpenAiCompatibleProvider {
    compatible_provider_result(model, base_url).expect("HTTP client")
}

fn compatible_provider_result(
    model: &str,
    base_url: impl AsRef<str>,
) -> Result<OpenAiCompatibleProvider, ProviderError> {
    let vault = MemoryVault::new();
    let alias = CredentialAlias::new("compatible-fixture");
    vault
        .put(&alias, b"fixture-secret")
        .expect("stores fixture secret");
    let handle = vault.resolve(&alias).expect("resolves fixture secret");
    OpenAiCompatibleProvider::new(handle, model, base_url)
        .map(|provider| provider.with_account(alias))
}

fn fixture_directory() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE_DIR)
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> T {
    let bytes = fs::read(path).expect("reads JSON fixture");
    serde_json::from_slice(&bytes).expect("parses JSON fixture")
}

/// MUTATION CHECK: drop the overload-prose fallback from the stream and
/// HTTP classifiers. Expected RUNTIME failure: the assertions below —
/// the codex backend's overload rides codes the typed arms don't know,
/// and misclassifying it as InvalidRequest turns a RETRYABLE condition
/// into a dead run (nine journaled failures before daemon.log named it).
#[test]
fn unknown_coded_overload_prose_classifies_retryable_overloaded() {
    let body = br#"{"error":{"message":"Our servers are currently overloaded. Please try again later.","type":"upstream_saturated","code":"chatgpt_overload"}}"#;
    let error = replay_openai_http_error(400, None, body);
    assert_eq!(error.kind, ProviderErrorKind::Overloaded);
    assert!(error.retryable);

    let stream = replay_openai_responses_sse(
        br#"event: response.failed
data: {"type":"response.failed","response":{"error":{"message":"Our servers are currently overloaded. Please try again later.","code":"chatgpt_overload"}}}

"#,
    );
    let failure = stream
        .into_iter()
        .find_map(|item| item.err())
        .expect("stream error item");
    assert_eq!(failure.kind, ProviderErrorKind::Overloaded);
    assert!(failure.retryable);
}
