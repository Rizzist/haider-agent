#![allow(clippy::expect_used)]

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use haider_accounts::{CredentialAlias, MemoryVault, Vault};
use haider_protocol::provider::{Block, FeatureResolve, FinishReason, StreamEvent};
use haider_provider::{
    Message, MessageRole, OpenAiCompatibleProvider, OpenAiProvider, OpenAiRetryPolicy, Provider,
    ProviderError, ProviderErrorKind, ProviderStreamItem, ToolDefinition, TurnRequest,
    replay_deepseek_chat_sse, replay_openai_chat_sse, replay_openai_http_error,
    replay_openai_models_response, replay_openai_responses_sse,
};
use serde::Deserialize;

use support::{ExpectedItem, read_json, reanchor_events};

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
        let actual = match fixture.family.as_str() {
            "responses" => replay_openai_responses_sse(&wire),
            "chat_completions" => replay_openai_chat_sse(&wire),
            "deepseek_chat_completions" => replay_deepseek_chat_sse(&wire),
            other => panic!("unknown OpenAI fixture family `{other}`"),
        };
        reanchor_events(&directory.join(&fixture.golden), &actual);
        let expected: Vec<ExpectedItem> = read_json(&directory.join(&fixture.golden));
        let expected = expected
            .into_iter()
            .map(ExpectedItem::into_result)
            .collect::<Vec<_>>();
        assert_eq!(actual, expected, "fixture `{}`", fixture.name);
    }
}

/// LAW (LK4 — missing `[DONE]` sentinel): a compatible Chat stream that ends
/// on EOF after delivering `finish_reason` completes cleanly — generic OSS
/// servers (research §generic) may never send the OpenAI `[DONE]` line.
///
/// MUTATION CHECK: require the sentinel (error from `finish` when a
/// finish_reason is already stored). Expected RUNTIME failure: the clean
/// Finish below becomes a MalformedFrame error.
#[test]
fn lk4_chat_stream_missing_done_sentinel_completes_on_eof() {
    let wire = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    );
    assert_eq!(
        replay_openai_chat_sse(wire.as_bytes()),
        vec![
            Ok(StreamEvent::TextDelta { text: "hi".into() }),
            Ok(StreamEvent::Finish {
                reason: FinishReason::EndTurn,
            }),
        ]
    );

    // A stream truncated BEFORE any finish_reason is a retryable stream
    // interruption. Core only retries it when no content was committed.
    let torn = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n";
    let items = replay_openai_chat_sse(torn.as_bytes());
    assert!(
        matches!(items.last(), Some(Err(error)) if error.kind == ProviderErrorKind::StreamInterrupted && error.retryable),
        "EOF before finish_reason is a retryable interrupted stream"
    );
}

/// LAW (LK5 — SSE comment/ping lines): `: ...` comment lines (llama.cpp
/// keep-alives) are protocol chrome, not data — the decoder skips them
/// without disturbing the event stream.
///
/// MUTATION CHECK: treat comment lines as data (drop the `:` skip in
/// `SseFramer::accept_line`). Expected RUNTIME failure: the ping below is
/// parsed as JSON and the clean sequence becomes an error.
#[test]
fn lk5_chat_stream_ignores_sse_comment_ping_lines() {
    let wire = concat!(
        ": ping - 2026-08-05 12:00:00\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"a\"},\"finish_reason\":null}]}\n\n",
        ": keep-alive\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"b\"},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    assert_eq!(
        replay_openai_chat_sse(wire.as_bytes()),
        vec![
            Ok(StreamEvent::TextDelta { text: "a".into() }),
            Ok(StreamEvent::TextDelta { text: "b".into() }),
            Ok(StreamEvent::Finish {
                reason: FinishReason::EndTurn,
            }),
        ]
    );
}

/// LAW (LK6 — absent stream usage): a compatible stream that never carries a
/// `usage` object (servers that reject or ignore `stream_options`) completes
/// with NO UsageUpdate and no error — missing usage is normal, never fatal.
/// The request keeps sending `stream_options.include_usage` (pinned by
/// `compatible_payload_uses_chat_completions_lingua_franca`).
///
/// MUTATION CHECK: require usage before Finish in the Chat decoder. Expected
/// RUNTIME failure: the clean sequence below gains an error item.
#[test]
fn lk6_chat_stream_without_usage_still_completes() {
    let wire = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    let items = replay_openai_chat_sse(wire.as_bytes());
    assert!(
        !items
            .iter()
            .any(|item| matches!(item, Ok(StreamEvent::UsageUpdate(_)) | Err(_))),
        "no usage and no error: {items:?}"
    );
    assert_eq!(
        items.last(),
        Some(&Ok(StreamEvent::Finish {
            reason: FinishReason::EndTurn,
        }))
    );
}

/// LAW (LK7 — absent tool-call ids): vLLM/llama.cpp may stream tool-call
/// deltas without ids. The decoder mints a STABLE per-index id
/// (`tool-call-{index}`), keeps later deltas on the same index correlated to
/// it, and distinct indexes get distinct ids.
///
/// MUTATION CHECK: restore the "started without an id" rejection. Expected
/// RUNTIME failure: the whole sequence below becomes one error item.
#[test]
fn lk7_chat_tool_calls_without_ids_synthesize_stable_per_index_ids() {
    let wire = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"name\":\"get_weather\",\"arguments\":\"{\\\"city\\\":\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"Tehran\\\"}\"}},{\"index\":1,\"function\":{\"name\":\"get_time\",\"arguments\":\"{}\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    assert_eq!(
        replay_openai_chat_sse(wire.as_bytes()),
        vec![
            Ok(StreamEvent::ToolCallStart {
                call_id: "tool-call-0".into(),
                name: "get_weather".into(),
            }),
            Ok(StreamEvent::ToolCallArgsDelta {
                call_id: "tool-call-0".into(),
                args_fragment: "{\"city\":".into(),
            }),
            Ok(StreamEvent::ToolCallArgsDelta {
                call_id: "tool-call-0".into(),
                args_fragment: "\"Tehran\"}".into(),
            }),
            Ok(StreamEvent::ToolCallStart {
                call_id: "tool-call-1".into(),
                name: "get_time".into(),
            }),
            Ok(StreamEvent::ToolCallArgsDelta {
                call_id: "tool-call-1".into(),
                args_fragment: "{}".into(),
            }),
            Ok(StreamEvent::ToolCallEnd {
                call_id: "tool-call-0".into(),
            }),
            Ok(StreamEvent::ToolCallEnd {
                call_id: "tool-call-1".into(),
            }),
            Ok(StreamEvent::Finish {
                reason: FinishReason::ToolUse,
            }),
        ]
    );
}

/// LAW (LK8 — finish_reason "stop" with tool calls present): a server that
/// streams tool-call deltas but closes with `"stop"` still gets its calls
/// COMPLETED — ToolCallEnd for every open call and Finish{ToolUse}, never a
/// silently dropped invocation. Non-tool closes (max tokens) keep dropping
/// partials (pinned by `chat_max_tokens_drops_partial_tool_call_*`).
///
/// MUTATION CHECK: remove the EndTurn→ToolUse upgrade in `finish_events`.
/// Expected RUNTIME failure: the sequence below loses its tool events and
/// finishes EndTurn.
#[test]
fn lk8_chat_finish_stop_with_tool_calls_still_completes_the_calls() {
    let wire = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_native\",\"function\":{\"name\":\"list_files\",\"arguments\":\"{}\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    assert_eq!(
        replay_openai_chat_sse(wire.as_bytes()),
        vec![
            Ok(StreamEvent::ToolCallStart {
                call_id: "call_native".into(),
                name: "list_files".into(),
            }),
            Ok(StreamEvent::ToolCallArgsDelta {
                call_id: "call_native".into(),
                args_fragment: "{}".into(),
            }),
            Ok(StreamEvent::ToolCallEnd {
                call_id: "call_native".into(),
            }),
            Ok(StreamEvent::Finish {
                reason: FinishReason::ToolUse,
            }),
        ]
    );

    // The EOF path (no [DONE]) upgrades identically.
    let eof_wire = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_native\",\"function\":{\"name\":\"list_files\",\"arguments\":\"{}\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    );
    assert_eq!(
        replay_openai_chat_sse(eof_wire.as_bytes()).last(),
        Some(&Ok(StreamEvent::Finish {
            reason: FinishReason::ToolUse,
        }))
    );
}

/// LAW (LK9 — unknown extra fields are ignored): OSS servers decorate chunks
/// with fields the OpenAI schema never named (`timings`, `system_fingerprint`,
/// vendor objects at every level). Deserialization is field-tolerant: the
/// known events decode and the junk is invisible.
///
/// MUTATION CHECK: reject unknown chunk fields in the Chat decoder dispatch.
/// Expected RUNTIME failure: the decorated stream below errors instead of
/// producing the clean three-event sequence.
#[test]
fn lk9_chat_unknown_extra_fields_are_ignored() {
    let wire = concat!(
        "data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"system_fingerprint\":\"b1\",\"timings\":{\"prompt_ms\":12.5},\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\",\"vendor_extension\":{\"nested\":true}},\"logprobs\":null,\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"thinking\"},\"finish_reason\":null}],\"extra_top_level\":[1,2,3]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"timings\":{\"predicted_ms\":99.0}}\n\n",
        "data: [DONE]\n\n",
    );
    assert_eq!(
        replay_openai_chat_sse(wire.as_bytes()),
        vec![
            Ok(StreamEvent::TextDelta { text: "ok".into() }),
            Ok(StreamEvent::ReasoningDelta {
                text: "thinking".into(),
            }),
            Ok(StreamEvent::Finish {
                reason: FinishReason::EndTurn,
            }),
        ]
    );
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
        cache_metadata: None,
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
        cache_metadata: None,
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

/// LAW (LK3 — literal half of the origin matrix, G4a): the CUSTOM-provenance
/// `new_custom` constructor accepts RFC1918 LAN literals over http AND https
/// (the deliberate, scoped loosening for local OSS servers), while
/// link-local `169.254.0.0/16` (cloud metadata), multicast, unspecified,
/// IPv6 ULA/link-local, and PUBLIC plain-HTTP origins stay refused — and the
/// builtin `new` constructor still refuses every private literal, so the
/// loosening never leaks into release-owned providers.
///
/// MUTATION CHECK (wrongly-blocked): route `new_custom` through the Strict
/// policy. Expected RUNTIME failure: the allowed LAN rows below error.
/// MUTATION CHECK (wrongly-allowed): drop the link-local block under
/// TrustedLan. Expected RUNTIME failure: the 169.254.169.254 rows construct.
#[test]
fn lk3_custom_origin_matrix_allows_rfc1918_and_keeps_metadata_and_public_http_blocked() {
    // ALLOWED under TrustedLan: loopback, all three RFC1918 ranges (http and
    // https), their IPv4-mapped IPv6 forms, and ordinary public HTTPS.
    let allowed = [
        "http://127.0.0.1:11434",
        "http://192.168.1.8:11434",
        "https://192.168.1.8:11434",
        "http://10.0.0.8:8080",
        "https://10.0.0.8",
        "http://172.16.0.8:1234",
        "https://172.31.255.254",
        "http://[::ffff:192.168.1.8]:11434",
        "https://api.example.com/openai",
    ];
    for base_url in allowed {
        custom_provider_result("test-model", base_url)
            .unwrap_or_else(|error| panic!("custom LAN origin `{base_url}` refused: {error}"));
    }

    // REFUSED under TrustedLan: the loosening is EXACTLY RFC1918.
    let refused = [
        "http://169.254.169.254",
        "https://169.254.169.254",
        "https://[::ffff:169.254.169.254]",
        "http://203.0.113.7",
        "http://224.0.0.1",
        "https://224.0.0.1",
        "https://0.0.0.0",
        "https://[fe80::1]",
        "https://[fc00::1]",
    ];
    for base_url in refused {
        let error = custom_provider_result("test-model", base_url)
            .expect_err("origin must stay refused under TrustedLan");
        assert_eq!(error.kind, ProviderErrorKind::InvalidRequest, "{base_url}");
        assert!(!error.retryable, "{base_url}");
    }

    // BUILTIN PINNED: the release-owned constructor still refuses RFC1918
    // entirely — the LAN policy is custom-provenance-only.
    for base_url in ["http://192.168.1.8:11434", "https://10.0.0.8"] {
        let error = compatible_provider_result("test-model", base_url)
            .expect_err("builtin strict constructor must still refuse private origins");
        assert_eq!(error.kind, ProviderErrorKind::InvalidRequest, "{base_url}");
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
        cache_metadata: None,
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

/// LAW E1c. MUTATION: let HTTP 429 win before the billing-code classifier;
/// this becomes RateLimited and the assertion fails.
#[test]
fn e1c_openai_insufficient_quota_is_non_retryable_quota_exhausted() {
    let body = br#"{"error":{"type":"insufficient_quota","code":"insufficient_quota","message":"private billing detail"}}"#;
    let error = replay_openai_http_error(429, Some("1"), body);
    assert_eq!(error.kind, ProviderErrorKind::QuotaExhausted);
    assert!(!error.retryable);
    assert_eq!(error.retry_after_ms, None);
    assert!(error.message.contains("retrying will not help"));
    assert!(!error.message.contains("private billing detail"));
}

#[test]
fn e1e_openai_empty_stream_is_retryable_stream_interruption() {
    let items = replay_openai_responses_sse(b"");
    assert!(matches!(
        items.as_slice(),
        [Err(error)] if error.kind == ProviderErrorKind::StreamInterrupted && error.retryable
    ));
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

fn custom_provider_result(
    model: &str,
    base_url: impl AsRef<str>,
) -> Result<OpenAiCompatibleProvider, ProviderError> {
    let vault = MemoryVault::new();
    let alias = CredentialAlias::new("custom-lan-fixture");
    vault
        .put(&alias, b"fixture-secret")
        .expect("stores fixture secret");
    let handle = vault.resolve(&alias).expect("resolves fixture secret");
    OpenAiCompatibleProvider::new_custom(handle, model, base_url)
        .map(|provider| provider.with_account(alias))
}

fn fixture_directory() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE_DIR)
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

/// LAW (LW2 openai half / decision 2): a finished hosted `web_search_call`
/// item is captured VERBATIM through the opaque channel (the reasoning-item
/// echo path), surfaces as one closed display row — and NEVER as a client
/// tool call, so the turn still finishes EndTurn, not ToolUse. url_citation
/// annotations on the finished message item decode into deduped display
/// sources; a failed call surfaces as a failed row.
#[test]
fn hosted_web_search_call_captures_verbatim_and_citations_surface_as_sources() {
    let search_item = serde_json::json!({
        "type": "web_search_call",
        "id": "ws_abc123",
        "status": "completed",
        "action": {"type": "search", "query": "rust sse decoding"},
    });
    let message_item = serde_json::json!({
        "type": "message",
        "id": "msg_1",
        "role": "assistant",
        "content": [{
            "type": "output_text",
            "text": "cited",
            "annotations": [
                {"type": "url_citation", "url": "https://example.com/a", "title": "A", "start_index": 0, "end_index": 5},
                {"type": "url_citation", "url": "https://example.com/a", "title": "A duplicate"},
                {"type": "file_citation", "file_id": "file_1"},
            ],
        }],
    });
    let stream = format!(
        "event: response.output_item.done\n\
         data: {}\n\n\
         event: response.output_text.delta\n\
         data: {}\n\n\
         event: response.output_item.done\n\
         data: {}\n\n\
         event: response.completed\n\
         data: {}\n\n",
        serde_json::json!({"type": "response.output_item.done", "output_index": 0, "item": search_item}),
        serde_json::json!({"type": "response.output_text.delta", "delta": "cited"}),
        serde_json::json!({"type": "response.output_item.done", "output_index": 1, "item": message_item}),
        serde_json::json!({"type": "response.completed", "response": {"id": "resp_1"}}),
    );
    assert_eq!(
        replay_openai_responses_sse(stream.as_bytes()),
        vec![
            Ok(StreamEvent::ProviderOpaque {
                provider: "openai".into(),
                data: search_item,
            }),
            Ok(StreamEvent::ServerToolUse {
                call_id: "ws_abc123".into(),
                name: "web_search".into(),
                args: serde_json::json!({"type": "search", "query": "rust sse decoding"}),
            }),
            Ok(StreamEvent::ServerToolResult {
                call_id: "ws_abc123".into(),
                preview: "search".into(),
                is_error: false,
            }),
            Ok(StreamEvent::TextDelta {
                text: "cited".into(),
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
        "hosted search captures verbatim, rows close, citations dedup, finish stays EndTurn"
    );

    // A failed call surfaces as a failed row — still no client tool call.
    let failed_item = serde_json::json!({
        "type": "web_search_call",
        "id": "ws_failed",
        "status": "failed",
    });
    let stream = format!(
        "event: response.output_item.done\n\
         data: {}\n\n\
         event: response.completed\n\
         data: {}\n\n",
        serde_json::json!({"type": "response.output_item.done", "output_index": 0, "item": failed_item}),
        serde_json::json!({"type": "response.completed", "response": {"id": "resp_2"}}),
    );
    let items = replay_openai_responses_sse(stream.as_bytes());
    assert!(
        items.contains(&Ok(StreamEvent::ServerToolResult {
            call_id: "ws_failed".into(),
            preview: "failed".into(),
            is_error: true,
        })),
        "a failed hosted call is an honest failed row: {items:?}"
    );
    assert!(
        items.contains(&Ok(StreamEvent::Finish {
            reason: FinishReason::EndTurn,
        })),
        "hosted calls never flip the finish to ToolUse: {items:?}"
    );
}
